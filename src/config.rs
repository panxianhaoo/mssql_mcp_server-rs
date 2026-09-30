//! 数据库配置：从环境变量读取连接信息。

use std::env;

use anyhow::{bail, Context, Result};

const DEFAULT_SERVER: &str = "localhost";
const DEFAULT_PORT: u16 = 1433;
const AZURE_DOMAIN_MARKER: &str = ".database.windows.net";

/// 实际使用的认证方式：SQL 登录或 Windows 集成认证（SSPI）。
#[derive(Debug, Clone, PartialEq)]
pub enum AuthKind {
    /// SQL Server 登录（用户名 + 密码）。
    SqlServer {
        user: String,
        password: String,
    },
    /// Windows 集成认证：使用当前登录用户身份，无需凭据（仅 Windows 平台存在此变体）。
    #[cfg(windows)]
    WindowsIntegrated,
}

impl AuthKind {
    /// 日志用的人类可读描述（绝不包含密码）。
    pub(crate) fn describe(&self) -> String {
        match self {
            Self::SqlServer { user, .. } => format!("{user} (SQL login)"),
            #[cfg(windows)]
            Self::WindowsIntegrated => "current Windows user (integrated auth)".to_string(),
        }
    }
}

/// MSSQL 连接配置（由环境变量构建）。
#[derive(Debug, Clone, PartialEq)]
pub struct DbConfig {
    pub server: String,
    pub port: u16,
    pub database: String,
    pub auth: AuthKind,
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

    /// 从环境变量构建配置，缺失必填项或取值非法时返回带明确提示的错误。
    /// Windows 集成认证（`MSSQL_AUTH=windows`）不需要用户名与密码。
    pub fn from_env() -> Result<Self> {
        let server = env::var("MSSQL_SERVER").unwrap_or_else(|_| DEFAULT_SERVER.to_string());
        let database = required("MSSQL_DATABASE")?;
        let mode = parse_auth_mode(env::var("MSSQL_AUTH").ok().as_deref())?;
        let auth = match mode {
            AuthMode::Sql => AuthKind::SqlServer {
                user: required("MSSQL_USER")?,
                password: required("MSSQL_PASSWORD")?,
            },
            #[cfg(windows)]
            AuthMode::Windows => AuthKind::WindowsIntegrated,
        };
        let port = parse_port(env::var("MSSQL_PORT").ok().as_deref())?;
        let encrypt = parse_bool(env::var("MSSQL_ENCRYPT").ok().as_deref());
        Ok(Self::build(server, port, database, auth, encrypt))
    }

    /// 纯函数构建：Azure 连接强制加密且不信任自签证书。
    fn build(
        server: String,
        port: u16,
        database: String,
        auth: AuthKind,
        encrypt_env: bool,
    ) -> Self {
        let is_azure = server.to_ascii_lowercase().contains(AZURE_DOMAIN_MARKER);
        let encrypt = is_azure || encrypt_env;
        // Azure 必须校验证书；本地/自建实例为方便自签证书场景默认信任。
        let trust_server_certificate = !is_azure;
        Self {
            server,
            port,
            database,
            auth,
            encrypt,
            trust_server_certificate,
        }
    }
}

fn required(name: &str) -> Result<String> {
    env::var(name).with_context(|| {
        format!(
            "Missing required database configuration: {name}. \
             MSSQL_USER, MSSQL_PASSWORD, and MSSQL_DATABASE are required"
        )
    })
}

fn parse_port(value: Option<&str>) -> Result<u16> {
    match value {
        None => Ok(DEFAULT_PORT),
        Some(raw) if raw.trim().is_empty() => Ok(DEFAULT_PORT),
        Some(raw) => raw
            .trim()
            .parse::<u16>()
            .with_context(|| format!("Invalid MSSQL_PORT value: {raw}")),
    }
}

fn parse_bool(value: Option<&str>) -> bool {
    matches!(value, Some(v) if v.eq_ignore_ascii_case("true"))
}

/// `MSSQL_AUTH` 的认证模式。
#[derive(Debug, Clone, Copy, PartialEq)]
enum AuthMode {
    /// SQL Server 登录（默认）。
    Sql,
    /// Windows 集成认证；仅 Windows 平台存在此变体。
    #[cfg(windows)]
    Windows,
}

