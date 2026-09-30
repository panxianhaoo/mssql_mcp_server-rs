# mssql_mcp_server-rs

Microsoft SQL Server 的 MCP（Model Context Protocol）服务器，Rust 移植版。

参考 [mssql_mcp_server](../mssql_mcp_server)（Python）实现，基于官方 Rust SDK
[rmcp](https://github.com/modelcontextprotocol/rust-sdk) 与
[tiberius](https://github.com/prisma/tiberius) TDS 驱动，通过 stdio 提供 JSON-RPC。

## 功能

- **工具 `execute_sql`**：执行任意 SQL 查询
  - `SELECT` 返回 CSV 格式结果（首行列名）
  - 对 `INFORMATION_SCHEMA.TABLES` 的查询返回 `Tables_in_{database}` 风格的表清单
  - 非 `SELECT` 语句返回受影响行数
- **资源 `mssql://{table}/data`**：每张用户表一个资源，读取前 100 行

## 环境变量

| 变量 | 必填 | 默认值 | 说明 |
|------|------|--------|------|
| `MSSQL_SERVER` | 否 | `localhost` | 服务器地址；`*.database.windows.net` 自动强制加密并校验证书 |
| `MSSQL_PORT` | 否 | `1433` | 端口 |
| `MSSQL_USER` | **是** | | SQL 认证用户名 |
| `MSSQL_PASSWORD` | **是** | | SQL 认证密码 |
| `MSSQL_DATABASE` | **是** | | 数据库名 |
| `MSSQL_ENCRYPT` | 否 | `false` | 非加密改为 `true` 启用 TLS（Azure 连接始终加密） |

日志通过 `RUST_LOG`（默认 `info`）控制，全部输出到 stderr，不干扰 stdout 协议流。

## 构建与运行

```bash
cargo build --release

MSSQL_SERVER=localhost MSSQL_PORT=1433 \
MSSQL_USER=sa MSSQL_PASSWORD=your_password MSSQL_DATABASE=master \
  ./target/release/mssql_mcp_server-rs
```

## 在 Claude Code 中使用

```bash
claude mcp add mssql -- ./path/to/mssql_mcp_server-rs \
  -e MSSQL_SERVER=localhost -e MSSQL_USER=sa \
  -e MSSQL_PASSWORD=your_password -e MSSQL_DATABASE=master
```

## 与 Python 版的差异

- 工具名固定为 `execute_sql`（不支持 `MSSQL_COMMAND` 自定义改名）
- 不支持 Windows 集成认证与 LocalDB 命名实例（tiberius 仅支持 TCP + SQL 认证）
- NULL 值渲染为 `NULL`、二进制渲染为十六进制、时间渲染为 ISO 格式

## 开发

```bash
cargo test    # 单元测试（表名校验、SELECT 判断、配置解析、值格式化）
cargo clippy
```

## License

MIT
