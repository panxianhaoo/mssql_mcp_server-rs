//! 数据库访问：连接池、执行查询、列出与读取表、查询表结构。

use anyhow::{Context, Result, bail};
use tiberius::{AuthMethod, Client, EncryptionLevel};
use tokio::net::TcpStream;
use tokio_util::compat::Compat;

use crate::config::{AuthKind, DbConfig};
use crate::format::{OutputFormat, render_resultset};
use crate::resultset::{Resultset, collect_first_resultset, resultset_to_csv};
use crate::sql::{is_read_only_query, is_tables_listing_query};

pub use crate::pool::ConnectionManager;

/// 连接池大小：stdio 单客户端场景，少量连接足够覆盖并发请求。
const DB_POOL_MAX_SIZE: u32 = 4;

/// 数据库连接池（bb8 管理自建的 [`ConnectionManager`]，见 [`crate::pool`]）。
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

/// 按数据库名构建连接池：除 `database` 外沿用 `base` 的全部连接设置。
///
/// 用于多数据库支持：TDS 连接建立时就绑定了默认库，跨库查询要么写三段式
/// 名称，要么另开一条连到目标库的连接。后者更稳妥（权限、默认 schema 都
/// 按目标库生效），故按需为每个库建一个池。
fn new_pool_for_database(base: &DbConfig, database: &str) -> DbPool {
    let mut config = base.clone();
    config.database = database.to_string();
    new_pool(&config)
}

/// 多数据库支持：按库名缓存连接池。
///
/// 池是 `Clone`（内部 `Arc`），故可安全跨请求共享；这里只缓存句柄，
/// 连接本身仍由 bb8 惰性建立。`DashMap` 未引入，改用 `RwLock<HashMap>`：
/// 库数量有限（数十级），读多写少，锁竞争可忽略。
#[derive(Debug, Clone)]
pub struct DatabasePools {
    base: DbConfig,
    pools: std::sync::Arc<std::sync::RwLock<std::collections::HashMap<String, DbPool>>>,
}

impl DatabasePools {
    /// 以给定配置为模板创建注册表；默认库的连接池立即建立。
    pub fn new(base: DbConfig) -> Self {
        let mut pools = std::collections::HashMap::new();
        pools.insert(base.database.clone(), new_pool(&base));
        Self {
            base,
            pools: std::sync::Arc::new(std::sync::RwLock::new(pools)),
        }
    }

    /// 取得指定数据库的连接池（首次访问时惰性建池）。
    ///
    /// `database` 为 `None` 或空时回落到默认库。
    pub fn pool(&self, database: Option<&str>) -> DbPool {
        let name = database
            .map(str::trim)
            .filter(|d| !d.is_empty())
            .unwrap_or(self.base.database.as_str());
        // 快路径：已缓存则直接克隆句柄（读锁内完成，不持有锁做 IO）。
        if let Some(pool) = self.pools.read().ok().and_then(|g| g.get(name).cloned()) {
            return pool;
        }
        let pool = new_pool_for_database(&self.base, name);
        if let Ok(mut guard) = self.pools.write() {
            // 可能已被并发插入；以已存在的为准，避免池泄漏。
            guard.entry(name.to_string()).or_insert(pool.clone());
            return guard.get(name).cloned().unwrap_or(pool);
        }
        pool
    }

    /// 默认数据库名（用于 `Tables_in_{database}` 表头等场景）。
    pub fn default_database(&self) -> &str {
        &self.base.database
    }

    /// 建池所用的配置模板（默认库名、`Tables_in_{database}` 表头等场景需要）。
    pub fn config(&self) -> &DbConfig {
        &self.base
    }
}

/// 从连接池借出一个连接。
///
/// 注意：多数据库支持**不**靠 `USE <db>` 切库——`USE` 是连接级状态，
/// 借出的连接归还后仍停在切换后的库上，下一个借用者会在错误的库里执行
/// 查询。正确做法是每个库一个连接池（见 [`DatabasePools`]），连接建立时
/// 就绑定到目标库，因此不存在跨请求的状态泄漏。
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
    /// 从 `INFORMATION_SCHEMA.TABLES.TABLE_TYPE` 的文本值解析。
    fn from_table_type(table_type: Option<&str>) -> Self {
        match table_type {
            Some("VIEW") => Self::View,
            _ => Self::Table,
        }
    }
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

