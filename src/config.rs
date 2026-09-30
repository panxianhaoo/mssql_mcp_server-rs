//! 数据库配置：从环境变量读取连接信息。

use std::env;

const DEFAULT_SERVER: &str = "localhost";
const DEFAULT_PORT: u16 = 1433;
const AZURE_DOMAIN_MARKER: &str = ".database.windows.net";

/// MSSQL 连接配置（由环境变量构建）。
#[derive(Debug, Clone, PartialEq)]
pub struct DbConfig {
    pub server: String,
    pub port: u16,
    pub database: String,
    pub user: String,
    pub password: String,
    pub encrypt: bool,
    pub trust_server_certificate: bool,
}

impl DbConfig {
    /// 判断是否连接 Azure SQL Database（Azure 强制加密且必须校验证书）。
    pub fn is_azure(&self) -> bool {
        self.server
            .to_ascii_lowercase()
            .contains(AZURE_DOMAIN_MARKER)
    }

    /// 从环境变量构建配置，缺失必填项或端口非法时返回带明确提示的错误。
    pub fn from_env() -> Result<Self, String> {
        let server = env::var("MSSQL_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string());
        let database = required("MSSQL_DATABASE")?;
        let user = required("MSSQL_USER")?;
        let password = required("MSSQL_PASSWORD")?;
        let port = parse_port(env::var("MSSQL_PORT").ok().as_deref())?;
        let encrypt = parse_bool(env::var("MSSQL_ENCRYPT").ok().as_deref());
        Self::build(server, port, database, user, password, encrypt)
    }

    /// 纯函数构建：Azure 连接强制加密且不信任自签证书。
    fn build(
        server: String,
        port: u16,
        database: String,
        user: String,
        password: String,
        encrypt_env: bool,
    ) -> Result<Self, String> {
        let is_azure = server.to_ascii_lowercase().contains(AZURE_DOMAIN_MARKER);
        let encrypt = is_azure || encrypt_env;
        // Azure 必须校验证书；本地/自建实例为方便自签证书场景默认信任。
        let trust_server_certificate = !is_azure;
        Ok(Self {
            server,
            port,
            database,
            user,
            password,
            encrypt,
            trust_server_certificate,
        })
    }
}

fn required(name: &str) -> Result<String, String> {
    env::var(name).map_err(|_| {
        format!(
            "Missing required database configuration: {name}. \
             MSSQL_USER, MSSQL_PASSWORD, and MSSQL_DATABASE are required"
        )
    })
}

fn parse_port(value: Option<&str>) -> Result<u16, String> {
    match value {
        None => Ok(DEFAULT_PORT),
        Some(raw) if raw.trim().is_empty() => Ok(DEFAULT_PORT),
        Some(raw) => raw
            .trim()
            .parse::<u16>()
            .map_err(|_| format!("Invalid MSSQL_PORT value: {raw}")),
    }
}

fn parse_bool(value: Option<&str>) -> bool {
    matches!(value, Some(v) if v.eq_ignore_ascii_case("true"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_config(server: &str, encrypt_env: bool) -> DbConfig {
        DbConfig::build(
            server.to_string(),
            1433,
            "testdb".to_string(),
            "sa".to_string(),
            "pass".to_string(),
            encrypt_env,
        )
        .unwrap()
    }

    #[test]
    fn build_defaults_to_unencrypted_for_local_server() {
        let config = build_config("localhost", false);
        assert!(!config.encrypt);
        assert!(config.trust_server_certificate);
        assert!(!config.is_azure());
    }

    #[test]
    fn build_respects_encrypt_env_for_local_server() {
        let config = build_config("localhost", true);
        assert!(config.encrypt);
        assert!(config.trust_server_certificate);
    }

    #[test]
    fn build_forces_encryption_on_azure() {
        let config = build_config("myserver.database.windows.net", false);
        assert!(config.is_azure());
        assert!(config.encrypt);
        assert!(!config.trust_server_certificate);
    }

    #[test]
    fn build_detects_azure_case_insensitively() {
        let config = build_config("MyServer.Database.Windows.NET", false);
        assert!(config.is_azure());
    }

    #[test]
    fn parse_port_accepts_valid_and_defaults() {
        assert_eq!(parse_port(None).unwrap(), 1433);
        assert_eq!(parse_port(Some("")).unwrap(), 1433);
        assert_eq!(parse_port(Some("1434")).unwrap(), 1434);
        assert!(parse_port(Some("not-a-port")).is_err());
        assert!(parse_port(Some("99999")).is_err());
    }

    #[test]
    fn parse_bool_only_accepts_true() {
        assert!(parse_bool(Some("true")));
        assert!(parse_bool(Some("TRUE")));
        assert!(!parse_bool(Some("false")));
        assert!(!parse_bool(None));
    }
}
