//! 连接池的连接管理器：为 bb8 按需建立并探活 tiberius 连接。
//!
//! 之所以不直接用 `bb8-tiberius`：它锁死 tiberius 0.12，而 0.12 在解析
//! `sql_variant`/`geography`/`hierarchyid` 等 UDT 列的元数据时会 `todo!()`
//! panic，查询含这类列的语句会永久挂住。tiberius 0.13 已实现 UDT 解码，
//! 因此这里自行实现 [`bb8::ManageConnection`]，行为对齐原先的
//! `bb8-tiberius`：`set_nodelay(true)`、跟随一次服务器重定向、`SELECT 1` 探活。

use std::fmt;
use std::io;

use bb8::ManageConnection;
use tiberius::Client;
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncReadCompatExt};

/// 池化的连接类型：tiberius 客户端 + tokio TCP 流的 compat 包装。
pub type PooledClient = Client<Compat<TcpStream>>;

/// 连接建立或探活失败的原因。
///
/// bb8 要求 `ManageConnection::Error: std::error::Error`，而 `anyhow::Error`
/// 不满足该约束，因此这里用具体的两变体枚举而非 `anyhow::Error`。
#[derive(Debug)]
pub enum ConnError {
    /// tiberius 协议/驱动层错误。
    Tiberius(tiberius::error::Error),
    /// 网络层错误。
    Io(io::Error),
}

impl fmt::Display for ConnError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Tiberius(e) => write!(f, "{e}"),
            Self::Io(e) => write!(f, "{e}"),
        }
    }
}

impl std::error::Error for ConnError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Tiberius(e) => Some(e),
            Self::Io(e) => Some(e),
        }
    }
}

impl From<tiberius::error::Error> for ConnError {
    fn from(e: tiberius::error::Error) -> Self {
        Self::Tiberius(e)
    }
}

impl From<io::Error> for ConnError {
    fn from(e: io::Error) -> Self {
        Self::Io(e)
    }
}

/// bb8 的连接管理器：按需建立 tiberius 连接并探活。
pub struct ConnectionManager {
    config: tiberius::Config,
}

impl ConnectionManager {
    /// 用给定的 tiberius 配置创建管理器。
    pub fn new(config: tiberius::Config) -> Self {
        Self { config }
    }

    /// 建立一条连接；服务器要求重定向时跟随一次（SQL Server 只会重定向一次）。
    async fn connect_inner(&self) -> Result<PooledClient, ConnError> {
        let tcp = self.connect_tcp().await?;
        let client = match Client::connect(self.config.clone(), tcp).await {
            Ok(client) => client,
            Err(tiberius::error::Error::Routing { host, port }) => {
                let mut config = self.config.clone();
                config.host(&host);
                config.port(port);
                let tcp = self.connect_tcp_to(config.get_addr()).await?;
                Client::connect(config, tcp).await?
            }
            Err(e) => return Err(e.into()),
        };
        Ok(client)
    }

    async fn connect_tcp(&self) -> Result<Compat<TcpStream>, ConnError> {
        self.connect_tcp_to(self.config.get_addr()).await
    }

    async fn connect_tcp_to(&self, addr: String) -> Result<Compat<TcpStream>, ConnError> {
        let tcp = TcpStream::connect(addr).await?;
        // 与 bb8-tiberius 一致：禁用 Nagle，避免小查询被缓冲拖延。
        tcp.set_nodelay(true)?;
        Ok(tcp.compat())
    }
}

impl ManageConnection for ConnectionManager {
    type Connection = PooledClient;
    type Error = ConnError;

    async fn connect(&self) -> Result<Self::Connection, Self::Error> {
        self.connect_inner().await
    }

    async fn is_valid(&self, conn: &mut Self::Connection) -> Result<(), Self::Error> {
        conn.simple_query("SELECT 1").await?;
        Ok(())
    }

    fn has_broken(&self, _conn: &mut Self::Connection) -> bool {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn conn_error_wraps_tiberius_error() {
        let err: ConnError = tiberius::error::Error::Protocol("boom".into()).into();
        assert!(matches!(err, ConnError::Tiberius(_)));
        assert!(err.to_string().contains("boom"));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn conn_error_wraps_io_error() {
        let io = io::Error::new(io::ErrorKind::ConnectionRefused, "refused");
        let err: ConnError = io.into();
        assert!(matches!(err, ConnError::Io(_)));
        assert!(err.to_string().contains("refused"));
        assert!(std::error::Error::source(&err).is_some());
    }

    #[test]
    fn manager_stores_config_for_lazy_connect() {
        // 建管理器不应发起连接（惰性建连是服务器能在数据库不可达时启动的前提）。
        let mut config = tiberius::Config::new();
        config.host("nonexistent.invalid");
        config.port(1433);
        let manager = ConnectionManager::new(config);
        assert_eq!(manager.config.get_addr(), "nonexistent.invalid:1433");
    }
}
