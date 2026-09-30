//! mssql_mcp_server-rs：基于官方 Rust MCP SDK 的 Microsoft SQL Server MCP 服务器。
//!
//! 通过 stdio 提供 JSON-RPC，工具：`execute_sql`；资源：`mssql://{table}/data`。

mod config;
mod db;
mod server;
mod sql;
mod values;

use rmcp::ServiceExt;
use rmcp::transport::stdio;

fn init_logging() {
    // MCP stdio 传输要求 stdout 只能承载协议消息，日志全部输出到 stderr。
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info"))
        .target(env_logger::Target::Stderr)
        .init();
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    init_logging();
    log::info!("Starting MSSQL MCP server...");

    let db_config = match config::DbConfig::from_env() {
        Ok(config) => config,
        Err(e) => {
            log::error!("{e}");
            return Err(e.into());
        }
    };
    log::info!(
        "Database config: {}:{}/{} as {} (encrypt: {}, azure: {})",
        db_config.server,
        db_config.port,
        db_config.database,
        db_config.user,
        db_config.encrypt,
        db_config.is_azure()
    );

    let service = server::McpServer::new(db_config).serve(stdio()).await?;
    service.waiting().await?;
    Ok(())
}
