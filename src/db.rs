//! 数据库访问：建立连接、执行查询、列出与读取表。

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::config::{AuthKind, DbConfig};
use crate::sql::{is_read_only_query, is_tables_listing_query};
use crate::values::column_data_to_string;

/// 单个结果集（列名 + 每行的字符串值）。
struct Resultset {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// 建立到 SQL Server 的连接（每次请求一个短连接，与参考实现一致）。
async fn connect(config: &DbConfig) -> Result<Client<Compat<TcpStream>>> {
    let mut tds_config = Config::new();
    tds_config.host(&config.server);
    tds_config.port(config.port);
    tds_config.database(&config.database);
    tds_config.authentication(match &config.auth {
        AuthKind::SqlServer { user, password } => AuthMethod::sql_server(user, password),
        // tiberius 0.13：Integrated = SSPI 当前登录用户（Windows + winauth feature）。
        #[cfg(windows)]
        AuthKind::WindowsIntegrated => AuthMethod::Integrated,
    });
    tds_config.encryption(if config.encrypt {
        EncryptionLevel::On
    } else {
        EncryptionLevel::Off
    });
    if config.trust_server_certificate {
        tds_config.trust_cert();
    }

    let tcp = TcpStream::connect((config.server.as_str(), config.port))
        .await
        .with_context(|| format!("failed to connect to {}", config.server))?;
    tcp.set_nodelay(true).context("failed to set TCP_NODELAY")?;
    Client::connect(tds_config, tcp.compat_write())
        .await
        .context("failed to complete TDS handshake")
}

/// 消费查询流，收集第一个结果集（能正确处理空结果集的列名）。
async fn collect_first_resultset(stream: tiberius::QueryStream<'_>) -> Result<Resultset> {
    let mut stream = stream;
    let mut columns: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();

    while let Some(item) = stream.next().await {
        match item? {
            QueryItem::Metadata(meta) => {
                // 第一个 metadata 提供列名，第二个 metadata 意味着新的结果集，停止收集。
                if columns.is_none() {
                    columns = Some(
                        meta.columns()
                            .iter()
                            .map(|c| c.name().to_string())
                            .collect(),
                    );
                } else {
                    break;
                }
            }
            QueryItem::Row(row) => {
                if columns.is_none() {
                    columns = Some(row.columns().iter().map(|c| c.name().to_string()).collect());
                }
                rows.push(
                    row.cells()
                        .map(|(_, data)| column_data_to_string(data))
                        .collect(),
                );
            }
        }
    }

    Ok(Resultset {
        columns: columns.unwrap_or_default(),
        rows,
    })
}

/// 把结果集渲染为 CSV（首行列名），与参考实现输出格式一致。
fn resultset_to_csv(resultset: &Resultset) -> String {
    let mut lines = vec![resultset.columns.join(",")];
    lines.extend(
        resultset
            .rows
            .iter()
            .map(|row| row.join(","))
            .collect::<Vec<_>>(),
    );
    lines.join("\n")
}

/// 数据库对象类型：用户表或视图。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DbObjectKind {
    Table,
    View,
}

impl DbObjectKind {
    /// 资源描述用的人类可读标签。
    pub fn label(self) -> &'static str {
        match self {
            Self::Table => "Table",
            Self::View => "View",
        }
    }
}

/// 当前数据库中的用户表或视图。
#[derive(Debug, Clone)]
pub struct DbObject {
    pub name: String,
    pub kind: DbObjectKind,
}

/// 列出当前数据库的所有用户表与视图。
pub async fn list_tables_and_views(config: &DbConfig) -> Result<Vec<DbObject>> {
    let mut client = connect(config).await?;
    let stream = client
        .simple_query(
            "SELECT TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.TABLES \
             WHERE TABLE_TYPE IN ('BASE TABLE', 'VIEW') ORDER BY TABLE_NAME",
        )
        .await
        .context("failed to list tables and views")?;
    let resultset = collect_first_resultset(stream).await?;
    Ok(resultset
        .rows
        .into_iter()
        .filter_map(|row| {
            let name = row.first()?.clone();
            let kind = match row.get(1).map(String::as_str) {
                Some("VIEW") => DbObjectKind::View,
                _ => DbObjectKind::Table,
            };
            Some(DbObject { name, kind })
        })
        .collect())
}

/// 读取表或视图的前 100 行（对象名必须已经过 `validate_table_name` 转义）。
pub async fn read_table(config: &DbConfig, safe_table: &str) -> Result<String> {
    let mut client = connect(config).await?;
    let query = format!("SELECT TOP 100 * FROM {safe_table}");
    let stream = client
        .simple_query(query)
        .await
        .context("failed to read table rows")?;
    let resultset = collect_first_resultset(stream).await?;
    Ok(resultset_to_csv(&resultset))
}

/// 查询表或视图的结构（列名、类型、可空性、长度/精度、默认值），按列序排列。
/// `INFORMATION_SCHEMA.COLUMNS` 同时覆盖表与视图，二者行为一致。
/// 表名经参数绑定传入，无注入风险；`schema` 为 `None` 时在所有 schema 中
/// 按表名匹配（结果包含 `TABLE_SCHEMA` 列以示区分）。
pub async fn describe_table(
    config: &DbConfig,
    schema: Option<&str>,
    table: &str,
) -> Result<String> {
    let mut client = connect(config).await?;
    const DESCRIBE_SQL: &str = "SELECT TABLE_SCHEMA, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, \
         CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, COLUMN_DEFAULT \
         FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = @P1";
    let stream = match schema {
        Some(schema_name) => {
            let sql = format!("{DESCRIBE_SQL} AND TABLE_SCHEMA = @P2 ORDER BY ORDINAL_POSITION");
            client.query(&sql, &[&table, &schema_name]).await
        }
        None => {
            let sql = format!("{DESCRIBE_SQL} ORDER BY ORDINAL_POSITION");
            client.query(&sql, &[&table]).await
        }
    }
    .context("failed to describe table")?;
    let resultset = collect_first_resultset(stream).await?;
    if resultset.rows.is_empty() {
        bail!("No columns found for table '{table}'");
    }
    Ok(resultset_to_csv(&resultset))
}

/// 执行只读 SQL 查询：仅接受单条 SELECT（含 `WITH ... SELECT`），
/// 任何修改语句（INSERT/UPDATE/DELETE/DDL/EXEC 等）或多语句批次一律拒绝。
pub async fn execute_query(config: &DbConfig, query: &str) -> Result<String> {
    if !is_read_only_query(query) {
        bail!(
            "read-only mode: only a single read-only SELECT query is allowed \
             (WITH ... SELECT is also supported); \
             statements that modify the database are rejected"
        );
    }

    let mut client = connect(config).await?;
    let stream = client
        .simple_query(query)
        .await
        .context("failed to execute query")?;
    let resultset = collect_first_resultset(stream).await?;

    // 对 INFORMATION_SCHEMA.TABLES 的查询输出 mysql 风格的表清单（对齐参考实现）。
    if is_tables_listing_query(query) && !resultset.columns.is_empty() {
        let header = format!("Tables_in_{}", config.database);
        let mut lines = vec![header];
        lines.extend(
            resultset
                .rows
                .iter()
                .map(|row| row.first().cloned().unwrap_or_default()),
        );
        return Ok(lines.join("\n"));
    }

    Ok(resultset_to_csv(&resultset))
}
