//! MCP server：提供只读的 `execute_sql` 工具与表资源（`mssql://{table}/data`）。

use std::sync::Arc;

use rmcp::handler::server::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, Implementation, ListResourcesResult, PaginatedRequestParams,
    ReadResourceRequestParams, ReadResourceResponse, ReadResourceResult, Resource,
    ResourceContents, ServerCapabilities, ServerConfig,
};
use rmcp::service::{RequestContext, RoleServer};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};

use crate::config::DbConfig;
use crate::db;
use crate::sql::{parse_table_name, validate_table_name};

const MSSQL_URI_SCHEME: &str = "mssql://";

/// `execute_sql` 工具的入参。
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct ExecuteSqlArgs {
    /// The SQL query to execute
    query: String,
}

/// `describe_table` 工具的入参。
#[derive(serde::Deserialize, schemars::JsonSchema)]
struct DescribeTableArgs {
    /// The table or view name to describe (e.g. "users", "dbo.users", "active_users")
    table: String,
}

#[derive(Clone)]
pub struct McpServer {
    config: Arc<DbConfig>,
    tool_router: ToolRouter<Self>,
}

#[tool_router]
impl McpServer {
    pub fn new(config: DbConfig) -> Self {
        Self {
            config: Arc::new(config),
            tool_router: Self::tool_router(),
        }
    }

    #[tool(
        description = "Execute a read-only SQL query (a single SELECT; WITH ... SELECT is allowed) on the SQL Server"
    )]
    async fn execute_sql(
        &self,
        Parameters(args): Parameters<ExecuteSqlArgs>,
    ) -> Result<CallToolResult, McpError> {
        if args.query.trim().is_empty() {
            return Err(McpError::invalid_params("Query is required", None));
        }
        // 数据库错误以文本形式返回（与参考实现一致），便于客户端读到失败原因。
        let output = match db::execute_query(&self.config, &args.query).await {
            Ok(text) => text,
            Err(e) => format!("Error executing query: {e:#}"),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(output)]))
    }

    #[tool(
        description = "Describe the structure of a SQL Server table or view (column names, types, nullability, length/precision, defaults)"
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
        // 数据库错误以文本形式返回（与 execute_sql 一致），便于客户端读到失败原因。
        let output = match db::describe_table(&self.config, schema.as_deref(), &table).await {
            Ok(text) => text,
            Err(e) => format!("Error describing table: {e:#}"),
        };
        Ok(CallToolResult::success(vec![ContentBlock::text(output)]))
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
             or read mssql://{table}/data resources to peek at table or view contents.",
        )
    }

    async fn list_resources(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListResourcesResult, McpError> {
        // 数据库不可达时返回空列表（与参考实现一致）。
        let resources = match db::list_tables_and_views(&self.config).await {
            Ok(objects) => objects
                .into_iter()
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
                .collect(),
            Err(e) => {
                log::error!("Failed to list resources: {e:#}");
                Vec::new()
            }
        };
        Ok(ListResourcesResult::with_all_items(resources))
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
        let contents = match db::read_table(&self.config, &safe_table).await {
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
}
