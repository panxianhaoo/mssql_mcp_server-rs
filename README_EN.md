# mssql_mcp_server-rs

[中文](README.md) | English

A Microsoft SQL Server MCP (Model Context Protocol) server, implemented in Rust.

Built on the official Rust SDK [rmcp](https://github.com/modelcontextprotocol/rust-sdk) and the [tiberius](https://github.com/prisma/tiberius) TDS driver, serving JSON-RPC over stdio (or streamable-http when built with the `http` feature).

## Features

- **Tool `execute_sql`**: run read-only SQL queries
  - Only a single `SELECT` statement (including `WITH ... SELECT`) is allowed; any modifying statement (INSERT/UPDATE/DELETE/DDL/EXEC, etc.) or multi-statement batch is rejected
  - `SELECT` results are returned as RFC 4180 CSV (first row = column names; values containing commas, quotes or newlines are quoted and escaped)
  - NULL values render as `NULL`, binary as hexadecimal, timestamps in ISO format; `money`/`smallmoney` keep 4 fixed-point decimals
  - Datetime fractional digits **follow the column's scale**: `datetime2(7)` prints 7 digits (`.1234567`), `datetime2(1)` prints 1 (`.1`), `time(0)` prints none; `datetimeoffset` prints the converted local time plus an offset suffix (e.g. `2026-10-01 12:00:00.123+08:00`)
  - Queries against `INFORMATION_SCHEMA.TABLES` return a `Tables_in_{database}`-style table listing (detected via lexical analysis, so mentions inside comments or strings do not trigger it; the bracketed form `[INFORMATION_SCHEMA].[TABLES]` does not either)
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
| `MSSQL_AUTH` | No | `sql` | `sql`: SQL login (default, requires username/password); `windows`: Windows Integrated Auth (Windows only, uses the current logged-in user, no username/password needed, TLS enabled by default) |
| `MSSQL_ENCRYPT` | No | `false` | Set to `true` to enable TLS (Azure connections are always encrypted; Windows Integrated Auth enables it by default unless explicitly set to `false`) |
| `MSSQL_TRANSPORT` | No | `stdio` | `stdio`: local subprocess (default); `http`: HTTP server (**requires building with the `http` feature**) |
| `MSSQL_HTTP_ADDR` | No | `127.0.0.1:8000` | Listen address when `MSSQL_TRANSPORT=http` |
| `MSSQL_HTTP_BEARER_TOKEN` | No | | Bearer token for HTTP mode; **required when binding a non-loopback address**, otherwise startup is refused |
| `MSSQL_HTTP_ALLOWED_HOSTS` | No | | Extra `Host` headers to allow in HTTP mode (comma-separated); required when running in a container with `-p` port mapping and reaching it from outside, otherwise requests get `403` |

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

HTTP transport (remote / multi-client, requires `--features http`):

```bash
cargo build --release --features http

MSSQL_TRANSPORT=http MSSQL_HTTP_ADDR=127.0.0.1:8000 \
MSSQL_USER=sa MSSQL_PASSWORD=your_password MSSQL_DATABASE=master \
  ./target/release/mssql_mcp_server-rs
# Endpoint: http://127.0.0.1:8000/mcp
```

Binding a non-loopback address (e.g. `0.0.0.0`) **requires** `MSSQL_HTTP_BEARER_TOKEN`
as well, otherwise startup is refused — this avoids accidentally exposing the
database to the local network. Clients must send `Authorization: Bearer <token>`.
Put a TLS-terminating reverse proxy in front for production.

In containers you also need to allow the `Host` header: the transport only accepts
`localhost` / `127.0.0.1` / `::1` by default (DNS rebinding protection), so reaching
a `-p`-mapped port from the host carries the host's address and is rejected with
`403 Forbidden: Host header is not allowed`.

```bash
MSSQL_TRANSPORT=http MSSQL_HTTP_ADDR=0.0.0.0:8000 \
MSSQL_HTTP_BEARER_TOKEN=your_token \
MSSQL_HTTP_ALLOWED_HOSTS=mcp.example.com,192.168.1.50:8000 \
  ./target/release/mssql_mcp_server-rs
```

This list is **added to** the loopback allowlist, so local access keeps working.

## Use with Claude Code

```bash
claude mcp add mssql -- ./path/to/mssql_mcp_server-rs \
  -e MSSQL_SERVER=localhost -e MSSQL_USER=sa \
  -e MSSQL_PASSWORD=your_password -e MSSQL_DATABASE=master
```

## Notes

- Read-only protection: `execute_sql` accepts only a single SELECT query; modifying statements are rejected before being sent to the database. This also covers statements that look like a SELECT but write (`SELECT ... INTO` creating and populating a table, `SELECT NEXT VALUE FOR` advancing a sequence). An application-layer allowlist cannot cover every edge case (e.g. functions with side effects), so in production pair it with a read-only database account
- Windows Integrated Auth (SSPI): `MSSQL_AUTH=windows`, Windows only (startup fails on other platforms). Credentials travel on the wire, so TLS is enabled by default unless `MSSQL_ENCRYPT` is set
- LocalDB named instances are not supported (direct TCP host:port connections only)
- Precision limit: the `money` extremes ±`922337203685477.5808` cannot round-trip their last 4 decimals — the driver decodes money into an `f64`, and 19 significant digits are truncated at decode time. Ordinary values well inside that range are unaffected

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

### Seed data for manual exploration

To poke at the server from an MCP client, load a sample database covering a
wide range of shapes (scalar types, NULLs, Chinese and special characters,
composite primary keys, INCLUDE indexes, foreign keys, views, per-column
collation):

```bash
docker compose up -d --wait
docker cp scripts/seed-test-db.sql mssql-test:/seed.sql
docker exec mssql-test /opt/mssql-tools18/bin/sqlcmd \
  -S localhost -U sa -P 'YourStrong!Passw0rd' -C -i /seed.sql
```

The script is idempotent; the container and its volume are kept around, so
there is nothing to clean up afterwards.

The code is organized as a library crate (`src/lib.rs`) plus a thin binary (`src/main.rs`),
which is what allows `tests/integration.rs` to call the `db`/`config` modules directly.

## License

MIT