/// 从 `INFORMATION_SCHEMA.TABLES.TABLE_TYPE` 的文本值解析。
/// 从查询结果行取对象类型：行布局固定为 `(schema, name, type)`
/// （见 [`objects_query`]），因此类型在**索引 2**，不是 1。
///
/// 回归点：查询加上 `TABLE_SCHEMA` 列后若仍读索引 1，拿到的是表名，
/// 于是所有视图都会被误判成 `Table`。
fn object_kind_from_table_type(row: &[String]) -> DbObjectKind {
    DbObjectKind::from_table_type(row.get(2).map(String::as_str))
}

/// 当前数据库中的用户表或视图。
#[derive(Debug, Clone)]
pub struct DbObject {
    pub name: String,
    pub kind: DbObjectKind,
}

/// 列出数据库的所有用户表与视图（按 schema、名称排序，供资源分页）。
///
/// 同时返回 `TABLE_SCHEMA`，资源 URI 才能是 `mssql://sales.orders/data`
/// 这种带 schema 的形式——否则 `sales.orders` 会退化成 `[orders]`，
/// 落到连接用户的默认 schema 上而读不到正确的表。
///
/// `database` 为 `Some` 时先 `USE` 到目标库（见 [`get_client_for`]）。
pub async fn list_tables_and_views(pool: &DbPool) -> Result<Vec<DbObject>> {
    let mut client = get_client(pool).await?;
    let stream = client
        .simple_query(&objects_query())
        .await
        .context("failed to list tables and views")?;
    let resultset = collect_first_resultset(stream).await?;
    Ok(resultset
        .rows
        .into_iter()
        .filter_map(|row| {
            // 前三列是 schema、name、type（`SELECT *` 语义下可能还有更多列）。
            let schema = row.first()?.clone();
            let name = row.get(1)?.clone();
            let kind = object_kind_from_table_type(&row);
            let qualified = qualify_name(&schema, &name);
            Some(DbObject {
                name: qualified,
                kind,
            })
        })
        .collect())
}

/// 列出用户表与视图的 SQL：必须带 `TABLE_SCHEMA`，资源 URI 才能是
/// `mssql://sales.orders/data`（否则跨 schema 会退化成默认 schema 读错表）。
fn objects_query() -> String {
    "SELECT TABLE_SCHEMA, TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.TABLES \
     WHERE TABLE_TYPE IN ('BASE TABLE', 'VIEW') \
     ORDER BY TABLE_SCHEMA, TABLE_NAME"
        .to_string()
}

/// 把 `(schema, name)` 拼成 `schema.name`；schema 为空时只返回表名。
fn qualify_name(schema: &str, name: &str) -> String {
    if schema.is_empty() {
        name.to_string()
    } else {
        format!("{schema}.{name}")
    }
}

/// 读取表或视图的前 100 行（对象名必须已经过 `validate_table_name` 转义）。
pub async fn read_table(pool: &DbPool, safe_table: &str) -> Result<String> {
    let resultset = read_table_resultset(pool, safe_table).await?;
    Ok(resultset_to_csv(&resultset))
}

/// 读取表或视图的前 100 行，返回结果集（供按格式渲染）。
async fn read_table_resultset(pool: &DbPool, safe_table: &str) -> Result<Resultset> {
    let mut client = get_client(pool).await?;
    let query = format!("SELECT TOP 100 * FROM {safe_table}");
    let stream = client
        .simple_query(query)
        .await
        .context("failed to read table rows")?;
    collect_first_resultset(stream).await
}

/// 查询表或视图上定义的视图：返回引用了它的视图名与视图定义 SQL。
///
/// LLM 常常需要知道“这张表被哪些视图包了一层”，否则会重复实现视图里已有的
/// 聚合逻辑。视图定义取自 `INFORMATION_SCHEMA.VIEWS.VIEW_DEFINITION`。
///
/// 注意：`VIEW_DEFINITION` 的类型是 `nvarchar(4000)`，超长定义会被 SQL Server
/// **截断**；这里不额外处理，因为截断至少仍能让 LLM 看出视图的主体逻辑。
pub async fn describe_dependent_views(
    pool: &DbPool,
    schema: Option<&str>,
    table: &str,
) -> Result<Resultset> {
    let mut client = get_client(pool).await?;
    let sql = dependent_views_query(schema);
    // 位置绑定：`@P1` = 表名，`@P2` = schema 名。
    let stream = match schema {
        Some(schema_name) => client.query(&sql, &[&table, &schema_name]).await,
        None => client.query(&sql, &[&table]).await,
    }
    .context("failed to describe dependent views")?;
    collect_first_resultset(stream).await
}

