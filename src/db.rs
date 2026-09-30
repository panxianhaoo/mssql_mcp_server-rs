//! 数据库访问：建立连接、执行查询、列出与读取表。

use futures_util::StreamExt;
use tiberius::{AuthMethod, Client, Config, EncryptionLevel, QueryItem};
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use crate::config::DbConfig;
use crate::sql::{is_select_query, is_tables_listing_query};
use crate::values::column_data_to_string;

/// 单个结果集（列名 + 每行的字符串值）。
struct Resultset {
    columns: Vec<String>,
    rows: Vec<Vec<String>>,
}

/// 建立到 SQL Server 的连接（每次请求一个短连接，与参考实现一致）。
async fn connect(config: &DbConfig) -> Result<Client<Compat<TcpStream>>, String> {
    let mut tds_config = Config::new();
    tds_config.host(&config.server);
    tds_config.port(config.port);
    tds_config.database(&config.database);
    tds_config.authentication(AuthMethod::sql_server(&config.user, &config.password));
    tds_config.encryption(if config.encrypt {
        EncryptionLevel::On
    } else {
        EncryptionLevel::Off
    });
    if config.trust_server_certificate {
        tds_config.trust_cert();
    }

    let tcp = TcpStream::connect((config.server.as_str(), config.port))
        .await
        .map_err(|e| format!("failed to connect to {}: {}", config.server, e))?;
    tcp.set_nodelay(true).map_err(|e| e.to_string())?;
    Client::connect(tds_config, tcp.compat_write())
        .await
        .map_err(|e| e.to_string())
}

/// 消费查询流，收集第一个结果集（能正确处理空结果集的列名）。
async fn collect_first_resultset(stream: tiberius::QueryStream<'_>) -> Result<Resultset, String> {
    let mut stream = stream;
    let mut columns: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();

    while let Some(item) = stream.next().await {
        match item.map_err(|e| e.to_string())? {
            QueryItem::Metadata(meta) => {
                // 第一个 metadata 提供列名，第二个 metadata 意味着新的结果集，停止收集。
                if columns.is_none() {
                    columns = Some(
                        meta.columns()
                            .iter()
                            .map(|c| c.name().to_string())
                            .collect(),
                    );
                } else {
                    break;
                }
            }
            QueryItem::Row(row) => {
                if columns.is_none() {
                    columns = Some(
                        row.columns()
                            .iter()
                            .map(|c| c.name().to_string())
                            .collect(),
                    );
                }
                rows.push(row.cells().map(|(_, data)| column_data_to_string(data)).collect());
            }
        }
    }

    Ok(Resultset {
        columns: columns.unwrap_or_default(),
        rows,
    })
}

/// 把结果集渲染为 CSV（首行列名），与参考实现输出格式一致。
fn resultset_to_csv(resultset: &Resultset) -> String {
    let mut lines = vec![resultset.columns.join(",")];
    lines.extend(
        resultset
            .rows
            .iter()
            .map(|row| row.join(","))
            .collect::<Vec<_>>(),
    );
    lines.join("\n")
}

/// 列出当前数据库的所有用户表。
pub async fn list_tables(config: &DbConfig) -> Result<Vec<String>, String> {
    let mut client = connect(config).await?;
    let stream = client
        .simple_query(
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_TYPE = 'BASE TABLE'",
        )
        .await
        .map_err(|e| e.to_string())?;
    let resultset = collect_first_resultset(stream).await?;
    Ok(resultset
        .rows
        .into_iter()
        .map(|row| row.first().cloned().unwrap_or_default())
        .collect())
}

/// 读取表的前 100 行（表名必须已经过 `validate_table_name` 转义）。
pub async fn read_table(config: &DbConfig, safe_table: &str) -> Result<String, String> {
    let mut client = connect(config).await?;
    let query = format!("SELECT TOP 100 * FROM {safe_table}");
    let stream = client
        .simple_query(query)
        .await
        .map_err(|e| e.to_string())?;
    let resultset = collect_first_resultset(stream).await?;
    Ok(resultset_to_csv(&resultset))
}

/// 执行任意 SQL：SELECT 返回 CSV，其余语句返回受影响行数。
pub async fn execute_query(config: &DbConfig, query: &str) -> Result<String, String> {
    let mut client = connect(config).await?;

    if !is_select_query(query) {
        let result = client
            .execute(query, &[])
            .await
            .map_err(|e| e.to_string())?;
        let affected: u64 = result.rows_affected().iter().sum();
        return Ok(format!("Query executed successfully. Rows affected: {affected}"));
    }

    let stream = client
        .simple_query(query)
        .await
        .map_err(|e| e.to_string())?;
    let resultset = collect_first_resultset(stream).await?;

    // 对 INFORMATION_SCHEMA.TABLES 的查询输出 mysql 风格的表清单（对齐参考实现）。
    if is_tables_listing_query(query) && !resultset.columns.is_empty() {
        let header = format!("Tables_in_{}", config.database);
        let mut lines = vec![header];
        lines.extend(
            resultset
                .rows
                .iter()
                .map(|row| row.first().cloned().unwrap_or_default()),
        );
        return Ok(lines.join("\n"));
    }

    Ok(resultset_to_csv(&resultset))
}
