# mssql_mcp_server-rs

[中文](README.md) | English

A Microsoft SQL Server MCP (Model Context Protocol) server, implemented in Rust.

Built on the official Rust SDK [rmcp](https://github.com/modelcontextprotocol/rust-sdk) and the [tiberius](https://github.com/prisma/tiberius) TDS driver, serving JSON-RPC over stdio (or streamable-http when built with the `http` feature).

## Features

- **Tool `execute_sql`**: run read-only SQL queries
  - Only a single `SELECT` statement (including `WITH ... SELECT`) is allowed; any modifying statement (INSERT/UPDATE/DELETE/DDL/EXEC, etc.) or multi-statement batch is rejected
  - `SELECT` results are returned as RFC 4180 CSV (first row = column names; values containing commas, quotes or newlines are quoted and escaped)
  - NULL values render as `NULL`, binary as hexadecimal, timestamps in ISO format
  - Queries against `INFORMATION_SCHEMA.TABLES` return a `Tables_in_{database}`-style table listing
- **Tool `describe_table`**: inspect the structure of a table or view (column names, types, nullability, length/precision, defaults, collation) plus index information (name, type, uniqueness, primary key, key columns, included columns)
  - The `table` parameter accepts `users`, `dbo.users`, or a view name; table names are passed via parameter binding — no injection risk
  - Returns **two CSV sections**: first the column info (in column order), then a blank line and a `# INDEXES` comment line introducing the second section. Each index column gets its own row (every key column of a composite primary key is emitted separately; included columns get their own row with `IS_INCLUDED_COLUMN=YES`), so there is no comma-joined field to tear CSV columns apart. Header only when there are no indexes
  - The column section has 9 columns: `TABLE_SCHEMA, COLUMN_NAME, DATA_TYPE, IS_NULLABLE, CHARACTER_MAXIMUM_LENGTH, NUMERIC_PRECISION, NUMERIC_SCALE, COLUMN_DEFAULT, COLLATION_NAME`
  - `COLLATION_NAME` reports the collation per column (SQL Server supports **column-level** collation, so one table may mix different rules); it is `NULL` for non-character types. Listed last to reduce impact on existing positional consumers
  - Without a schema qualifier, all schemas are matched and the result includes `TABLE_SCHEMA`/`OBJECT_SCHEMA` columns
  - Skip lines starting with `# ` and both sections can be fed to a standard CSV parser
- **`format` parameter** (`execute_sql` / `table_sizes` / `list_databases`): output format `csv` (default), `json`, or `markdown` (`md` for short); case-insensitive
  - `json` emits an array of objects; values containing commas/quotes need no escaping to round-trip unambiguously. Values are always strings, preserving money fixed-point, hex binary, and other rendering semantics
  - `markdown` emits a table, escaping `|` as `\|` and collapsing newlines to `<br>`
  - `describe_table` has no `format`: it returns sections separated by `# INDEXES`-style comment lines, which JSON/Markdown would flatten (use `include_dependent_views` for view definitions, `include_row_count` for row counts)
  - The same `SELECT id, note FROM ...` (with `note` containing commas/pipes/newlines) in all three formats:

    `csv`

    ```
    id,note
    1,"a,b"
    2,x|y
    3,"l1
    l2"
    ```

    `json`

    ```json
    [
      {
        "id": "1",
        "note": "a,b"
      },
      {
        "id": "2",
        "note": "x|y"
      },
      {
        "id": "3",
        "note": "l1\nl2"
      }
    ]
    ```

    `markdown`

    ```
    | id | note |
    | --- | --- |
    | 1 | a,b |
    | 2 | x\|y |
    | 3 | l1<br>l2 |
    ```
- **Tool `table_sizes`**: reports row counts and disk usage for user tables, read from metadata (always O(1) — **no table scan**); optionally filter by table-name substring
- **Tool `list_databases`**: lists databases on the server that are online and accessible to the current login, so you can discover databases before querying tables
- **Multi-database**: `execute_sql` / `describe_table` / `table_sizes` accept a `database` parameter; each database gets its own lazily-created connection pool (rather than `USE` — that is connection-level state and leaks to the next borrower)
- **Optional `describe_table` sections**: `include_row_count` (rows and space) and `include_dependent_views` (views referencing the table, with their definition SQL); off by default to preserve the existing two-section CSV layout
- **Resource `mssql://{table}/data`**: one resource per user table/view, reading the first 100 rows; resource names distinguish `Table: x` / `View: x`; `resources/list` is cursor-paginated (max 500 items per page), keeping response size bounded on large catalogs
  - Resource names carry the schema (`mssql://dbo.orders/data`) so cross-schema lookups don't silently fall back to the default schema
  - A resource template `mssql://{table}/data` is also advertised, letting clients complete URIs instead of enumerating first
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
| `MSSQL_TRANSPORT` | No | `stdio` | `stdio`: local subprocess (default); `http`: HTTP server (**requires building with the `http` feature**) |
| `MSSQL_HTTP_ADDR` | No | `127.0.0.1:8000` | Listen address when `MSSQL_TRANSPORT=http` |
| `MSSQL_HTTP_BEARER_TOKEN` | No | | Bearer token for HTTP mode; **required when binding a non-loopback address**, otherwise startup is refused |

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
cargo test    # unit tests (table name validation, SELECT detection, config parsing, value formatting, CSV rendering)
cargo clippy
```

### Integration tests

They need a real SQL Server. A compose file is included — one command brings up the
instance, and `--wait` blocks until its health check passes (no manual sleep needed).

```bash
docker compose up -d --wait

MSSQL_SERVER=localhost MSSQL_PORT=14333 \
MSSQL_USER=sa MSSQL_PASSWORD='YourStrong!Passw0rd' MSSQL_DATABASE=master \
  cargo test --test integration -- --ignored

docker compose down        # add -v to also drop the data volume
```

Each test creates its own tables/views/indexes under a unique name (composite primary keys,
indexes with included columns, rows containing commas and quotes), so nothing is shared:
no schema setup is required, tests run safely in parallel, and repeated runs give stable
results. CI runs the same suite on a Linux runner.

The code is organized as a library crate (`src/lib.rs`) plus a thin binary (`src/main.rs`),
which is what allows `tests/integration.rs` to call the `db`/`config` modules directly.

## License

MIT