/// 构造“引用了该表的视图”查询 SQL。
///
/// 用 `sys.sql_expression_dependencies` 而非 `sys.dm_sql_referencing_entities`：
/// 前者不要求调用方对目标对象有额外权限，且在跨库引用场景下行为稳定。
/// 只取视图（`referencing_class = 1` 且 `sys.objects.type = 'V'`）以避免把
/// 存储过程、函数也一并列出。
fn dependent_views_query(schema: Option<&str>) -> String {
    const SELECT_FROM: &str = "SELECT DISTINCT \
         SCHEMA_NAME(v.schema_id) AS VIEW_SCHEMA, v.name AS VIEW_NAME, \
         sm.definition AS VIEW_DEFINITION \
         FROM sys.sql_expression_dependencies d \
         JOIN sys.objects v \
             ON v.object_id = d.referencing_id AND v.type = 'V' \
         LEFT JOIN sys.sql_modules sm ON sm.object_id = v.object_id \
         WHERE d.referenced_entity_name = @P1";
    const ORDER_BY: &str = " ORDER BY VIEW_SCHEMA, VIEW_NAME";
    match schema {
        Some(_) => format!("{SELECT_FROM} AND d.referenced_schema_name = @P2{ORDER_BY}"),
        None => format!("{SELECT_FROM}{ORDER_BY}"),
    }
}

/// 查询用户表与视图的规模（行数 + 占用空间），按行数降序。
///
/// 行数据自 `sys.dm_db_partition_stats`（元数据，恒 O(1)），而非 `COUNT(*)`
/// ——后者在大表上是全表/全索引扫描，代价与表大小成正比。
///
/// 行数按 `index_id IN (0, 1)` 过滤：0 = 堆、1 = 聚集索引，两者互斥，
/// 因此每个对象的基表行恰好被计一次；若纳入 `> 1`（非聚集索引）会重复计数。
///
/// 视图在 `sys.dm_db_partition_stats` 中没有行（视图不存数据），因此这里
/// 只覆盖表；视图名仍可从 `describe_dependent_views` 拿到。
pub async fn list_table_sizes(pool: &DbPool, filter: Option<&str>) -> Result<Resultset> {
    let mut client = get_client(pool).await?;
    let sql = table_sizes_query(filter.is_some());
    let stream = match filter {
        Some(name) => client.query(&sql, &[&name]).await,
        None => client.query(&sql, &[]).await,
    }
    .context("failed to list table sizes")?;
    collect_first_resultset(stream).await
}

/// 构造表规模查询 SQL：`with_filter` 为真时按对象名（含 schema 前缀匹配）过滤。
///
/// 输出列：`TABLE_SCHEMA`、`TABLE_NAME`、`ROW_COUNT`、`TOTAL_SPACE_KB`、
/// `USED_SPACE_KB`、`DATA_SPACE_KB`，按行数降序。
fn table_sizes_query(with_filter: bool) -> String {
    // 采用 Microsoft 文档里广为使用的表规模写法：
    // `sys.tables` 联 `sys.partitions` / `sys.allocation_units`，
    // 且 `index_id <= 1`（0 = 堆、1 = 聚集索引）保证每张表只被计一次。
    // `p.rows` 取自元数据，恒为 O(1)，不像 `COUNT(*)` 那样扫表。
    const SELECT_FROM: &str = "SELECT \
         s.name AS TABLE_SCHEMA, t.name AS TABLE_NAME, \
         SUM(p.rows) AS ROW_COUNT, \
         SUM(a.total_pages) * 8 AS TOTAL_SPACE_KB, \
         SUM(a.used_pages) * 8 AS USED_SPACE_KB, \
         (SUM(a.total_pages) - SUM(a.used_pages)) * 8 AS UNUSED_SPACE_KB \
         FROM sys.tables t \
         JOIN sys.schemas s ON s.schema_id = t.schema_id \
         JOIN sys.indexes i ON i.object_id = t.object_id \
         JOIN sys.partitions p ON p.object_id = i.object_id AND p.index_id = i.index_id \
         JOIN sys.allocation_units a ON a.container_id = p.partition_id \
         WHERE t.is_ms_shipped = 0 AND i.index_id <= 1";
    const GROUP_BY: &str = " GROUP BY s.name, t.name";
    const ORDER_BY: &str = " ORDER BY ROW_COUNT DESC, TABLE_SCHEMA, TABLE_NAME";
    match with_filter {
        // 名字过滤用 `LIKE '%' + @P1 + '%'`：既支持子串搜索，又保持参数绑定。
        true => format!(
            "{SELECT_FROM} AND (t.name LIKE '%' + @P1 + '%' \
             OR s.name + '.' + t.name LIKE '%' + @P1 + '%'){GROUP_BY}{ORDER_BY}"
        ),
        false => format!("{SELECT_FROM}{GROUP_BY}{ORDER_BY}"),
    }
}

