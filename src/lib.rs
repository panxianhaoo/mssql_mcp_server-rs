//! mssql_mcp_server-rs：基于官方 Rust MCP SDK 的 Microsoft SQL Server MCP 服务器。
//!
//! 库 crate 暴露全部模块，供 `main.rs` 组装二进制、同时供集成测试
//! （`tests/`）直接调用——binary crate 无法被集成测试引用。

pub mod config;
pub mod db;
pub mod format;
#[cfg(feature = "http")]
pub mod http;
pub mod pool;
pub mod resultset;
pub mod server;
pub mod sql;
pub mod values;
