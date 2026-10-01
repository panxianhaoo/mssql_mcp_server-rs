//! 需要真实 SQL Server 连接的集成测试。
//!
//! 夹具（表、索引、数据）由每个测试用**专属名称**自行创建并各自插入数据，
//! 因此测试之间互无耦合、可安全并行，CI 里也无需预先准备 schema。
//!
//! 默认全部 `#[ignore]`。本地先用仓库根目录的 `docker-compose.yml` 起实例：
//!
//! ```bash
//! docker compose up -d --wait
//! MSSQL_SERVER=localhost MSSQL_PORT=14333 \
//! MSSQL_USER=sa MSSQL_PASSWORD='YourStrong!Passw0rd' MSSQL_DATABASE=master \
//!   cargo test --test integration -- --ignored
//! docker compose down
//! ```
//!
//! 注意 DDL/data seeding 直接走 `db` 层的连接池，绕过了 `execute_sql` 的
//! 只读守卫——守卫本身另有专门的用例验证。

use std::collections::BTreeSet;

use mssql_mcp_server_rs::config::DbConfig;
use mssql_mcp_server_rs::db::{
    DbPool, describe_table, execute_query, list_tables_and_views, new_pool, read_table,
};
use mssql_mcp_server_rs::resultset::collect_first_resultset;
use mssql_mcp_server_rs::sql::{is_read_only_query, validate_table_name};

/// 从环境变量构建连接池；缺少配置时 panic 并给出清晰指引。
fn test_pool() -> DbPool {
    let config = DbConfig::from_env().expect(
        "integration tests need MSSQL_SERVER/MSSQL_DATABASE/MSSQL_USER/MSSQL_PASSWORD \
         (see tests/integration.rs docs)",
    );
    new_pool(&config)
}

/// 直接执行一条 SQL batch 并消费其结果集（用于建表/插数，绕过只读守卫）。
///
/// `simple_query` 返回的流必须消费到末尾，否则后续复用该连接会读到残留数据。
async fn exec(pool: &DbPool, sql: &str) {
    let mut client = pool
        .get()
        .await
        .expect("connection for fixture setup failed");
    let stream = client
        .simple_query(sql)
        .await
        .unwrap_or_else(|e| panic!("fixture SQL failed: {sql}\nerror: {e:#}"));
    collect_first_resultset(stream)
        .await
        .expect("consuming fixture resultset failed");
}

/// 拆固件：带有各类索引的表，以及一行含“逗号/引号/换行”的值。
///
/// `tag` 用于隔离并发执行的用例（每个测试传自己的标识）。
struct Fixture {
    table: String,
    view: String,
}

impl Fixture {
    /// 表名在 `mcp_it_{tag}` 基础上派生，确保仅含合法标识符字符。
    fn new(tag: &str) -> Self {
        assert!(
            tag.chars().all(|c| c.is_ascii_alphanumeric()),
            "tag must be alphanumeric: {tag}"
        );
        Self {
            table: format!("mcp_it_{tag}"),
            view: format!("mcp_it_{tag}_v"),
        }
    }
}

/// 创建夹具：复合主键 + 带 INCLUDE 列的索引 + 一行特殊字符数据。
///
/// 复合主键正是验证「索引列必须各自成行」的关键——若用 `STRING_AGG`
/// 拼成一个字段，其中的逗号会撕裂 CSV。
async fn setup_fixture(pool: &DbPool, tag: &str) -> Fixture {
    let fx = Fixture::new(tag);
    exec(
        pool,
        &format!(
            "IF OBJECT_ID('dbo.{}','U') IS NULL \
             CREATE TABLE dbo.{} (\
               id       INT NOT NULL, \
               region   NVARCHAR(20) NOT NULL, \
               customer NVARCHAR(50) NULL, \
               amount   DECIMAL(10,2) NULL, \
               CONSTRAINT pk_{} PRIMARY KEY (id, region))",
            fx.table, fx.table, tag
        ),
    )
    .await;
    exec(
        pool,
        &format!(
            "IF NOT EXISTS (SELECT 1 FROM sys.indexes WHERE name = 'ix_{tag}') \
             CREATE INDEX ix_{tag} ON dbo.{} (customer) INCLUDE (amount)",
            fx.table
        ),
    )
    .await;
    // CREATE VIEW 必须是 batch 的唯一语句，故单独执行；DROP ... IF EXISTS 保证幂等。
    exec(pool, &format!("DROP VIEW IF EXISTS dbo.{}", fx.view)).await;
    exec(
        pool,
        &format!(
            "CREATE VIEW dbo.{} AS SELECT id, region FROM dbo.{}",
            fx.view, fx.table
        ),
    )
    .await;
    exec(
        pool,
        // 先清空：上一轮若中途 panic 而未走到 teardown，残留数据会撞主键约束。
        &format!(
            "DELETE FROM dbo.{}; \
             INSERT INTO dbo.{} (id, region, customer, amount) VALUES \
             (1, N'East', N'Alice, Inc', 10.50), (2, N'West', N'Bob \"the builder\"', 20.00)",
            fx.table, fx.table
        ),
    )
    .await;
    fx
}