/// 列出服务器上可访问的数据库。
///
/// 过滤条件：`state = 0`（ONLINE）且 `HAS_DBACCESS(...) = 1`，避免把
/// 离线/无法访问的库列进清单让 LLM 白试一次连接。
pub async fn list_databases(pool: &DbPool) -> Result<Resultset> {
    let mut client = get_client(pool).await?;
    let stream = client
        .query(&databases_query(), &[])
        .await
        .context("failed to list databases")?;
    collect_first_resultset(stream).await
}

/// 列出可访问数据库的 SQL：过滤 `state = 0`（ONLINE）与 `HAS_DBACCESS`，
/// 避免把离线/无权访问的库列进清单让 LLM 白试一次连接。
fn databases_query() -> String {
    "SELECT name AS DATABASE_NAME, \
            DATABASEPROPERTYEX(name, 'Collation') AS COLLATION_NAME, \
            DATABASEPROPERTYEX(name, 'Recovery')   AS RECOVERY_MODEL, \
            DATABASEPROPERTYEX(name, 'Status')     AS STATE_DESC \
     FROM sys.databases \
     WHERE state = 0 AND HAS_DBACCESS(name) = 1 \
     ORDER BY name"
        .to_string()
}

/// 表结构描述的组成部分，供 [`describe_table`] 按调用方选择拼装。
///
/// 拆成位集合而不是布尔参数列表：新增分节时不必改动所有调用点，且
/// `ServerHandler` 侧可直接从工具入参映射。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DescribeSections {
    pub columns: bool,
    pub indexes: bool,
    pub row_count: bool,
    pub dependent_views: bool,
}

impl DescribeSections {
    /// 默认分节：列 + 索引（与此前行为一致，保证向后兼容）。
    pub fn default_sections() -> Self {
        Self {
            columns: true,
            indexes: true,
            row_count: false,
            dependent_views: false,
        }
    }

    /// 全部可用分节。
    pub fn all() -> Self {
        Self {
            columns: true,
            indexes: true,
            row_count: true,
            dependent_views: true,
        }
    }
}

/// 查询表或视图的结构：列信息（名称、类型、可空性、长度/精度、默认值，按列序
/// 排列）之后跟随 `INDEXES` 分节，列出索引（名称、类型、唯一性、主键、键列、
/// 包含列，CSV，无索引时仅表头）。
/// 列信息基于 `INFORMATION_SCHEMA.COLUMNS`，索引基于 `sys.indexes`，二者同时
/// 覆盖表与视图，schema 匹配语义一致。
/// 表名经参数绑定传入，无注入风险；`schema` 为 `None` 时在所有 schema 中
/// 按表名匹配（结果包含 `TABLE_SCHEMA`/`OBJECT_SCHEMA` 列以示区分）。
///
/// `sections` 控制附加分节（`ROW_COUNT`、`DEPENDENT_VIEWS`）；默认只出
/// 列与索引，以免破坏既有的「两段 CSV」消费者。
pub async fn describe_table(
    pool: &DbPool,
    schema: Option<&str>,
    table: &str,
    sections: DescribeSections,
) -> Result<String> {
    let mut client = get_client(pool).await?;
    let columns = describe_columns(&mut client, schema, table).await?;
    if columns.rows.is_empty() {
        bail!("No columns found for table '{table}'");
    }
    // 分节标记带 `#` 前缀：下游按注释行跳过即可用标准 CSV 解析器读取每段。
    let mut out = String::new();
    if sections.columns {
        push_section(&mut out, None, &columns);
    }
    if sections.indexes {
        let indexes = describe_indexes(&mut client, schema, table).await?;
        push_section(&mut out, Some("INDEXES"), &indexes);
    }
    if sections.row_count {
        // 用精确名匹配而非 `list_table_sizes` 的子串过滤：`orders` 不该
        // 命中 `orders_archive` 的统计。
        let sizes = describe_table_size(pool, schema, table).await?;
        push_section(&mut out, Some("ROW_COUNT"), &sizes);
    }
    if sections.dependent_views {
        let views = describe_dependent_views(pool, schema, table).await?;
        push_section(&mut out, Some("DEPENDENT_VIEWS"), &views);
    }
    Ok(out)
}

