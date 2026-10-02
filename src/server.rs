//! MCP server：只读工具（`execute_sql`、`describe_table`、`table_sizes`、
//! `list_databases`）、分页的表资源列表（`mssql://{table}/data`）与资源模板。

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourceTemplatesResult, ListResourcesResult,
    PaginatedRequestParams, ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult,
    Resource, ResourceContents, ResourceTemplate, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::config::DbConfig;
use crate::db::{self, DatabasePools, DbPool, DescribeSections};
use crate::format::{OutputFormat, render_resultset};
use crate::sql::{parse_table_name, validate_table_name};

const MSSQL_URI_SCHEME: &str = "mssql://";

/// `resources/list` 每页返回的资源数量上限，避免大目录撑爆响应体。
const RESOURCES_PAGE_SIZE: usize = 500;

/// `execute_sql` 工具的入参。
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ExecuteSqlArgs {
    /// The SQL query to execute (a single SELECT)
    query: String,
    /// Output format: "csv" (default), "json", or "markdown"
    #[serde(default)]
    format: Option<String>,
    /// Optional database to run against; defaults to MSSQL_DATABASE
    #[serde(default)]
    database: Option<String>,
}

/// `describe_table` 工具的入参。
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct DescribeTableArgs {
    /// The table or view name to describe (e.g. "users", "dbo.users", "active_users")
    table: String,
    /// Include approximate row count and space usage (default: false)
    #[serde(default)]
    include_row_count: Option<bool>,
    /// Include views that reference this table, with their definitions (default: false)
    #[serde(default)]
    include_dependent_views: Option<bool>,
    /// Optional database to inspect; defaults to MSSQL_DATABASE
    #[serde(default)]
    database: Option<String>,
}

/// `table_sizes` 工具的入参。
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct TableSizesArgs {
    /// Optional substring filter on table name or "schema.table"
    #[serde(default)]
    table: Option<String>,
    /// Output format: "csv" (default), "json", or "markdown"
    #[serde(default)]
    format: Option<String>,
    /// Optional database to inspect; defaults to MSSQL_DATABASE
    #[serde(default)]
    database: Option<String>,
}

/// `list_databases` 工具的入参。
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ListDatabasesArgs {
    /// Output format: "csv" (default), "json", or "markdown"
    #[serde(default)]
    format: Option<String>,
}

/// 解析工具入参里的输出格式；非法值返回可直接展示给模型的错误。
fn resolve_format(value: Option<&str>) -> Result<OutputFormat, McpError> {
    OutputFormat::parse(value).ok_or_else(|| {
        McpError::invalid_params(
            format!(
                "Invalid format: {}. Expected 'csv', 'json', or 'markdown'",
                value.unwrap_or_default()
            ),
            None,
        )
    })
}

/// 归一化可选的数据库名：空字符串视为未指定。
fn normalize_database(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|d| !d.is_empty())
}

