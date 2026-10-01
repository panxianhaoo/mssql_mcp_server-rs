//! 结果集的收集与 CSV 渲染。
//!
//! `Resultset` 是纯数据结构（列名 + 字符串化的行值），与连接池无关，
//! 因此 CSV 渲染逻辑可以独立单元测试。

use futures_util::StreamExt;
use tiberius::QueryItem;

use crate::values::column_data_to_string_typed;

/// 单个结果集（列名 + 每行的字符串值）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Resultset {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<String>>,
}

impl Resultset {
    /// 构造空结果集（无列名、无行）。
    pub fn empty() -> Self {
        Self {
            columns: Vec::new(),
            rows: Vec::new(),
        }
    }
}

/// 消费查询流，收集第一个结果集（能正确处理空结果集的列名）。
pub async fn collect_first_resultset(
    stream: tiberius::QueryStream<'_>,
) -> anyhow::Result<Resultset> {
    let mut stream = stream;
    let mut columns: Option<Vec<String>> = None;
    let mut rows: Vec<Vec<String>> = Vec::new();

    while let Some(item) = stream.next().await {
        match item? {
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
                    columns = Some(row.columns().iter().map(|c| c.name().to_string()).collect());
                }
                // 逐格带上列的 TDS 类型：`money` 与 `float` 在 tiberius 侧都是
                // f64，只有靠 ColumnType 才能分别按定点/浮点语义渲染。
                rows.push(
                    row.cells()
                        .map(|(column, data)| {
                            column_data_to_string_typed(data, column.column_type())
                        })
                        .collect(),
                );
            }
        }
    }

    Ok(Resultset {
        columns: columns.unwrap_or_default(),
        rows,
    })
}

/// 按 RFC 4180 转义单个 CSV 字段。
///
/// 字段含分隔符、引号或换行时必须加引号包裹，内部的 `"` 加倍为 `""`，
/// 否则下游（LLM 或电子表格）解析出的列会错位。
/// 据此负号开头的数字、`NULL` 等无特殊字符的值保持原样，不损失可读性。
fn escape_csv_field(field: &str) -> String {
    if field.chars().any(|c| matches!(c, ',' | '"' | '\n' | '\r')) {
        format!("\"{}\"", field.replace('"', "\"\""))
    } else {
        field.to_string()
    }
}

/// 把结果集渲染为 CSV（首行列名），与参考实现输出格式一致。
pub fn resultset_to_csv(resultset: &Resultset) -> String {
    let mut lines = vec![csv_line(&resultset.columns)];
    lines.extend(resultset.rows.iter().map(|row| csv_line(row)));
    lines.join("\n")
}

/// 把一行字段拼接为 CSV 行（逐字段转义后以逗号连接）。
fn csv_line(fields: &[String]) -> String {
    fields
        .iter()
        .map(|field| escape_csv_field(field))
        .collect::<Vec<_>>()
        .join(",")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn resultset(columns: &[&str], rows: Vec<Vec<&str>>) -> Resultset {
        Resultset {
            columns: columns.iter().map(|s| s.to_string()).collect(),
            rows: rows
                .into_iter()
                .map(|row| row.into_iter().map(|s| s.to_string()).collect())
                .collect(),
        }
    }

    #[test]
    fn renders_header_and_rows() {
        let csv = resultset_to_csv(&resultset(
            &["id", "name"],
            vec![vec!["1", "alice"], vec!["2", "bob"]],
        ));
        assert_eq!(csv, "id,name\n1,alice\n2,bob");
    }

    #[test]
    fn renders_header_only_for_empty_resultset() {
        let csv = resultset_to_csv(&resultset(&["a", "b"], vec![]));
        assert_eq!(csv, "a,b");
    }

    #[test]
    fn renders_nothing_for_empty_columns() {
        assert_eq!(resultset_to_csv(&Resultset::empty()), "");
    }

    #[test]
    fn quotes_fields_containing_delimiters() {
        let csv = resultset_to_csv(&resultset(&["a", "b"], vec![vec!["hello,world", "plain"]]));
        assert_eq!(csv, "a,b\n\"hello,world\",plain");
    }

    #[test]
    fn quotes_fields_containing_quotes_and_doubles_them() {
        let csv = resultset_to_csv(&resultset(&["a"], vec![vec!["say \"hi\""]]));
        assert_eq!(csv, "a\n\"say \"\"hi\"\"\"");
    }

    #[test]
    fn quotes_fields_containing_newlines() {
        let csv = resultset_to_csv(&resultset(&["a"], vec![vec!["line1\nline2"]]));
        assert_eq!(csv, "a\n\"line1\nline2\"");
        let csv_cr = resultset_to_csv(&resultset(&["a"], vec![vec!["line1\r\nline2"]]));
        assert_eq!(csv_cr, "a\n\"line1\r\nline2\"");
    }

    #[test]
    fn leaves_plain_and_negative_numbers_unquoted() {
        // 负号开头的数字不能被 CSV 注入防护改写成引号形式，否则变成字符串。
        let csv = resultset_to_csv(&resultset(
            &["n", "s"],
            vec![vec!["-5", "NULL"], vec!["3.14", ""]],
        ));
        assert_eq!(csv, "n,s\n-5,NULL\n3.14,");
    }

    #[test]
    fn row_with_embedded_comma_keeps_column_count() {
        // 关键回归点：含逗号的值必须被引号包裹，才能维持每行列数一致。
        let csv = resultset_to_csv(&resultset(
            &["id", "note"],
            vec![vec!["1", "a,b,c"], vec!["2", "ok"]],
        ));
        let mut lines = csv.lines();
        assert_eq!(lines.next(), Some("id,note"));
        assert_eq!(lines.next(), Some("1,\"a,b,c\""));
        assert_eq!(lines.next(), Some("2,ok"));
    }

    #[test]
    fn empty_resultset_has_no_rows() {
        let empty = Resultset::empty();
        assert!(empty.columns.is_empty());
        assert!(empty.rows.is_empty());
    }
}