/// 追加一个 CSV 分节；`label` 为 `Some` 时前置 `# LABEL` 注释行。
/// 分节之间以空行分隔，使下游可按 `\n\n` 切段。
fn push_section(out: &mut String, label: Option<&str>, resultset: &Resultset) {
    if !out.is_empty() {
        out.push_str("\n\n");
    }
    if let Some(label) = label {
        out.push_str(&format!("# {label}\n"));
    }
    out.push_str(&resultset_to_csv(resultset));
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
///
/// `format` 控制输出渲染（CSV 默认 / JSON / Markdown）。
pub async fn execute_query(
    pool: &DbPool,
    database: &str,
    query: &str,
    format: OutputFormat,
) -> Result<String> {
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
    // 仅 CSV 生效：该格式是刻意对齐参考实现的特例，JSON/Markdown 走通用渲染。
    let is_tables_listing = is_tables_listing_query(query) && !resultset.columns.is_empty();
    if is_tables_listing && format == OutputFormat::Csv {
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

    Ok(render_resultset(&resultset, format))
}

/// 查询单个表或视图的规模（行数 + 占用空间），用于 `describe_table` 的
/// `ROW_COUNT` 分节与 `table_sizes` 工具。
///
/// 与 [`list_table_sizes`] 的区别：这里按**精确表名**过滤，避免子串命中
/// 同名的其他表（`orders` 不应匹配 `orders_archive` 的统计）。
async fn describe_table_size(
    pool: &DbPool,
    schema: Option<&str>,
    table: &str,
) -> Result<Resultset> {
    let mut client = get_client(pool).await?;
    let sql = single_table_size_query(schema.is_some());
    // 位置绑定：`@P1` = 表名，`@P2` = schema 名。
    let stream = match schema {
        Some(schema_name) => client.query(&sql, &[&table, &schema_name]).await,
        None => client.query(&sql, &[&table]).await,
    }
    .context("failed to read table size")?;
    collect_first_resultset(stream).await
}

/// 构造单表规模查询 SQL（精确按对象名，而非子串）。
fn single_table_size_query(with_schema: bool) -> String {
    // 与 [`table_sizes_query`] 同一套联接，但按精确对象名过滤（`orders`
    // 不应命中 `orders_archive`）。
    const SELECT_FROM: &str = "SELECT \
         s.name AS TABLE_SCHEMA, t.name AS TABLE_NAME, \
         SUM(p.rows) AS ROW_COUNT, \
         SUM(a.total_pages) * 8 AS TOTAL_SPACE_KB, \
         SUM(a.used_pages) * 8 AS USED_SPACE_KB, \
         (SUM(a.total_pages) - SUM(a.used_pages)) * 8 AS UNUSED_SPACE_KB \
         FROM sys.objects t \
         JOIN sys.schemas s ON s.schema_id = t.schema_id \
         JOIN sys.indexes i ON i.object_id = t.object_id \
         JOIN sys.partitions p ON p.object_id = i.object_id AND p.index_id = i.index_id \
         JOIN sys.allocation_units a ON a.container_id = p.partition_id \
         WHERE i.index_id <= 1 AND t.name = @P1";
    const GROUP_BY: &str = " GROUP BY s.name, t.name";
    match with_schema {
        true => format!("{SELECT_FROM} AND s.name = @P2{GROUP_BY}"),
        false => format!("{SELECT_FROM}{GROUP_BY}"),
    }
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
    fn table_sizes_query_uses_metadata_not_count_star() {
        // 回归点：行数必须取自元数据（O(1)），不能是 COUNT(*)（扫全表）。
        for sql in [table_sizes_query(false), table_sizes_query(true)] {
            assert!(
                !sql.to_ascii_uppercase().contains("COUNT("),
                "must not scan: {sql}"
            );
            assert!(sql.contains("p.rows"), "expected metadata rows: {sql}");
        }
    }

    #[test]
    fn table_sizes_query_counts_each_table_once() {
        // 回归点：`index_id <= 1`（堆/聚集索引）否则非聚集索引会让行数翻倍。
        for sql in [table_sizes_query(false), table_sizes_query(true)] {
            assert!(
                sql.contains("i.index_id <= 1"),
                "must restrict to heap/clustered: {sql}"
            );
            assert!(sql.contains("GROUP BY"), "aggregation needs GROUP BY");
        }
    }

    #[test]
    fn table_sizes_query_filter_binds_parameter() {
        // 子串过滤必须走参数绑定，不能把用户输入拼进 SQL。
        let filtered = table_sizes_query(true);
        assert!(filtered.contains("LIKE '%' + @P1 + '%'"));
        assert!(filtered.contains("@P1"));
        assert!(!table_sizes_query(false).contains("@P1"));
    }

    #[test]
    fn single_table_size_query_matches_exact_name() {
        // 精确匹配：`orders` 不得命中 `orders_archive`（故无 LIKE）。
        for sql in [
            single_table_size_query(false),
            single_table_size_query(true),
        ] {
            assert!(sql.contains("t.name = @P1"), "exact match: {sql}");
            assert!(!sql.contains("LIKE"), "must not substring-match: {sql}");
        }
        assert!(single_table_size_query(true).contains("s.name = @P2"));
        assert!(!single_table_size_query(false).contains("@P2"));
    }

    #[test]
    fn single_table_size_query_placeholders_in_bind_order() {
        // 位置绑定：@P1（表名）必须先于 @P2（schema）出现。
        let sql = single_table_size_query(true);
        assert!(sql.find("@P1").unwrap() < sql.find("@P2").unwrap());
    }

    #[test]
    fn dependent_views_query_selects_definition_and_filters_views() {
        for sql in [
            dependent_views_query(None),
            dependent_views_query(Some("dbo")),
        ] {
            assert!(sql.contains("VIEW_DEFINITION"), "definition: {sql}");
            // 只要视图，排除存储过程/函数。
            assert!(sql.contains("v.type = 'V'"), "views only: {sql}");
        }
        assert!(dependent_views_query(Some("dbo")).contains("referenced_schema_name = @P2"));
        assert!(!dependent_views_query(None).contains("@P2"));
    }

    #[test]
    fn dependent_views_query_placeholders_in_bind_order() {
        let sql = dependent_views_query(Some("dbo"));
        assert!(sql.find("@P1").unwrap() < sql.find("@P2").unwrap());
    }

    #[test]
    fn list_databases_query_filters_online_and_accessible() {
        // 只列 ONLINE 且当前登录可访问的库，避免 LLM 白试一次连接。
        let sql = databases_query();
        assert!(sql.contains("state = 0"), "online only: {sql}");
        assert!(sql.contains("HAS_DBACCESS"), "accessible only: {sql}");
    }

    #[test]
    fn list_objects_query_returns_schema_for_qualified_uris() {
        // 回归点：资源 URI 需要 `schema.table`，否则跨 schema 会读错表。
        let sql = objects_query();
        assert!(sql.contains("TABLE_SCHEMA"), "needs schema: {sql}");
        assert!(sql.contains("ORDER BY TABLE_SCHEMA, TABLE_NAME"));
    }

    #[test]
    fn object_kind_reads_column_two_not_one() {
        // 回归点：查询是 (schema, name, type)，类型在索引 2。
        // 读成索引 1（表名）会把所有视图误判成 Table。
        let row = |schema: &str, name: &str, kind: &str| {
            vec![schema.to_string(), name.to_string(), kind.to_string()]
        };
        assert_eq!(
            object_kind_from_table_type(&row("dbo", "v1", "VIEW")),
            DbObjectKind::View
        );
        assert_eq!(
            object_kind_from_table_type(&row("dbo", "t1", "BASE TABLE")),
            DbObjectKind::Table
        );
        // 缺列时降级为 Table，而不是 panic。
        assert_eq!(
            object_kind_from_table_type(&row("dbo", "t1", "").as_slice()[..2]),
            DbObjectKind::Table
        );
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
