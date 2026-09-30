# mssql_mcp_server-rs

[中文](README.md) | English

A Microsoft SQL Server MCP (Model Context Protocol) server, implemented in Rust.

Built on the official Rust SDK [rmcp](https://github.com/modelcontextprotocol/rust-sdk) and the [tiberius](https://github.com/prisma/tiberius) TDS driver, serving JSON-RPC over stdio.

## Features

- **Tool `execute_sql`**: run read-only SQL queries
  - Only a single `SELECT` statement (including `WITH ... SELECT`) is allowed; any modifying statement (INSERT/UPDATE/DELETE/DDL/EXEC, etc.) or multi-statement batch is rejected
  - `SELECT` results are returned as CSV (first row = column names)
  - NULL values render as `NULL`, binary as hexadecimal, timestamps in ISO format
  - Queries against `INFORMATION_SCHEMA.TABLES` return a `Tables_in_{database}`-style table listing
- **Tool `describe_table`**: inspect the structure of a table or view (column names, types, nullability, length/precision, defaults) plus index information (name, type, uniqueness, primary key, key columns, included columns)
  - The `table` parameter accepts `users`, `dbo.users`, or a view name; table names are passed via parameter binding — no injection risk
  - Returns CSV ordered by column position, followed by an `INDEXES` section (CSV; header only when there are no indexes; the index query uses `STRING_AGG`, requires SQL Server 2017+). Without a schema qualifier, all schemas are matched and the result includes `TABLE_SCHEMA`/`OBJECT_SCHEMA` columns
- **Resource `mssql://{table}/data`**: one resource per user table/view, reading the first 100 rows; resource names distinguish `Table: x` / `View: x`; `resources/list` is cursor-paginated (max 500 items per page), keeping response size bounded on large catalogs
- **Connection pool**: built-in bb8 pool (max 4 connections, `SELECT 1` liveness check on checkout), reusing connections instead of a TCP/TDS handshake per request

## Environment Variables

| Variable | Required | Default | Description |
|------|------|--------|------|
| `MSSQL_SERVER` | No | `localhost` | Server address; `*.database.windows.net` automatically enforces encryption and certificate validation |
| `MSSQL_PORT` | No | `1433` | Port |
| `MSSQL_USER` | **Yes** | | SQL auth username |
| `MSSQL_PASSWORD` | **Yes** | | SQL auth password |
| `MSSQL_DATABASE` | **Yes** | | Database name |
| `MSSQL_AUTH` | No | `sql` | `sql`: SQL login (default, requires username/password); `windows`: Windows Integrated Auth (Windows only, uses the current logged-in user, no username/password needed) |
| `MSSQL_ENCRYPT` | No | `false` | Set to `true` to enable TLS (Azure connections are always encrypted) |

Logs are controlled via `RUST_LOG` (default `info`) and go entirely to stderr, so they never interfere with the stdout protocol stream.

## Build & Run

```bash
cargo build --release

MSSQL_SERVER=localhost MSSQL_PORT=1433 \
MSSQL_USER=sa MSSQL_PASSWORD=your_password MSSQL_DATABASE=master \
  ./target/release/mssql_mcp_server-rs
```

Windows Integrated Auth (passwordless, uses the current user):

```powershell
$env:MSSQL_AUTH="windows"; $env:MSSQL_DATABASE="master"
.\target\release\mssql_mcp_server-rs.exe
```

## Use with Claude Code

```bash
claude mcp add mssql -- ./path/to/mssql_mcp_server-rs \
  -e MSSQL_SERVER=localhost -e MSSQL_USER=sa \
  -e MSSQL_PASSWORD=your_password -e MSSQL_DATABASE=master
```

## Notes

- Read-only protection: `execute_sql` accepts only a single SELECT query; modifying statements are rejected before being sent to the database. An application-layer allowlist cannot cover every edge case (e.g. functions with side effects), so in production pair it with a read-only database account
- Windows Integrated Auth (SSPI): `MSSQL_AUTH=windows`, Windows only (startup fails on other platforms)
- LocalDB named instances are not supported (direct TCP host:port connections only)

## Development

```bash
cargo test    # unit tests (table name validation, SELECT detection, config parsing, value formatting)
cargo clippy
```

## License

MIT
