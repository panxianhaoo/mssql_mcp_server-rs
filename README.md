# mssql_mcp_server-rs

中文 | [English](README_EN.md)

Microsoft SQL Server 的 MCP（Model Context Protocol）服务器，Rust 实现。

基于官方 Rust SDK [rmcp](https://github.com/modelcontextprotocol/rust-sdk) 与
[tiberius](https://github.com/prisma/tiberius) TDS 驱动，通过 stdio 提供 JSON-RPC
（启用 `http` feature 后也可走 streamable-http）。

## 功能

- **工具 `execute_sql`**：执行只读 SQL 查询
  - 仅允许单条 `SELECT`（含 `WITH ... SELECT`）；任何修改语句（INSERT/UPDATE/DELETE/DDL/EXEC 等）与多语句批次都会被拒绝
  - `SELECT` 返回 RFC 4180 CSV 格式结果（首行列名；含逗号/引号/换行的值按引号包裹转义）
  - NULL 值渲染为 `NULL`、二进制渲染为十六进制、时间渲染为 ISO 格式
  - 对 `INFORMATION_SCHEMA.TABLES` 的查询返回 `Tables_in_{database}` 风格的表清单
- **工具 `describe_table`**：查看表或视图的结构（列名、类型、可空性、长度/精度、默认值）与索引信息（名称、类型、唯一性、主键、键列、包含列）
  - 参数 `table` 接受 `users`、`dbo.users` 或视图名；表名经参数绑定传入，无注入风险
  - 返回**两段 CSV**：先是列信息（按列序），空一行后以注释行 `# INDEXES` 引出第二段索引信息。每个索引列一行（复合主键的每个键列各自成行，`INCLUDE` 列亦单独一行并标 `IS_INCLUDED_COLUMN=YES`），因此不存在逗号拼接导致的列错位；无索引时第二段仅表头
  - 列信息为 9 列：`TABLE_SCHEMA, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, COLUMN_DEFAULT, COLLATION_NAME`
  - `COLLATION_NAME` 逐列给出排序规则（SQL Server 支持**列级** collation，同一张表可混用不同规则），非字符类型该列为 `NULL`；这一列放在最末，以降低对既有「按位置」消费者的影响
  - 不带 schema 时匹配所有 schema，结果含 `TABLE_SCHEMA`/`OBJECT_SCHEMA` 列
  - 跳过 `# ` 开头的注释行后，两段均可交由标准 CSV 解析器读取
- **`format` 参数**（`execute_sql` / `table_sizes` / `list_databases`）：输出格式可选 `csv`（默认）、`json`、`markdown`
  - `json` 输出对象数组，含逗号/引号的值无需转义即可无歧义解析；值一律为字符串，以保留 money 定点、二进制十六进制等渲染语义
  - `markdown` 输出表格，单元格内的 `|` 转义为 `\|`、换行折叠为 `<br>`
- **工具 `table_sizes`**：报告用户表的行数与磁盘占用（取自元数据，恒 O(1)，**不扫表**）；可按表名子串过滤
- **工具 `list_databases`**：列出服务器上的数据库（仅 ONLINE 且当前登录可访问），便于先发现库再查表
- **多数据库**：`execute_sql` / `describe_table` / `table_sizes` 均可带 `database` 参数；每个库按需建独立连接池（不用 `USE` 切库——`USE` 是连接级状态，会泄漏给下一个借用者）
- **`describe_table` 可选分节**：`include_row_count`（行数与空间）、`include_dependent_views`（引用该表的视图及其定义 SQL），默认关闭以保持既有「两段 CSV」格式
- **资源 `mssql://{table}/data`**：每张用户表/视图一个资源，读取前 100 行；资源名称区分 `Table: x` / `View: x`；`resources/list` 按 cursor 分页（每页最多 500 条），大目录下响应体有界
  - URI 中的表名带 schema（`mssql://dbo.orders/data`），避免跨 schema 落到默认 schema 读错表
  - 同时声明资源模板 `mssql://{table}/data`，客户端可直接补全而不必先枚举
- **连接池**：内置 bb8 连接池（最多 4 个连接，借出前 `SELECT 1` 探活），复用连接省去每请求的 TCP/TDS 握手开销

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

| `MSSQL_TRANSPORT` | 否 | `stdio` | `stdio`：本地子进程（默认）；`http`：HTTP 服务（**需启用 `http` feature** 编译） |
| `MSSQL_HTTP_ADDR` | 否 | `127.0.0.1:8000` | `MSSQL_TRANSPORT=http` 时的监听地址 |
| `MSSQL_HTTP_BEARER_TOKEN` | 否 | | HTTP 模式的 bearer token；**绑定非 loopback 地址时必须设置**，否则拒绝启动 |

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

HTTP 传输（远程/多客户端共享，需 `--features http`）：

```bash
cargo build --release --features http

MSSQL_TRANSPORT=http MSSQL_HTTP_ADDR=127.0.0.1:8000 \
MSSQL_USER=sa MSSQL_PASSWORD=your_password MSSQL_DATABASE=master \
  ./target/release/mssql_mcp_server-rs
# 端点：http://127.0.0.1:8000/mcp
```

绑定到非 loopback 地址（如 `0.0.0.0`）**必须**同时设置 `MSSQL_HTTP_BEARER_TOKEN`，
否则进程拒绝启动——避免无意间把数据库暴露到局域网。客户端需带
`Authorization: Bearer <token>`。生产部署请前置带 TLS 的反向代理。

## 在 Claude Code 中使用

```bash
claude mcp add mssql -- ./path/to/mssql_mcp_server-rs \
  -e MSSQL_SERVER=localhost -e MSSQL_USER=sa \
  -e MSSQL_PASSWORD=your_password -e MSSQL_DATABASE=master
```

## 注意事项

- 只读保护：`execute_sql` 仅接受单条 SELECT 查询，修改语句在发送到数据库前即被拒绝；除了 INSERT/UPDATE/DELETE/DDL/EXEC 与多语句批次，也会拒绝语法上是 SELECT 但会写入的语句（`SELECT ... INTO` 建表写数据、`SELECT NEXT VALUE FOR` 推进序列）。应用层白名单不能覆盖所有边界（如带副作用的函数），生产环境建议配合只读数据库账号
- Windows 集成认证（SSPI）：`MSSQL_AUTH=windows`，仅 Windows 平台可用（非 Windows 启动即报错）
- 不支持 LocalDB 命名实例（仅 TCP 直连 host:port）

## 开发

```bash
cargo test    # 单元测试（表名校验、SELECT 判断、配置解析、值格式化、CSV 渲染）
cargo clippy
```

代码组织为 library crate（`src/lib.rs`）+ 薄二进制入口（`src/main.rs`），
因此 `tests/integration.rs` 可直接调用 `db`/`config` 等模块。

### 集成测试

需要真实 SQL Server。仓库自带 compose 文件，一条命令即可起好实例：`--wait`
会阻塞到实例健康检查通过，不需要手动 sleep。

```bash
docker compose up -d --wait

MSSQL_SERVER=localhost MSSQL_PORT=14333 \
MSSQL_USER=sa MSSQL_PASSWORD='YourStrong!Passw0rd' MSSQL_DATABASE=master \
  cargo test --test integration -- --ignored

docker compose down        # 加 -v 连同数据卷删除
```

测试会各自创建**专属命名**的表/视图/索引（复合主键、带 `INCLUDE` 列的索引、
含逗号与引号的数据行），互不干扰，因此既无需预置 schema、也可并行执行，
重复运行结果稳定。CI 在 Linux runner 上跑同样这组用例。

## License

MIT
