//! mssql_mcp_server-rs 的可执行入口：通过 stdio 或 HTTP 提供 JSON-RPC。
//!
//! MCP 工具：`execute_sql`、`describe_table`、`table_sizes`、`list_databases`；
//! 资源：`mssql://{table}/data`。

use mssql_mcp_server_rs::config;
use mssql_mcp_server_rs::server::McpServer;
use rmcp::ServiceExt;

fn init_logging() {
    // MCP stdio 传输要求 stdout 只能承载协议消息，日志全部输出到 stderr。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();
}

/// 传输方式：`stdio`（默认）或启用 `http` feature 后的 `http`。
enum Transport {
    Stdio,
    #[cfg(feature = "http")]
    Http {
        addr: std::net::SocketAddr,
        bearer_token: Option<String>,
        /// 显式放行的 Host 头（DNS rebinding 防护白名单），见 [`crate::http`]。
        allowed_hosts: Vec<String>,
    },
}

/// 从环境变量读取传输配置。
fn transport_from_env() -> anyhow::Result<Transport> {
    let raw = std::env::var("MSSQL_TRANSPORT")
        .ok()
        .map(|v| v.trim().to_ascii_lowercase());
    match raw.as_deref() {
        None | Some("") | Some("stdio") => Ok(Transport::Stdio),
        #[cfg(feature = "http")]
        Some(v) if v.eq_ignore_ascii_case("http") => {
            let lookup = |name: &str| std::env::var(name).ok();
            Ok(Transport::Http {
                addr: mssql_mcp_server_rs::http::http_addr_from_env(&lookup)?,
                bearer_token: std::env::var("MSSQL_HTTP_BEARER_TOKEN")
                    .ok()
                    .map(|v| v.trim().to_string())
                    .filter(|v| !v.is_empty()),
                allowed_hosts: mssql_mcp_server_rs::http::http_allowed_hosts_from_env(&lookup),
            })
        }
        Some(raw) => {
            let supported = if cfg!(feature = "http") {
                "'stdio' or 'http'"
            } else {
                "'stdio' (rebuild with the 'http' feature to enable HTTP)"
            };
            anyhow::bail!("Invalid MSSQL_TRANSPORT value: {raw}. Expected {supported}")
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    log::info!("Starting MSSQL MCP server...");

    let db_config = match config::DbConfig::from_env() {
        Ok(config) => config,
        Err(e) => {
            log::error!("{e}");
            return Err(e);
        }
    };
    log::info!(
        "Database config: {}:{}/{} as {} (encrypt: {}, azure: {})",
        db_config.server,
        db_config.port,
        db_config.database,
        db_config.auth.describe(),
        db_config.encrypt,
        db_config.is_azure()
    );

    match transport_from_env()? {
        Transport::Stdio => {
            let service = McpServer::new(db_config)
                .serve(rmcp::transport::stdio())
                .await?;
            service.waiting().await?;
        }
        #[cfg(feature = "http")]
        Transport::Http {
            addr,
            bearer_token,
            allowed_hosts,
        } => {
            mssql_mcp_server_rs::http::serve_http(db_config, addr, bearer_token, allowed_hosts)
                .await?;
        }
    }
    Ok(())
}