#[derive(Clone)]
pub struct McpServer {
    /// 数据库配置（连接池之外的少量信息，如 `Tables_in_{database}` 表头用的库名）。
    config: Arc<DbConfig>,
    /// 按库名缓存的连接池，支撑多数据库查询。
    ///
    /// 用 `Arc` 包一层是 HTTP 传输的关键：rmcp 的 service_factory 会被反复
    /// 调用（每个新 session 一次，schema 缓存未命中时也会），若每次都新建
    /// [`DatabasePools`]，每个 session 就会各起一套连接池，客户端数目直接
    /// 放大到 SQL Server 的连接数上。共享同一个句柄才能真正池化。
    pools: Arc<DatabasePools>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl McpServer {
    /// 新建服务并连带创建默认库的连接池。
    pub fn new(config: DbConfig) -> Self {
        Self::with_pools(Arc::new(DatabasePools::new(config)))
    }

    /// 复用一组已有的连接池（HTTP 传输下多 session 共享，见字段注释）。
    ///
    /// 此处只 `clone` 句柄内部的 `Arc`，不额外建立任何连接。
    pub fn with_pools(pools: Arc<DatabasePools>) -> Self {
        Self {
            config: Arc::new(pools.config().clone()),
            pools,
            tool_router: Self::tool_router(),
        }
    }

    /// 取目标数据库的连接池（`database` 为 `None` 时用默认库）。
    fn pool(&self, database: Option<&str>) -> DbPool {
        self.pools.pool(database)
    }

    #[tool(
        description = "Execute a read-only SQL query (a single SELECT; WITH ... SELECT is allowed) on the SQL Server. Returns results as CSV (default), JSON, or Markdown.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn execute_sql(
        &self,
        Parameters(args): Parameters<ExecuteSqlArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.query.trim().is_empty() {
            return Err(McpError::invalid_params("Query is required", None));
        }
        let format = resolve_format(args.format.as_deref())?;
        let database = normalize_database(args.database.as_deref());
        let pool = self.pool(database);
        // 数据库错误以文本形式返回（与参考实现一致），便于客户端读到失败原因。
        // 用 is_error 标记让协议层也能区分成败，而不是一律 success。
        let text = match db::execute_query(
            &pool,
            &self.database_name(database),
            &args.query,
            format,
        )
        .await
        {
            Ok(text) => text,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Error executing query: {e:#}"
                ))]));
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Describe the structure of a SQL Server table or view (column names, types, nullability, length/precision, defaults, collation, indexes). Optionally include row counts and dependent views.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn describe_table(
        &self,
        Parameters(args): Parameters<DescribeTableArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.table.trim().is_empty() {
            return Err(McpError::invalid_params("Table name is required", None));
        }
        let (schema, table) = parse_table_name(&args.table)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let database = normalize_database(args.database.as_deref());
        // 从默认分节派生而非重新列举全部字段：日后新增分节时，此处无需改动
        // 也不会漏掉某个 flag。
        let sections = DescribeSections {
            row_count: args.include_row_count.unwrap_or(false),
            dependent_views: args.include_dependent_views.unwrap_or(false),
            ..DescribeSections::default_sections()
        };
        let pool = self.pool(database);
        // 数据库错误以文本形式返回（与 execute_sql 一致），便于客户端读到失败原因。
        let text = match db::describe_table(&pool, schema.as_deref(), &table, sections).await {
            Ok(text) => text,
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Error describing table: {e:#}"
                ))]));
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "Report approximate row counts and disk space usage for user tables, from metadata (no table scan). Optionally filter by table name substring.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn table_sizes(
        &self,
        Parameters(args): Parameters<TableSizesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let format = resolve_format(args.format.as_deref())?;
        let database = normalize_database(args.database.as_deref());
        let pool = self.pool(database);
        let filter = args
            .table
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let text = match db::list_table_sizes(&pool, filter).await {
            Ok(resultset) => render_resultset(&resultset, format),
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Error listing table sizes: {e:#}"
                ))]));
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    #[tool(
        description = "List the databases on this SQL Server that are online and accessible to the current login.",
        annotations(
            read_only_hint = true,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        )
    )]
    async fn list_databases(
        &self,
        Parameters(args): Parameters<ListDatabasesArgs>,
    ) -> Result<CallToolResult, McpError> {
        let format = resolve_format(args.format.as_deref())?;
        let text = match db::list_databases(&self.pool(None)).await {
            Ok(resultset) => render_resultset(&resultset, format),
            Err(e) => {
                return Ok(CallToolResult::error(vec![ContentBlock::text(format!(
                    "Error listing databases: {e:#}"
                ))]));
            }
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(text)]))
    }

    /// `Tables_in_{database}` 表头等场景需要的实际库名。
    fn database_name(&self, database: Option<&str>) -> String {
        database
            .map(str::to_string)
            .unwrap_or_else(|| self.config.database.clone())
    }
}

#[tool_handler(router = self.tool_router.clone())]
impl ServerHandler for McpServer {
    fn get_info(&self) -> ServerConfig {
        ServerConfig::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_server_info(Implementation::new(
            "mssql_mcp_server-rs",
            env!("CARGO_PKG_VERSION"),
        ))
        .with_instructions(
            "MSSQL MCP server: use execute_sql to run read-only queries, \
             describe_table to inspect the structure of a table or view, \
             table_sizes to check row counts and space usage, \
             list_databases to discover databases, \
             or read mssql://{table}/data resources to peek at table or view contents.",
        )
    }

    async fn list_resources(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        let offset = decode_cursor(request.as_ref().and_then(|p| p.cursor.clone()))?;
        // 数据库不可达时返回空列表（与参考实现一致）。
        let objects = match db::list_tables_and_views(&self.pool(None)).await {
            Ok(objects) => objects,
            Err(e) => {
                log::error!("Failed to list resources: {e:#}");
                Vec::new()
            }
        };
        // 游标为起始偏移量：越界视为空页（客户端应回退到首页重新分页）。
        let start = offset.min(objects.len());
        let end = (start + RESOURCES_PAGE_SIZE).min(objects.len());
        let resources = objects[start..end]
            .iter()
            .map(|object| {
                Resource::new(
                    format!("{MSSQL_URI_SCHEME}{}/data", object.name),
                    format!("{}: {}", object.kind.label(), object.name),
                )
                .with_description(format!(
                    "Data in {}: {}",
                    object.kind.label().to_lowercase(),
                    object.name
                ))
                .with_mime_type("text/plain")
            })
            .collect();
        let mut result = ListResourcesResult::with_all_items(resources);
        if end < objects.len() {
            result.next_cursor = Some(end.to_string());
        }
        Ok(result)
    }