/// 解析 `MSSQL_AUTH`：缺省/`sql` 为 SQL 登录，`windows`/`win`/`integrated`
/// 为 Windows 集成认证。
fn parse_auth_mode(value: Option<&str>) -> Result<AuthMode> {
    match value
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_ascii_lowercase)
        .as_deref()
    {
        None | Some("sql") => Ok(AuthMode::Sql),
        Some("windows" | "win" | "integrated") => windows_auth_mode(),
        Some(raw) => bail!("Invalid MSSQL_AUTH value: {raw}. Expected 'sql' or 'windows'"),
    }
}

/// Windows 平台：集成认证可用。
#[cfg(windows)]
fn windows_auth_mode() -> Result<AuthMode> {
    Ok(AuthMode::Windows)
}

/// 非 Windows 平台：启动即报错，避免留到连接阶段才失败。
#[cfg(not(windows))]
fn windows_auth_mode() -> Result<AuthMode> {
    bail!("MSSQL_AUTH=windows (Windows integrated auth) is only supported when running on Windows")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_config(server: &str, encrypt_env: bool) -> DbConfig {
        DbConfig::build(
            server.to_string(),
            1433,
            "testdb".to_string(),
            AuthKind::SqlServer {
                user: "sa".to_string(),
                password: "pass".to_string(),
            },
            encrypt_env,
        )
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

    #[test]
    fn parse_auth_mode_accepts_sql_and_default() {
        assert_eq!(parse_auth_mode(None).unwrap(), AuthMode::Sql);
        assert_eq!(parse_auth_mode(Some("")).unwrap(), AuthMode::Sql);
        assert_eq!(parse_auth_mode(Some("sql")).unwrap(), AuthMode::Sql);
        assert_eq!(parse_auth_mode(Some(" SQL ")).unwrap(), AuthMode::Sql);
    }

    #[cfg(windows)]
    #[test]
    fn parse_auth_mode_accepts_windows_on_windows() {
        for value in ["windows", "WIN", "integrated"] {
            assert_eq!(
                parse_auth_mode(Some(value)).unwrap(),
                AuthMode::Windows,
                "should accept: {value}"
            );
        }
    }

    #[cfg(not(windows))]
    #[test]
    fn parse_auth_mode_rejects_windows_off_windows() {
        for value in ["windows", "win", "integrated"] {
            assert!(parse_auth_mode(Some(value)).is_err(), "should reject: {value}");
        }
    }

    #[test]
    fn parse_auth_mode_rejects_unknown_values() {
        assert!(parse_auth_mode(Some("ntlm")).is_err());
        assert!(parse_auth_mode(Some("ldap")).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn build_supports_windows_integrated_auth() {
        let config = DbConfig::build(
            "localhost".to_string(),
            1433,
            "testdb".to_string(),
            AuthKind::WindowsIntegrated,
            false,
        );
        assert_eq!(config.auth, AuthKind::WindowsIntegrated);
        assert_eq!(
            config.auth.describe(),
            "current Windows user (integrated auth)"
        );
    }

    #[cfg(windows)]
    #[test]
    fn from_env_windows_mode_does_not_require_credentials() {
        unsafe {
            env::set_var("MSSQL_DATABASE", "testdb");
            env::set_var("MSSQL_AUTH", "windows");
            env::remove_var("MSSQL_USER");
            env::remove_var("MSSQL_PASSWORD");
        }
        let config = DbConfig::from_env().unwrap();
        assert_eq!(config.auth, AuthKind::WindowsIntegrated);
    }

    #[cfg(not(windows))]
    #[test]
    fn from_env_rejects_windows_mode_off_windows() {
        unsafe {
            env::set_var("MSSQL_DATABASE", "testdb");
            env::set_var("MSSQL_AUTH", "windows");
            env::remove_var("MSSQL_USER");
            env::remove_var("MSSQL_PASSWORD");
        }
        assert!(DbConfig::from_env().is_err());
    }

    #[test]
    fn describe_never_leaks_password() {
        let auth = AuthKind::SqlServer {
            user: "sa".to_string(),
            password: "secret".to_string(),
        };
        assert_eq!(auth.describe(), "sa (SQL login)");
    }
}