/// 释放夹具（视图须先于表删除）。
async fn teardown_fixture(pool: &DbPool, fx: &Fixture) {
    exec(pool, &format!("DROP VIEW IF EXISTS dbo.{}", fx.view)).await;
    exec(pool, &format!("DROP TABLE IF EXISTS dbo.{}", fx.table)).await;
}

/// 用真正的 CSV 解析器按 `\n` 分段解析，返回各段的行。
/// `describe_table` 返回的是**两段** CSV，中间以 `# INDEXES` 注释行分隔。
/// 注释行不是数据，按 `#` 前缀跳过，使每段都能用标准解析器校验列对齐。
fn parse_csv(text: &str) -> Vec<Vec<Vec<String>>> {
    text.split("\n\n")
        .map(|section| {
            let without_comments = section
                .lines()
                .filter(|l| !l.trim_start().starts_with('#'))
                .collect::<Vec<_>>()
                .join("\n");
            csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(without_comments.as_bytes())
                .records()
                .map(|r| r.expect("CSV record").iter().map(str::to_string).collect())
                .collect()
        })
        .collect()
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn connection_pool_serves_simple_query() {
    let pool = test_pool();
    let out = execute_query(&pool, "master", "SELECT 42 AS answer")
        .await
        .expect("SELECT must succeed against a live server");
    let sections = parse_csv(&out);
    assert_eq!(sections[0][0], vec!["answer"]);
    assert!(out.lines().any(|l| l == "42"), "unexpected output: {out}");
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn csv_output_quotes_values_containing_delimiters() {
    // 回归点：含逗号/引号的值必须被引号包裹，否则下游解析出的列数会变化。
    let pool = test_pool();
    let fx = setup_fixture(&pool, "csvquote").await;
    let query = format!("SELECT customer FROM dbo.{} ORDER BY id", fx.table);
    assert!(is_read_only_query(&query), "guard must accept this SELECT");
    let out = execute_query(&pool, "master", &query)
        .await
        .expect("query must succeed");
    let rows = &parse_csv(&out)[0];
    // 每个数据行都必须是恰好 1 列（其值内含逗号/引号，已被完整引用）。
    assert_eq!(rows.len(), 3, "expect header + 2 rows: {out}");
    for row in &rows[1..] {
        assert_eq!(row.len(), 1, "value must stay in one column: {row:?}");
    }
    let first = &rows[1][0];
    assert_eq!(first, "Alice, Inc", "CSV parser should restore the comma");
    assert!(
        rows[2][0].contains("\"the builder\""),
        "quotes preserved: {}",
        rows[2][0]
    );
    teardown_fixture(&pool, &fx).await;
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn describe_table_lists_composite_key_columns_as_separate_rows() {
    // 复合主键 (id, region) 必须输出两行，而不是 STRING_AGG 拼成的一行 "id,region"。
    let pool = test_pool();
    let fx = setup_fixture(&pool, "composite").await;
    let out = describe_table(&pool, Some("dbo"), &fx.table)
        .await
        .expect("describe_table must succeed");
    let sections = parse_csv(&out);
    assert_eq!(sections.len(), 2, "expected columns + INDEXES sections");
    let index_rows = &sections[1];
    let header = &index_rows[0];
    assert!(
        header.contains(&"COLUMN_NAME".to_string()),
        "header: {header:?}"
    );
    // 主键 idx 的键列各占一行：找所有 IS_PRIMARY_KEY=YES 的行，取 COLUMN_NAME。
    let pk_pos = header
        .iter()
        .position(|h| h == "IS_PRIMARY_KEY")
        .expect("IS_PRIMARY_KEY column");
    let col_pos = header
        .iter()
        .position(|h| h == "COLUMN_NAME")
        .expect("COLUMN_NAME column");
    let pk_columns: Vec<&String> = index_rows[1..]
        .iter()
        .filter(|r| r[pk_pos] == "YES")
        .map(|r| &r[col_pos])
        .collect();
    assert_eq!(
        pk_columns,
        vec!["id", "region"],
        "composite key must be one row per column: {pk_columns:?}"
    );
    // 每个索引行的列数都必须对齐（证明没有任何字段被逗号撕裂）。
    for (i, row) in index_rows.iter().enumerate() {
        assert_eq!(row.len(), header.len(), "row {i} misaligned: {row:?}");
    }
    teardown_fixture(&pool, &fx).await;
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn describe_table_includes_included_columns_as_rows() {
    // INCLUDE (amount) 应作为独立行出现，且 IS_INCLUDED_COLUMN=YES。
    let pool = test_pool();
    let fx = setup_fixture(&pool, "include").await;
    let out = describe_table(&pool, None, &fx.table)
        .await
        .expect("describe_table must succeed");
    let rows = &parse_csv(&out)[1];
    let header = &rows[0];
    let incl_pos = header
        .iter()
        .position(|h| h == "IS_INCLUDED_COLUMN")
        .expect("IS_INCLUDED_COLUMN column");
    let col_pos = header
        .iter()
        .position(|h| h == "COLUMN_NAME")
        .expect("COLUMN_NAME column");
    let included: Vec<&String> = rows[1..]
        .iter()
        .filter(|r| r[incl_pos] == "YES")
        .map(|r| &r[col_pos])
        .collect();
    assert_eq!(
        included,
        vec!["amount"],
        "unexpected included columns: {included:?}"
    );
    teardown_fixture(&pool, &fx).await;
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn describe_table_works_for_views() {
    let pool = test_pool();
    let fx = setup_fixture(&pool, "viewprobe").await;
    let out = describe_table(&pool, Some("dbo"), &fx.view)
        .await
        .expect("describe_table must handle views");
    let rows = &parse_csv(&out)[0];
    // 视图的列：id、region（顺序同 SELECT 列表）。
    let cols: Vec<&str> = rows[1..].iter().map(|r| r[1].as_str()).collect();
    assert_eq!(
        cols,
        vec!["id", "region"],
        "unexpected view columns: {cols:?}"
    );
    teardown_fixture(&pool, &fx).await;
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn describe_table_reports_per_column_collation() {
    // 回归点：SQL Server 允许列级 collation，同一张表可混用不同规则。
    // 缺了 COLLATION_NAME 就无法发现这种差异（会影响中文排序与 JOIN 时的
    // collation conflict）。
    let pool = test_pool();
    let table = "mcp_it_collation";
    exec(&pool, &format!("DROP TABLE IF EXISTS dbo.{table}")).await;
    exec(
        &pool,
        &format!(
            "CREATE TABLE dbo.{table} (\
               latin_col  VARCHAR(20)  COLLATE Latin1_General_CI_AS NULL, \
               chinese_col NVARCHAR(20) COLLATE Chinese_PRC_90_CI_AS NULL, \
               plain_int  INT NULL)"
        ),
    )
    .await;

    let out = describe_table(&pool, Some("dbo"), table)
        .await
        .expect("describe_table must succeed");
    let rows = &parse_csv(&out)[0];
    let header = &rows[0];
    let collation_pos = header
        .iter()
        .position(|h| h == "COLLATION_NAME")
        .expect("COLLATION_NAME must be present in header");
    // COMMENTS ON COLUMN order: 列必须在最末，避免打乱既有列的位置。
    assert_eq!(
        collation_pos,
        header.len() - 1,
        "COLLATION_NAME should be the last column: {header:?}"
    );
    let name_pos = header
        .iter()
        .position(|h| h == "COLUMN_NAME")
        .expect("COLUMN_NAME column");
    let find = |name: &str| -> &str {
        rows[1..]
            .iter()
            .find(|r| r[name_pos] == name)
            .unwrap_or_else(|| panic!("column {name} missing: {rows:?}"))[collation_pos]
            .as_str()
    };
    // 各列各自的 collation 必须如实反映，而非统一返回库级默认值。
    assert_eq!(find("latin_col"), "Latin1_General_CI_AS");
    assert_eq!(find("chinese_col"), "Chinese_PRC_90_CI_AS");
    // 非字符类型无 collation。
    assert_eq!(find("plain_int"), "NULL");

    exec(&pool, &format!("DROP TABLE IF EXISTS dbo.{table}")).await;
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn describe_table_rejects_unknown_table() {
    let pool = test_pool();
    let err = describe_table(&pool, Some("dbo"), "no_such_table_exists_here")
        .await
        .expect_err("unknown table must be reported as an error");
    assert!(
        format!("{err:#}").contains("No columns found"),
        "unexpected error: {err:#}"
    );
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn read_table_returns_csv_header_and_rows() {
    let pool = test_pool();
    let fx = setup_fixture(&pool, "readrows").await;
    let safe = validate_table_name(&fx.table).expect("fixture name is a valid identifier");
    let out = read_table(&pool, &safe)
        .await
        .expect("reading a table must succeed");
    let rows = &parse_csv(&out)[0];
    assert!(rows.len() >= 3, "expected header + rows: {out}");
    assert_eq!(rows[0].len(), rows[1].len(), "header/row column mismatch");
    teardown_fixture(&pool, &fx).await;
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn list_tables_and_views_reports_both_kinds() {
    let pool = test_pool();
    let fx = setup_fixture(&pool, "listkinds").await;
    let objects = list_tables_and_views(&pool)
        .await
        .expect("listing must succeed");
    let table_names: BTreeSet<&str> = objects.iter().map(|o| o.name.as_str()).collect();
    assert!(
        table_names.contains(fx.table.as_str()),
        "fixture table missing from listing: {table_names:?}"
    );
    assert!(
        table_names.contains(fx.view.as_str()),
        "fixture view missing from listing: {table_names:?}"
    );
    let view = objects
        .iter()
        .find(|o| o.name == fx.view)
        .expect("view row");
    assert_eq!(view.kind.label(), "View");
    let table = objects
        .iter()
        .find(|o| o.name == fx.table)
        .expect("table row");
    assert_eq!(table.kind.label(), "Table");
    teardown_fixture(&pool, &fx).await;
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn list_tables_and_views_names_are_unique() {
    let pool = test_pool();
    let objects = list_tables_and_views(&pool)
        .await
        .expect("listing must succeed");
    assert!(
        !objects.is_empty(),
        "INFORMATION_SCHEMA.TABLES returned nothing"
    );
    assert_eq!(
        objects
            .iter()
            .map(|o| o.name.clone())
            .collect::<BTreeSet<_>>()
            .len(),
        objects.len(),
        "table or view names must be unique"
    );
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn read_only_guard_rejects_writes_before_hitting_server() {
    let pool = test_pool();
    for dangerous in [
        "DROP TABLE mcp_it_guardprobe",
        "INSERT INTO mcp_it_guardprobe VALUES (1)",
        "SELECT 1; DROP TABLE mcp_it_guardprobe",
        "UPDATE mcp_it_guardprobe SET id = 2",
        "TRUNCATE TABLE mcp_it_guardprobe",
    ] {
        let err = execute_query(&pool, "master", dangerous)
            .await
            .expect_err("write statements must be rejected");
        assert!(
            format!("{err:#}").contains("read-only mode"),
            "expected read-only rejection for {dangerous}, got: {err:#}"
        );
    }
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn execute_query_accepts_cte_select() {
    let pool = test_pool();
    let fx = setup_fixture(&pool, "cte").await;
    let query = format!(
        "WITH ranked AS (SELECT id, region FROM dbo.{}) SELECT * FROM ranked ORDER BY id",
        fx.table
    );
    let out = execute_query(&pool, "master", &query)
        .await
        .expect("CTE SELECT must be allowed and succeed");
    assert!(out.contains("id,region"), "expected CSV header: {out}");
    assert!(out.lines().count() >= 3, "expected rows: {out}");
    teardown_fixture(&pool, &fx).await;
}

#[tokio::test]
#[ignore = "requires a live SQL Server"]
async fn tables_listing_query_uses_mysql_style_header() {
    let pool = test_pool();
    let out = execute_query(
        &pool,
        "mydb",
        "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_TYPE = 'BASE TABLE'",
    )
    .await
    .expect("INFORMATION_SCHEMA query must succeed");
    assert!(
        out.lines().next().unwrap_or_default() == "Tables_in_mydb",
        "expected Tables_in_ header, got: {out}"
    );
}
