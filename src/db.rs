//! 数据库访问：连接池、执行查询、列出与读取表、查询表结构。

use anyhow::{Context, Result, bail};
use bb8_tiberius::ConnectionManager;
use tiberius::{AuthMethod, Client, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::Compat;

use crate::config::{AuthKind, DbConfig};
use crate::resultset::{Resultset, collect_first_resultset, resultset_to_csv};
use crate::sql::{is_read_only_query, is_tables_listing_query};

/// 连接池大小：stdio 单客户端场景，少量连接足够覆盖并发请求。
const DB_POOL_MAX_SIZE: u32 = 4;

/// 数据库连接池（bb8 + bb8-tiberius 管理客户端连接，按需复用）。
pub type DbPool = bb8::Pool<ConnectionManager>;

/// 从应用配置构建 tiberius 连接配置。
fn tiberius_config(config: &DbConfig) -> tiberius::Config {
    let mut tds_config = tiberius::Config::new();
    tds_config.host(&config.server);
    tds_config.port(config.port);
    tds_config.database(&config.database);
    tds_config.authentication(match &config.auth {
        AuthKind::SqlServer { user, password } => AuthMethod::sql_server(user, password),
        // Integrated = SSPI 当前登录用户（Windows + winauth feature）。
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
    tds_config
}

/// 构建连接池（惰性建连：数据库暂不可达时服务器仍可启动，调用时再报错）。
pub fn new_pool(config: &DbConfig) -> DbPool {
    bb8::Pool::builder()
        .max_size(DB_POOL_MAX_SIZE)
        .test_on_check_out(true)
        .build_unchecked(ConnectionManager::new(tiberius_config(config)))
}

/// 从连接池借出一个连接。
async fn get_client(pool: &DbPool) -> Result<bb8::PooledConnection<'_, ConnectionManager>> {
    pool.get()
        .await
        .context("failed to get a connection from pool")
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
pub async fn list_tables_and_views(pool: &DbPool) -> Result<Vec<DbObject>> {
    let mut client = get_client(pool).await?;
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
pub async fn read_table(pool: &DbPool, safe_table: &str) -> Result<String> {
    let mut client = get_client(pool).await?;
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
pub async fn describe_table(pool: &DbPool, schema: Option<&str>, table: &str) -> Result<String> {
    let mut client = get_client(pool).await?;
    let columns = describe_columns(&mut client, schema, table).await?;
    if columns.rows.is_empty() {
        bail!("No columns found for table '{table}'");
    }
    let indexes = describe_indexes(&mut client, schema, table).await?;
    // 分节标记带 `#` 前缀：下游按注释行跳过即可用标准 CSV 解析器读取两段。
    Ok(format!(
        "{}\n\n# INDEXES\n{}",
        resultset_to_csv(&columns),
        resultset_to_csv(&indexes)
    ))
}

/// 查询列结构（`INFORMATION_SCHEMA.COLUMNS`，覆盖表与视图）。
///
/// 注意：tiberius 的 `query(sql, &[params])` 按数组位置绑定占位符，即
/// 第 1、2 个参数分别对应 SQL 中的 `@P1`、`@P2`（见 `tiberius::Client::query`
/// 文档示例）。因此这里传入 `&[&table, &schema_name]` 的顺序必须与 SQL 里
/// 出现的 `@P1`、`@P2` 保持一致，不可按 SQL 书写先后随意重排。
async fn describe_columns(
    client: &mut Client<Compat<TcpStream>>,
    schema: Option<&str>,
    table: &str,
) -> Result<Resultset> {
    let stream = match schema {
        // @P1 = 表名，@P2 = schema 名（位置绑定，见上方说明）。
        Some(schema_name) => {
            let sql = columns_query(schema);
            client.query(&sql, &[&table, &schema_name]).await
        }
        None => {
            let sql = columns_query(schema);
            client.query(&sql, &[&table]).await
        }
    }
    .context("failed to describe table")?;
    collect_first_resultset(stream).await
}

/// 构造列结构查询 SQL：`schema` 为 `Some` 时附加 `TABLE_SCHEMA = @P2` 过滤。
///
/// 输出 9 列，依次为：`TABLE_SCHEMA`、`COLUMN_NAME`、`DATA_TYPE`、`IS_NULLABLE`、
/// `CHARACTER_MAXIMUM_LENGTH`、`NUMERIC_PRECISION`、`NUMERIC_SCALE`、`COLUMN_DEFAULT`、
/// `COLLATION_NAME`。
///
/// 选列依据：`COLLATION_NAME` 携带排序/比较语义，且 SQL Server 支持**列级**
/// collation（同一张表可混用不同规则），因此必须逐列输出；它放在最末以降低
/// 对既有「按位置」消费者的影响。
/// 反之不选 `CHARACTER_SET_NAME` —— nvarchar/ntext/sysname 恒为 `UNICODE`、
/// varchar/text 恒为 `iso_1`，完全可由 `DATA_TYPE` 推出，属冗余列。
fn columns_query(schema: Option<&str>) -> String {
    const SELECT_FROM: &str = "SELECT TABLE_SCHEMA, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, \
         CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, COLUMN_DEFAULT, \
         COLLATION_NAME \
         FROM INFORMATION_SCHEMA.COLUMNS WHERE TABLE_NAME = @P1";
    const ORDER_BY: &str = " ORDER BY ORDINAL_POSITION";
    match schema {
        Some(_) => format!("{SELECT_FROM} AND TABLE_SCHEMA = @P2{ORDER_BY}"),
        None => format!("{SELECT_FROM}{ORDER_BY}"),
    }
}

/// 查询索引信息：`sys.indexes` 联 `sys.index_columns`，每个键列/包含列输出一行
/// （tidy/long 格式）。堆表无索引行，返回仅含表头的空结果。
///
/// 之所以不用 `STRING_AGG` 把多个键列拼进单个字段：拼接结果里的逗号会破坏
/// CSV 的列对齐（复合索引很常见，例如 `(last_name, first_name)`），而这属于
/// SQL Server 侧聚合，Rust 转义层无从补救。一行一列则天然无歧义。
async fn describe_indexes(
    client: &mut Client<Compat<TcpStream>>,
    schema: Option<&str>,
    table: &str,
) -> Result<Resultset> {
    let sql = indexes_query(schema);
    // 位置绑定：`@P1` = 表名，`@P2` = schema 名。
    let stream = match schema {
        Some(schema_name) => client.query(&sql, &[&table, &schema_name]).await,
        None => client.query(&sql, &[&table]).await,
    }
    .context("failed to describe indexes")?;
    collect_first_resultset(stream).await
}

/// 构造索引查询 SQL：`schema` 为 `Some` 时附加 `SCHEMA_NAME = @P2` 过滤。
///
/// 每个索引列一行，列含：`OBJECT_SCHEMA`、`INDEX_NAME`、`INDEX_TYPE`、`IS_UNIQUE`、
/// `IS_PRIMARY_KEY`、`COLUMN_NAME`、`IS_INCLUDED_COLUMN`、`KEY_ORDINAL`。
/// 排序：主键索引优先，其后按 schema、索引名，索引内部先是键列（按 `key_ordinal`）
/// 再是包含列。
fn indexes_query(schema: Option<&str>) -> String {
    const SELECT_FROM: &str = "SELECT SCHEMA_NAME(o.schema_id) AS OBJECT_SCHEMA, \
         i.name AS INDEX_NAME, i.type_desc AS INDEX_TYPE, \
         CASE WHEN i.is_unique = 1 THEN 'YES' ELSE 'NO' END AS IS_UNIQUE, \
         CASE WHEN i.is_primary_key = 1 THEN 'YES' ELSE 'NO' END AS IS_PRIMARY_KEY, \
         c.name AS COLUMN_NAME, \
         CASE WHEN ic.is_included_column = 1 THEN 'YES' ELSE 'NO' END AS IS_INCLUDED_COLUMN, \
         ic.key_ordinal AS KEY_ORDINAL \
         FROM sys.indexes i \
         JOIN sys.objects o ON o.object_id = i.object_id \
         JOIN sys.index_columns ic \
             ON ic.object_id = i.object_id AND ic.index_id = i.index_id \
         JOIN sys.columns c ON c.object_id = ic.object_id AND c.column_id = ic.column_id \
         WHERE o.name = @P1";
    const ORDER_BY: &str = " ORDER BY IS_PRIMARY_KEY DESC, OBJECT_SCHEMA, INDEX_NAME, \
         ic.is_included_column, ic.key_ordinal";
    match schema {
        Some(_) => format!("{SELECT_FROM} AND SCHEMA_NAME(o.schema_id) = @P2{ORDER_BY}"),
        None => format!("{SELECT_FROM}{ORDER_BY}"),
    }
}

/// 执行只读 SQL 查询：仅接受单条 SELECT（含 `WITH ... SELECT`），
/// 任何修改语句（INSERT/UPDATE/DELETE/DDL/EXEC 等）或多语句批次一律拒绝。
pub async fn execute_query(pool: &DbPool, database: &str, query: &str) -> Result<String> {
    if !is_read_only_query(query) {
        bail!(
            "read-only mode: only a single read-only SELECT query is allowed \
             (WITH ... SELECT is also supported); \
             statements that modify the database are rejected"
        );
    }

    let mut client = get_client(pool).await?;
    let stream = client
        .simple_query(query)
        .await
        .context("failed to execute query")?;
    let resultset = collect_first_resultset(stream).await?;

    // 对 INFORMATION_SCHEMA.TABLES 的查询输出 mysql 风格的表清单（对齐参考实现）。
    if is_tables_listing_query(query) && !resultset.columns.is_empty() {
        let header = format!("Tables_in_{database}");
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
        assert!(sql.contains("WHERE o.name = @P1"));
        assert!(!sql.contains("@P2"));
        assert!(!sql.contains("STRING_AGG"));
    }

    #[test]
    fn indexes_query_with_schema_filters_by_schema() {
        let sql = indexes_query(Some("dbo"));
        assert!(sql.contains("WHERE o.name = @P1 AND SCHEMA_NAME(o.schema_id) = @P2"));
    }

    #[test]
    fn indexes_query_orders_primary_key_first() {
        for sql in [indexes_query(None), indexes_query(Some("dbo"))] {
            assert!(sql.ends_with(
                " ORDER BY IS_PRIMARY_KEY DESC, OBJECT_SCHEMA, INDEX_NAME, \
         ic.is_included_column, ic.key_ordinal"
            ));
        }
    }

    #[test]
    fn indexes_query_emits_one_row_per_column() {
        // 回归点：绝不能回到 STRING_AGG —— 复合索引的逗号拼接会撕裂 CSV。
        for sql in [indexes_query(None), indexes_query(Some("dbo"))] {
            assert!(
                !sql.contains("STRING_AGG"),
                "must not aggregate into one field"
            );
            assert!(
                !sql.contains("GROUP BY"),
                "no aggregation means no GROUP BY"
            );
            assert!(sql.contains("c.name AS COLUMN_NAME"));
            assert!(sql.contains("ic.key_ordinal AS KEY_ORDINAL"));
            assert!(sql.contains("IS_INCLUDED_COLUMN"));
        }
    }

    #[test]
    fn columns_query_selects_collation_last() {
        // 回归点：COLLATION_NAME 必须存在且位于最末（见 columns_query 的选列说明）。
        for sql in [columns_query(None), columns_query(Some("dbo"))] {
            assert!(
                sql.contains("COLLATION_NAME"),
                "collation column missing: {sql}"
            );
            // CHARACTER_SET_NAME 是冗余列，不应出现在结果里。
            assert!(
                !sql.contains("CHARACTER_SET_NAME"),
                "character set is derivable from DATA_TYPE: {sql}"
            );
        }
    }

    #[test]
    fn columns_query_columns_in_stable_order() {
        let sql = columns_query(None);
        let expected = [
            "TABLE_SCHEMA",
            "COLUMN_NAME",
            "DATA_TYPE",
            "IS_NULLABLE",
            "CHARACTER_MAXIMUM_LENGTH",
            "NUMERIC_PRECISION",
            "NUMERIC_SCALE",
            "COLUMN_DEFAULT",
            "COLLATION_NAME",
        ];
        let positions: Vec<usize> = expected
            .iter()
            .map(|c| sql.find(c).unwrap_or_else(|| panic!("{c} missing: {sql}")))
            .collect();
        assert!(
            positions.windows(2).all(|w| w[0] < w[1]),
            "columns must appear in order: {expected:?} -> {positions:?}"
        );
    }

    #[test]
    fn columns_query_with_schema_filters_by_schema() {
        assert!(columns_query(Some("dbo")).contains("AND TABLE_SCHEMA = @P2"));
        assert!(!columns_query(None).contains("@P2"));
    }

    #[test]
    fn parameter_placeholders_precede_usage_in_bind_order() {
        // 回归点：tiberius 按位置绑定，因此 SQL 中出现 @P2 之前必须先出现 @P1，
        // 且 @P1 始终绑到表名而非 schema；一旦 SQL 拼接顺序变动，此断言会先失败。
        let sql = indexes_query(Some("dbo"));
        assert!(sql.find("@P1").unwrap() < sql.find("@P2").unwrap());
        assert!(sql.contains("WHERE o.name = @P1 AND SCHEMA_NAME(o.schema_id) = @P2"));

        let columns_sql = "SELECT TABLE_SCHEMA FROM INFORMATION_SCHEMA.COLUMNS \
             WHERE TABLE_NAME = @P1 AND TABLE_SCHEMA = @P2 ORDER BY ORDINAL_POSITION";
        assert!(columns_sql.find("@P1").unwrap() < columns_sql.find("@P2").unwrap());
    }
}
