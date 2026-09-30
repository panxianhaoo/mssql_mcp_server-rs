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

/// 查询表或视图的结构：列信息（名称、类型、可空性、长度/精度、默认值，按列序
/// 排列）之后跟随 `INDEXES` 分节，列出索引（名称、类型、唯一性、主键、键列、
/// 包含列，CSV，无索引时仅表头）。
/// 列信息基于 `INFORMATION_SCHEMA.COLUMNS`，索引基于 `sys.indexes`，二者同时
/// 覆盖表与视图，schema 匹配语义一致。
/// 表名经参数绑定传入，无注入风险；`schema` 为 `None` 时在所有 schema 中
/// 按表名匹配（结果包含 `TABLE_SCHEMA`/`OBJECT_SCHEMA` 列以示区分）。
pub async fn describe_table(
    config: &DbConfig,
    schema: Option<&str>,
    table: &str,
) -> Result<String> {
    let mut client = connect(config).await?;
    let columns = describe_columns(&mut client, schema, table).await?;
    if columns.rows.is_empty() {
        bail!("No columns found for table '{table}'");
    }
    let indexes = describe_indexes(&mut client, schema, table).await?;
    Ok(format!(
        "{}\n\nINDEXES\n{}",
        resultset_to_csv(&columns),
        resultset_to_csv(&indexes)
    ))
}

/// 查询列结构（`INFORMATION_SCHEMA.COLUMNS`，覆盖表与视图）。
async fn describe_columns(
    client: &mut Client<Compat<TcpStream>>,
    schema: Option<&str>,
    table: &str,
) -> Result<Resultset> {
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
    collect_first_resultset(stream).await
}

/// 查询索引信息：`sys.indexes` 聚合键列与包含列（`STRING_AGG`，需 SQL Server 2017+）。
/// 堆表无索引行，返回仅含表头的空结果。
async fn describe_indexes(
    client: &mut Client<Compat<TcpStream>>,
    schema: Option<&str>,
    table: &str,
) -> Result<Resultset> {
    let sql = indexes_query(schema);
    let stream = match schema {
        Some(schema_name) => client.query(&sql, &[&table, &schema_name]).await,
        None => client.query(&sql, &[&table]).await,
    }
    .context("failed to describe indexes")?;
    collect_first_resultset(stream).await
}

/// 构造索引查询 SQL：`schema` 为 `Some` 时附加 `SCHEMA_NAME = @P2` 过滤；
/// 主键排在最前，其余按 schema、名称排序。
fn indexes_query(schema: Option<&str>) -> String {
    const BASE: &str = "SELECT SCHEMA_NAME(o.schema_id) AS OBJECT_SCHEMA, \
         i.name AS INDEX_NAME, i.type_desc AS INDEX_TYPE, \
         CASE WHEN i.is_unique = 1 THEN 'YES' ELSE 'NO' END AS IS_UNIQUE, \
         CASE WHEN i.is_primary_key = 1 THEN 'YES' ELSE 'NO' END AS IS_PRIMARY_KEY, \
         STRING_AGG(CAST(c.name AS nvarchar(max)), ',') \
             WITHIN GROUP (ORDER BY ic.key_ordinal) AS KEY_COLUMNS, \
         STRING_AGG(CASE WHEN ic.is_included_column = 1 THEN c.name END, ',') \
             WITHIN GROUP (ORDER BY ic.key_ordinal) AS INCLUDED_COLUMNS \
         FROM sys.indexes i \
         JOIN sys.objects o ON o.object_id = i.object_id \
         JOIN sys.index_columns ic \
             ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE o.name = @P1";
    const ORDER_BY: &str = " ORDER BY IS_PRIMARY_KEY DESC, OBJECT_SCHEMA, INDEX_NAME";
    match schema {
        Some(_) => format!("{BASE} AND SCHEMA_NAME(o.schema_id) = @P2{ORDER_BY}"),
        None => format!("{BASE}{ORDER_BY}"),
    }
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn indexes_query_without_schema_matches_all_schemas() {
        let sql = indexes_query(None);
        assert!(sql.contains("WHERE o.name = @P1 ORDER BY"));
        assert!(!sql.contains("@P2"));
    }

    #[test]
    fn indexes_query_with_schema_filters_by_schema() {
        let sql = indexes_query(Some("dbo"));
        assert!(sql.contains("WHERE o.name = @P1 AND SCHEMA_NAME(o.schema_id) = @P2"));
    }

    #[test]
    fn indexes_query_orders_primary_key_first() {
        for sql in [indexes_query(None), indexes_query(Some("dbo"))] {
            assert!(sql.ends_with(" ORDER BY IS_PRIMARY_KEY DESC, OBJECT_SCHEMA, INDEX_NAME"));
        }
    }
}
