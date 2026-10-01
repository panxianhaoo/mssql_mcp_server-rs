//! mssql_mcp_server-rs 的可执行入口：通过 stdio 提供 JSON-RPC。
//!
//! MCP 工具：`execute_sql`、`describe_table`；资源：`mssql://{table}/data`。

use mssql_mcp_server_rs::config;
use mssql_mcp_server_rs::server::McpServer;
use rmcp::ServiceExt;
use rmcp::transport::stdio;

fn init_logging() {
    // MCP stdio 传输要求 stdout 只能承载协议消息，日志全部输出到 stderr。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    init_logging();
    log::info!("Starting MSSQL MCP server...");

    let db_config = match config::DbConfig::from_env() {
        Ok(config) => config,
        Err(e) => {
            log::error!("{e}");
            return Err(e);
        }
    };
    log::info!(
        "Database config: {}:{}/{} as {} (encrypt: {}, azure: {})",
        db_config.server,
        db_config.port,
        db_config.database,
        db_config.auth.describe(),
        db_config.encrypt,
        db_config.is_azure()
    );

    let service = McpServer::new(db_config).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