    /// 声明 `mssql://{table}/data` 模板：客户端据此补全而非仅能枚举。
    ///
    /// 没有模板时，客户端只能先 `resources/list` 拿到全部具体 URI 才能读；
    /// 有了模板，模型可直接拼出 `mssql://dbo.orders/data`。
    async fn list_resource_templates(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourceTemplatesResult, McpError> {
        let template =
            ResourceTemplate::new(format!("{MSSQL_URI_SCHEME}{{table}}/data"), "table_data")
                .with_description("First 100 rows of a table or view, as CSV")
                .with_mime_type("text/csv");
        Ok(ListResourceTemplatesResult::with_all_items(vec![template]))
    }

    async fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: RequestContext<RoleServer>,
    ) -> Result<ReadResourceResponse, McpError> {
        let uri = request.uri;
        let table = parse_table_from_uri(&uri)
            .ok_or_else(|| McpError::invalid_params(format!("Invalid URI scheme: {uri}"), None))?;
        let safe_table = validate_table_name(&table)
            .map_err(|e| McpError::invalid_params(e.to_string(), None))?;
        let contents = match db::read_table(&self.pool(None), &safe_table).await {
            Ok(text) => text,
            Err(e) => {
                log::error!("Database error reading resource {uri}: {e:#}");
                return Err(McpError::internal_error(
                    format!("Database error: {e:#}"),
                    None,
                ));
            }
        };
        Ok(ReadResourceResult::new(vec![ResourceContents::text(contents, uri)]).into())
    }
}

/// 解码 `resources/list` 分页游标：不透明字符串 = 起始偏移量，无游标从 0 开始。
fn decode_cursor(cursor: Option<String>) -> Result<usize, McpError> {
    cursor.map_or(Ok(0), |c| {
        c.parse::<usize>()
            .map_err(|_| McpError::invalid_params(format!("Invalid cursor: {c}"), None))
    })
}

/// 从 `mssql://{table}/data` 中解析表或视图名。
fn parse_table_from_uri(uri: &str) -> Option<String> {
    uri.strip_prefix(MSSQL_URI_SCHEME)?
        .split('/')
        .next()
        .filter(|table| !table.is_empty())
        .map(str::to_string)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn decode_cursor_defaults_to_zero() {
        assert_eq!(decode_cursor(None).unwrap(), 0);
    }

    #[test]
    fn decode_cursor_parses_offset() {
        assert_eq!(decode_cursor(Some("500".to_string())).unwrap(), 500);
        assert_eq!(decode_cursor(Some("0".to_string())).unwrap(), 0);
    }

    #[test]
    fn decode_cursor_rejects_invalid() {
        assert!(decode_cursor(Some("abc".to_string())).is_err());
        assert!(decode_cursor(Some("".to_string())).is_err());
        assert!(decode_cursor(Some("-1".to_string())).is_err());
    }

    #[test]
    fn parse_table_from_uri_extracts_table() {
        assert_eq!(
            parse_table_from_uri("mssql://users/data").as_deref(),
            Some("users")
        );
        assert_eq!(
            parse_table_from_uri("mssql://dbo.users/data").as_deref(),
            Some("dbo.users")
        );
    }

    #[test]
    fn parse_table_from_uri_rejects_other_schemes() {
        assert_eq!(parse_table_from_uri("file:///etc/passwd"), None);
        assert_eq!(parse_table_from_uri("mssql:///data"), None);
    }

    #[test]
    fn resolve_format_defaults_to_csv() {
        assert_eq!(resolve_format(None).unwrap(), OutputFormat::Csv);
        assert_eq!(resolve_format(Some("")).unwrap(), OutputFormat::Csv);
        assert_eq!(resolve_format(Some("json")).unwrap(), OutputFormat::Json);
        assert_eq!(
            resolve_format(Some("markdown")).unwrap(),
            OutputFormat::Markdown
        );
    }

    #[test]
    fn resolve_format_rejects_unknown() {
        let err = resolve_format(Some("xml")).unwrap_err();
        assert!(
            err.message.contains("Invalid format"),
            "got: {}",
            err.message
        );
    }

    #[test]
    fn normalize_database_treats_blank_as_unset() {
        assert_eq!(normalize_database(None), None);
        assert_eq!(normalize_database(Some("")), None);
        assert_eq!(normalize_database(Some("  ")), None);
        assert_eq!(normalize_database(Some(" other ")), Some("other"));
    }
}
