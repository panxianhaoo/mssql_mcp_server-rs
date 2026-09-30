# mssql_mcp_server-rs

Microsoft SQL Server 的 MCP（Model Context Protocol）服务器，Rust 实现。

基于官方 Rust SDK [rmcp](https://github.com/modelcontextprotocol/rust-sdk) 与
[tiberius](https://github.com/prisma/tiberius) TDS 驱动，通过 stdio 提供 JSON-RPC。

## 功能

- **工具 `execute_sql`**：执行只读 SQL 查询
  - 仅允许单条 `SELECT`（含 `WITH ... SELECT`）；任何修改语句（INSERT/UPDATE/DELETE/DDL/EXEC 等）与多语句批次都会被拒绝
  - `SELECT` 返回 CSV 格式结果（首行列名）
  - NULL 值渲染为 `NULL`、二进制渲染为十六进制、时间渲染为 ISO 格式
  - 对 `INFORMATION_SCHEMA.TABLES` 的查询返回 `Tables_in_{database}` 风格的表清单
- **工具 `describe_table`**：查看表或视图的结构（列名、类型、可空性、长度/精度、默认值）
  - 参数 `table` 接受 `users`、`dbo.users` 或视图名；表名经参数绑定传入，无注入风险
  - 返回 CSV，按列顺序排列；不带 schema 时匹配所有 schema，结果含 `TABLE_SCHEMA` 列
- **资源 `mssql://{table}/data`**：每张用户表/视图一个资源，读取前 100 行；资源名称区分 `Table: x` / `View: x`

## 环境变量

| 变量 | 必填 | 默认值 | 说明 |
|------|------|--------|------|
| `MSSQL_SERVER` | 否 | `localhost` | 服务器地址；`*.database.windows.net` 自动强制加密并校验证书 |
| `MSSQL_PORT` | 否 | `1433` | 端口 |
| `MSSQL_USER` | **是** | | SQL 认证用户名 |
| `MSSQL_PASSWORD` | **是** | | SQL 认证密码 |
| `MSSQL_DATABASE` | **是** | | 数据库名 |
| `MSSQL_AUTH` | 否 | `sql` | `sql`：SQL 登录（默认，需用户名密码）；`windows`：Windows 集成认证（仅 Windows 平台，使用当前登录用户，无需用户名密码） |
| `MSSQL_ENCRYPT` | 否 | `false` | 非加密改为 `true` 启用 TLS（Azure 连接始终加密） |

日志通过 `RUST_LOG`（默认 `info`）控制，全部输出到 stderr，不干扰 stdout 协议流。

## 构建与运行

```bash
cargo build --release

MSSQL_SERVER=localhost MSSQL_PORT=1433 \
MSSQL_USER=sa MSSQL_PASSWORD=your_password MSSQL_DATABASE=master \
  ./target/release/mssql_mcp_server-rs
```

Windows 集成认证（免密，使用当前登录用户）：

```powershell
$env:MSSQL_AUTH="windows"; $env:MSSQL_DATABASE="master"
.\target\release\mssql_mcp_server-rs.exe
```

## 在 Claude Code 中使用

```bash
claude mcp add mssql -- ./path/to/mssql_mcp_server-rs \
  -e MSSQL_SERVER=localhost -e MSSQL_USER=sa \
  -e MSSQL_PASSWORD=your_password -e MSSQL_DATABASE=master
```

## 注意事项

- 只读保护：`execute_sql` 仅接受单条 SELECT 查询，修改语句在发送到数据库前即被拒绝；应用层白名单不能覆盖所有边界（如带副作用的函数），生产环境建议配合只读数据库账号
- Windows 集成认证（SSPI）：`MSSQL_AUTH=windows`，仅 Windows 平台可用（非 Windows 启动即报错）
- 不支持 LocalDB 命名实例（仅 TCP 直连 host:port）

## 开发

```bash
cargo test    # 单元测试（表名校验、SELECT 判断、配置解析、值格式化）
cargo clippy
```

## License

MIT
