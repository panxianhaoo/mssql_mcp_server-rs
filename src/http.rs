//! 可选的 HTTP 传输（streamable-http），由 `http` feature 启用。
//!
//! 默认只提供 stdio（适合 Claude Code 等本地起子进程客户端）；启用后可用
//! `MSSQL_TRANSPORT=http` 起一个 HTTP 服务，供远程/多客户端共享同一实例。
//!
//! 安全默认：
//! - rmcp 的 `StreamableHttpServerConfig` 默认**只接受 loopback 的 Host 头**，
//!   用于阻断 DNS rebinding；这里保留该默认值，并额外要求显式设置
//!   `MSSQL_HTTP_BEARER_TOKEN` 才监听非 loopback 地址，避免用户无意间把
//!   数据库暴露到局域网。
//! - 绑定地址默认 `127.0.0.1`，公网部署必须由使用者显式改绑并配好 TLS 反代。

use std::net::{IpAddr, Ipv4Addr, SocketAddr};

use anyhow::{Context, Result, bail};
use axum::response::IntoResponse;
use rmcp::transport::streamable_http_server::session::local::LocalSessionManager;
use rmcp::transport::streamable_http_server::tower::{
    StreamableHttpServerConfig, StreamableHttpService,
};

use crate::config::DbConfig;
use crate::server::McpServer;

/// HTTP 监听地址的默认值：仅 loopback，避免无意间对外暴露数据库。
const DEFAULT_HTTP_ADDR: SocketAddr = SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8000);

/// 通过 streamable-http 提供 MCP 服务（阻塞直到进程结束）。
pub async fn serve_http(
    config: DbConfig,
    addr: SocketAddr,
    bearer_token: Option<String>,
) -> Result<()> {
    if !addr.ip().is_loopback() && bearer_token.is_none() {
        bail!(
            "refusing to bind {addr} without MSSQL_HTTP_BEARER_TOKEN: \
             a non-loopback bind would expose the database to the network unauthenticated. \
             Either bind to 127.0.0.1 or set a bearer token."
        );
    }

    // 每个新 session 构造一个 McpServer：连接诃是 Clone（内部 Arc），
    // 因此多 session 共享同一组连接诃，不会各建一套。
    let service = StreamableHttpService::new(
        move || Ok(McpServer::new(config.clone())),
        std::sync::Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("failed to bind HTTP listener on {addr}"))?;
    log::info!("MCP streamable-http server listening on http://{addr}/mcp");

    let app = axum::Router::new().nest_service("/mcp", service_with_auth(service, bearer_token));
    axum::serve(listener, app)
        .await
        .context("HTTP server failed")?;
    Ok(())
}

/// 用 bearer token 包裹 MCP service：未配置 token 时直接暴露（仅限 loopback）。
///
/// 这里只做最小可用的鉴权——读取 `Authorization: Bearer <token>`，
/// 不匹配返回 401。生产部署应前置带 TLS 的反向代理。
fn service_with_auth(
    service: StreamableHttpService<McpServer, LocalSessionManager>,
    token: Option<String>,
) -> axum::Router {
    match token {
        None => axum::Router::new().route_service("/", service),
        Some(token) => {
            axum::Router::new()
                .route_service("/", service)
                .layer(axum::middleware::from_fn(
                    move |req: axum::extract::Request, next: axum::middleware::Next| {
                        let expected = token.clone();
                        async move {
                            let authorized = req
                                .headers()
                                .get(axum::http::header::AUTHORIZATION)
                                .and_then(|v| v.to_str().ok())
                                .and_then(|v| v.strip_prefix("Bearer "))
                                .is_some_and(|present| constant_time_eq(present, &expected));
                            if authorized {
                                next.run(req).await
                            } else {
                                axum::http::StatusCode::UNAUTHORIZED.into_response()
                            }
                        }
                    },
                ))
        }
    }
}

/// 常量时间字符串比较，避免通过响应时间侧信道逐字节猜测 token。
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// 从环境变量解析监听地址；未设置或非法时报错（不静默回落到默认值）。
pub fn http_addr_from_env(lookup: &dyn Fn(&str) -> Option<String>) -> Result<SocketAddr> {
    match lookup("MSSQL_HTTP_ADDR") {
        None => Ok(DEFAULT_HTTP_ADDR),
        Some(raw) if raw.trim().is_empty() => Ok(DEFAULT_HTTP_ADDR),
        Some(raw) => raw
            .trim()
            .parse::<SocketAddr>()
            .with_context(|| format!("Invalid MSSQL_HTTP_ADDR value: {raw}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let owned: Vec<(String, String)> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
            .collect();
        move |name| {
            owned
                .iter()
                .find(|(k, _)| k == name)
                .map(|(_, v)| v.clone())
        }
    }

    #[test]
    fn http_addr_defaults_to_loopback() {
        assert_eq!(
            http_addr_from_env(&lookup_from(&[])).unwrap(),
            DEFAULT_HTTP_ADDR
        );
        assert_eq!(
            http_addr_from_env(&lookup_from(&[("MSSQL_HTTP_ADDR", "")])).unwrap(),
            DEFAULT_HTTP_ADDR
        );
        assert!(DEFAULT_HTTP_ADDR.ip().is_loopback());
    }

    #[test]
    fn http_addr_parses_explicit_socket_addr() {
        let addr =
            http_addr_from_env(&lookup_from(&[("MSSQL_HTTP_ADDR", "0.0.0.0:9001")])).unwrap();
        assert_eq!(addr.port(), 9001);
        assert!(!addr.ip().is_loopback());
    }

    #[test]
    fn http_addr_rejects_malformed() {
        assert!(http_addr_from_env(&lookup_from(&[("MSSQL_HTTP_ADDR", "not-an-addr")])).is_err());
        assert!(http_addr_from_env(&lookup_from(&[("MSSQL_HTTP_ADDR", "9001")])).is_err());
    }

    #[test]
    fn constant_time_eq_compares_correctly() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "Secret"));
        assert!(!constant_time_eq("secret", "secre"));
        assert!(!constant_time_eq("", "a"));
        assert!(constant_time_eq("", ""));
    }
}
