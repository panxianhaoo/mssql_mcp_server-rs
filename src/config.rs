//! 数据库配置：从环境变量读取连接信息。

use std::env;

use anyhow::{Context, Result, bail};

const DEFAULT_SERVER: &str = "localhost";
const DEFAULT_PORT: u16 = 1433;
const AZURE_DOMAIN_MARKER: &str = ".database.windows.net";

/// 实际使用的认证方式：SQL 登录或 Windows 集成认证（SSPI）。
#[derive(Debug, Clone, PartialEq)]
pub enum AuthKind {
    /// SQL Server 登录（用户名 + 密码）。
    SqlServer { user: String, password: String },
    /// Windows 集成认证：使用当前登录用户身份，无需凭据（仅 Windows 平台存在此变体）。
    #[cfg(windows)]
    WindowsIntegrated,
}

impl AuthKind {
    /// 日志用的人类可读描述（绝不包含密码）。
    pub fn describe(&self) -> String {
        match self {
            Self::SqlServer { user, .. } => format!("{user} (SQL login)"),
            #[cfg(windows)]
            Self::WindowsIntegrated => "current Windows user (integrated auth)".to_string(),
        }
    }

    /// 该认证方式是否应当默认走加密连接。
    ///
    /// 集成认证在线路上传输 NTLM/Kerberos 握手凭据，明文链路等于把身份
    /// 暴露给同网段的观察者；SQL 登录虽也传密码，但那些密码通常专用于
    /// 数据库，且大量既有本地实例只用自签证书，故沿用原有默认不动。
    fn requires_encryption(&self) -> bool {
        match self {
            Self::SqlServer { .. } => false,
            #[cfg(windows)]
            Self::WindowsIntegrated => true,
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
        Self::from_lookup(&|name| env::var(name).ok())
    }

    /// 解析配置的通用入口：`lookup` 按键名返回变量值（`None` = 未设置）。
    ///
    /// 把「读取来源」与「解析逻辑」解耦：进程环境变量只是一种来源，测试可注入
    /// 任意来源。这样既不必在多线程测试里改写全局 env（Edition 2024 起
    /// `env::set_var` 为 `unsafe`，且并行测试会互相污染），也让全部配置分支
    /// 可在任何平台上被覆盖。
    fn from_lookup(lookup: &dyn Fn(&str) -> Option<String>) -> Result<Self> {
        // 空字符串视为未设置（与 MSSQL_PORT/MSSQL_AUTH 的处理保持一致）。
        let server = lookup("MSSQL_SERVER")
            .map(|value| value.trim().to_string())
            .filter(|value| !value.is_empty())
            .unwrap_or_else(|| DEFAULT_SERVER.to_string());
        let database = required(lookup, "MSSQL_DATABASE")?;
        let mode = parse_auth_mode(lookup("MSSQL_AUTH").as_deref())?;
        let auth = match mode {
            AuthMode::Sql => AuthKind::SqlServer {
                user: required(lookup, "MSSQL_USER")?,
                password: required(lookup, "MSSQL_PASSWORD")?,
            },
            #[cfg(windows)]
            AuthMode::Windows => AuthKind::WindowsIntegrated,
        };
        let port = parse_port(lookup("MSSQL_PORT").as_deref())?;
        // 保留「未设置」与「显式 false」的区别：集成认证要据此决定是否强制加密。
        let encrypt = lookup("MSSQL_ENCRYPT")
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
            .map(|v| v.eq_ignore_ascii_case("true"));
        Ok(Self::build(server, port, database, auth, encrypt))
    }

    /// 纯函数构建：Azure 与集成认证连接强制加密（除非显式关闭），Azure 另
    /// 要求校验证书。
    ///
    /// `encrypt_env` 为 `None` 表示用户未表达偏好，此时按认证方式给安全默认；
    /// `Some(false)` 是显式要求明文，必须予以尊重（本地自签证书排查场景）。
    fn build(
        server: String,
        port: u16,
        database: String,
        auth: AuthKind,
        encrypt_env: Option<bool>,
    ) -> Self {
        let is_azure = server.to_ascii_lowercase().contains(AZURE_DOMAIN_MARKER);
        // 集成认证走 NTLM/Kerberos 握手，凭据在链路上传输，必须包进 TLS；
        // 仅有用户显式 `MSSQL_ENCRYPT=false` 才放行明文（此时 TLS 与自签证书
        // 都无从谈起，故一并信任）。
        let encrypt = match encrypt_env {
            Some(explicit) => is_azure || explicit,
            None => is_azure || auth.requires_encryption(),
        };
        // Azure 必须校验证书；集成认证则是 TLS 握手后仍可能用到自签证书，
        // 本地/自建实例为方便起见默认信任。
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

/// 读取必填变量，缺失时返回带变量名的明确错误。
fn required(lookup: &dyn Fn(&str) -> Option<String>, name: &str) -> Result<String> {
    lookup(name).with_context(|| {
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

    /// 由 `(key, value)` 列表构造变量查找函数：不触碰进程全局 env，
    /// 因此测试互不干扰、可安全并行执行。
    fn lookup_from(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        // 转为自有数据，闭包无需借用入参（省去生命周期标注）。
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

    /// 常见的最小可用配置：MSSQL_DATABASE + SQL 登录凭据。
    const MINIMAL_PAIRS: &[(&str, &str)] = &[
        ("MSSQL_DATABASE", "testdb"),
        ("MSSQL_USER", "sa"),
        ("MSSQL_PASSWORD", "pass"),
    ];

    fn build_config(server: &str, encrypt_env: Option<bool>) -> DbConfig {
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
        // SQL 登录且未指定：保持明文默认，兼容既有本地自签实例。
        let config = build_config("localhost", None);
        assert!(!config.encrypt);
        assert!(config.trust_server_certificate);
        assert!(!config.is_azure());
    }

    #[test]
    fn build_respects_encrypt_env_for_local_server() {
        let config = build_config("localhost", Some(true));
        assert!(config.encrypt);
        assert!(config.trust_server_certificate);
    }

    #[test]
    fn build_respects_explicit_plaintext_request() {
        let config = build_config("localhost", Some(false));
        assert!(!config.encrypt);
    }

    #[test]
    fn build_forces_encryption_on_azure() {
        // Azure 必须加密：即使显式关闭也不能放行明文。
        let config = build_config("myserver.database.windows.net", Some(false));
        assert!(config.is_azure());
        assert!(config.encrypt);
        assert!(!config.trust_server_certificate);
    }

    #[test]
    fn build_detects_azure_case_insensitively() {
        let config = build_config("MyServer.Database.Windows.NET", None);
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
    fn parse_auth_mode_accepts_sql_and_default() {
        assert_eq!(parse_auth_mode(None).unwrap(), AuthMode::Sql);
        assert_eq!(parse_auth_mode(Some("")).unwrap(), AuthMode::Sql);
        assert_eq!(parse_auth_mode(Some("sql")).unwrap(), AuthMode::Sql);
        assert_eq!(parse_auth_mode(Some(" SQL ")).unwrap(), AuthMode::Sql);
    }

    #[test]
    fn parse_auth_mode_rejects_unknown_values() {
        assert!(parse_auth_mode(Some("ntlm")).is_err());
        assert!(parse_auth_mode(Some("ldap")).is_err());
    }

    #[test]
    fn describe_never_leaks_password() {
        let auth = AuthKind::SqlServer {
            user: "sa".to_string(),
            password: "secret".to_string(),
        };
        assert_eq!(auth.describe(), "sa (SQL login)");
    }

    #[test]
    fn from_lookup_builds_config_from_injected_source() {
        let config = DbConfig::from_lookup(&lookup_from(MINIMAL_PAIRS)).unwrap();
        assert_eq!(config.database, "testdb");
        assert_eq!(
            config.auth,
            AuthKind::SqlServer {
                user: "sa".to_string(),
                password: "pass".to_string()
            }
        );
    }

    #[test]
    fn from_lookup_defaults_server_and_port() {
        let config = DbConfig::from_lookup(&lookup_from(MINIMAL_PAIRS)).unwrap();
        assert_eq!(config.server, DEFAULT_SERVER);
        assert_eq!(config.port, DEFAULT_PORT);
    }

    #[test]
    fn from_lookup_reads_server_port_and_encrypt() {
        let pairs = &[
            ("MSSQL_SERVER", "dbhost"),
            ("MSSQL_PORT", "1435"),
            ("MSSQL_DATABASE", "proddb"),
            ("MSSQL_USER", "app"),
            ("MSSQL_PASSWORD", "secret"),
            ("MSSQL_ENCRYPT", "true"),
        ];
        let config = DbConfig::from_lookup(&lookup_from(pairs)).unwrap();
        assert_eq!(config.server, "dbhost");
        assert_eq!(config.port, 1435);
        assert_eq!(config.database, "proddb");
        assert!(config.encrypt);
        assert!(!config.is_azure());
    }

    #[test]
    fn from_lookup_rejects_missing_required_variables() {
        // 逐个摘掉必填变量，每个都应报错且错误里点名缺失的变量。
        for missing in ["MSSQL_DATABASE", "MSSQL_USER", "MSSQL_PASSWORD"] {
            let pairs: Vec<(&str, &str)> = MINIMAL_PAIRS
                .iter()
                .filter(|(k, _)| *k != missing)
                .copied()
                .collect();
            let err = DbConfig::from_lookup(&lookup_from(&pairs)).unwrap_err();
            let message = format!("{err:#}");
            assert!(
                message.contains(missing),
                "error should name {missing}, got: {message}"
            );
        }
    }

    #[test]
    fn from_lookup_rejects_invalid_port() {
        let pairs = &[
            ("MSSQL_DATABASE", "testdb"),
            ("MSSQL_USER", "sa"),
            ("MSSQL_PASSWORD", "pass"),
            ("MSSQL_PORT", "abc"),
        ];
        let err = DbConfig::from_lookup(&lookup_from(pairs)).unwrap_err();
        assert!(format!("{err:#}").contains("Invalid MSSQL_PORT"));
    }

    #[test]
    fn from_lookup_propagates_invalid_auth_mode() {
        let pairs = &[
            ("MSSQL_DATABASE", "testdb"),
            ("MSSQL_USER", "sa"),
            ("MSSQL_PASSWORD", "pass"),
            ("MSSQL_AUTH", "kerberos"),
        ];
        assert!(DbConfig::from_lookup(&lookup_from(pairs)).is_err());
    }

    #[test]
    fn from_lookup_accepts_explicit_sql_mode() {
        let pairs = &[
            ("MSSQL_DATABASE", "testdb"),
            ("MSSQL_USER", "sa"),
            ("MSSQL_PASSWORD", "pass"),
            ("MSSQL_AUTH", "sql"),
        ];
        let config = DbConfig::from_lookup(&lookup_from(pairs)).unwrap();
        assert_eq!(
            config.auth,
            AuthKind::SqlServer {
                user: "sa".to_string(),
                password: "pass".to_string()
            }
        );
    }

    #[test]
    fn from_lookup_treats_empty_server_as_unset() {
        // 空字符串变量视为未设置：回落到 localhost 而非连向空主机。
        let pairs = &[
            ("MSSQL_SERVER", ""),
            ("MSSQL_DATABASE", "testdb"),
            ("MSSQL_USER", "sa"),
            ("MSSQL_PASSWORD", "pass"),
        ];
        let config = DbConfig::from_lookup(&lookup_from(pairs)).unwrap();
        assert_eq!(config.server, "localhost");
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
            assert!(
                parse_auth_mode(Some(value)).is_err(),
                "should reject: {value}"
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn build_supports_windows_integrated_auth() {
        let config = DbConfig::build(
            "localhost".to_string(),
            1433,
            "testdb".to_string(),
            AuthKind::WindowsIntegrated,
            None,
        );
        assert_eq!(config.auth, AuthKind::WindowsIntegrated);
        assert_eq!(
            config.auth.describe(),
            "current Windows user (integrated auth)"
        );
    }

    #[cfg(windows)]
    #[test]
    fn build_encrypts_integrated_auth_by_default() {
        // 回归点：集成认证的 NTLM/Kerberos 握手在线路上传输凭据，未指定
        // MSSQL_ENCRYPT 时必须包进 TLS，不能沿用 SQL 登录的明文默认。
        let config = DbConfig::build(
            "localhost".to_string(),
            1433,
            "testdb".to_string(),
            AuthKind::WindowsIntegrated,
            None,
        );
        assert!(config.encrypt, "integrated auth must default to TLS");

        let sql_login = DbConfig::build(
            "localhost".to_string(),
            1433,
            "testdb".to_string(),
            AuthKind::SqlServer {
                user: "sa".to_string(),
                password: "pass".to_string(),
            },
            None,
        );
        assert!(
            !sql_login.encrypt,
            "SQL login plaintext default must stay unchanged"
        );
    }

    #[cfg(windows)]
    #[test]
    fn build_honors_explicit_plaintext_for_integrated_auth() {
        // 显式 MSSQL_ENCRYPT=false 是用户有意为之，必须尊重
        // （本地自签证书排查等场景）。
        let config = DbConfig::build(
            "localhost".to_string(),
            1433,
            "testdb".to_string(),
            AuthKind::WindowsIntegrated,
            Some(false),
        );
        assert!(!config.encrypt);
    }

    #[cfg(windows)]
    #[test]
    fn from_lookup_encrypts_integrated_auth_end_to_end() {
        let pairs = &[("MSSQL_DATABASE", "testdb"), ("MSSQL_AUTH", "windows")];
        let config = DbConfig::from_lookup(&lookup_from(pairs)).unwrap();
        assert!(config.encrypt, "must enable TLS for integrated auth");

        let explicit = &[
            ("MSSQL_DATABASE", "testdb"),
            ("MSSQL_AUTH", "windows"),
            ("MSSQL_ENCRYPT", "false"),
        ];
        let config = DbConfig::from_lookup(&lookup_from(explicit)).unwrap();
        assert!(!config.encrypt, "explicit opt-out must be honored");
    }

    #[cfg(windows)]
    #[test]
    fn from_lookup_windows_mode_does_not_require_credentials() {
        // 无 MSSQL_USER / MSSQL_PASSWORD：集成认证用当前登录用户身份。
        let pairs = &[("MSSQL_DATABASE", "testdb"), ("MSSQL_AUTH", "windows")];
        let config = DbConfig::from_lookup(&lookup_from(pairs)).unwrap();
        assert_eq!(config.auth, AuthKind::WindowsIntegrated);
        assert_eq!(config.database, "testdb");
    }

    #[cfg(not(windows))]
    #[test]
    fn from_lookup_rejects_windows_mode_off_windows() {
        // 非 Windows 平台启动即报错，避免留到连接阶段才失败。
        let pairs = &[("MSSQL_DATABASE", "testdb"), ("MSSQL_AUTH", "windows")];
        let err = DbConfig::from_lookup(&lookup_from(pairs)).unwrap_err();
        assert!(format!("{err:#}").contains("only supported when running on Windows"));
    }
}
