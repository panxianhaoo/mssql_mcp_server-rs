//! 工具输出格式：`execute_sql` 的 `format` 参数支持 CSV / JSON / Markdown。
//!
//! CSV 是默认格式（与 `describe_table` 及既有消费者保持一致），但对 LLM
//! 来说引号转义是阅读负担；JSON 无歧义、Markdown 表格可读性好，因此三者
//! 都可按调用方偏好选择。

use serde::Serialize;

use crate::resultset::Resultset;

/// 工具输出的渲染格式。
///
/// 带 `#[serde(rename_all = "lowercase")]`，使 JSON Schema 与线上取值都写成
/// `"csv"` / `"json"` / `"markdown"`。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Deserialize, schemars::JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum OutputFormat {
    /// RFC 4180 CSV（默认）：首行列名。
    #[default]
    Csv,
    /// JSON 对象数组：每行为 `{"列名": "值"}`，值一律为字符串。
    Json,
    /// Markdown 表格：表头 + `---` 分隔行 + 数据行。
    Markdown,
}

impl OutputFormat {
    /// 解析工具入参里的格式名；无法识别时返回 `None` 由调用方报错。
    pub fn parse(value: Option<&str>) -> Option<Self> {
        match value.map(str::trim).filter(|v| !v.is_empty()) {
            // 缺省即 CSV，避免破坏既有调用。
            None => Some(Self::Csv),
            Some(v) if v.eq_ignore_ascii_case("csv") => Some(Self::Csv),
            Some(v) if v.eq_ignore_ascii_case("json") => Some(Self::Json),
            Some(v) if v.eq_ignore_ascii_case("markdown") || v.eq_ignore_ascii_case("md") => {
                Some(Self::Markdown)
            }
            Some(_) => None,
        }
    }
}

/// 按指定格式渲染结果集。
pub fn render_resultset(resultset: &Resultset, format: OutputFormat) -> String {
    match format {
        OutputFormat::Csv => crate::resultset::resultset_to_csv(resultset),
        OutputFormat::Json => resultset_to_json(resultset),
        OutputFormat::Markdown => resultset_to_markdown(resultset),
    }
}

/// 渲染为 JSON 对象数组（值一律为字符串，与 CSV/Markdown 保持同一套值语义）。
///
/// 之所以不按数据库原生类型输出数字/布尔：列值是先经 [`crate::values`]
/// 格式化成字符串的（money 保留 4 位小数、二进制转十六进制、时间转 ISO），
/// 再塞回 JSON 数字会丢掉这些语义。统一字符串可让 LLM 稳定解析。
fn resultset_to_json(resultset: &Resultset) -> String {
    let rows: Vec<Row> = resultset
        .rows
        .iter()
        .map(|row| Row {
            fields: &resultset.columns,
            values: row,
        })
        .collect();
    serde_json::to_string_pretty(&rows).unwrap_or_else(|e| format!("{{\"error\": \"{e}\"}}"))
}

/// 单行：借用列名与值，按 `{列名: 值}` 序列化。
struct Row<'a> {
    fields: &'a [String],
    values: &'a [String],
}

impl Serialize for Row<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.fields.len()))?;
        for (field, value) in self.fields.iter().zip(self.values) {
            map.serialize_entry(field, value)?;
        }
        // 行比列名短（理论上不会）：缺的键补 NULL，避免静默丢列。
        for field in self.fields.iter().skip(self.values.len()) {
            map.serialize_entry(field, "NULL")?;
        }
        map.end()
    }
}

/// 渲染为 Markdown 表格。
///
/// 单元格里的 `|` 必须转义为 `\\|`，换行替换为 `<br>`，否则表格结构会被撕裂
/// （这与 CSV 里必须转义逗号是同一类问题）。
fn resultset_to_markdown(resultset: &Resultset) -> String {
    if resultset.columns.is_empty() {
        return String::new();
    }
    let mut lines = vec![
        markdown_line(&resultset.columns),
        markdown_separator(resultset.columns.len()),
    ];
    lines.extend(resultset.rows.iter().map(|row| markdown_line(row)));
    lines.join("\n")
}

/// 转义并拼接一行 Markdown 表格单元格。
fn markdown_line(cells: &[String]) -> String {
    let escaped: Vec<String> = cells
        .iter()
        .map(|cell| escape_markdown_cell(cell))
        .collect();
    format!("| {} |", escaped.join(" | "))
}

/// 分隔行：`| --- | --- | ... |`。
fn markdown_separator(columns: usize) -> String {
    let cells: Vec<&str> = (0..columns).map(|_| "---").collect();
    format!("| {} |", cells.join(" | "))
}

/// Markdown 单元格转义：`|` → `\\|`，换行 → `<br>`（表格里无法表达真实换行）。
fn escape_markdown_cell(cell: &str) -> String {
    // 先归一 CRLF 再逐字符替换，避免 `\r\n` 被拆成两个 `<br>`。
    cell.replace("\r\n", "\n")
        .replace('|', "\\|")
        .replace(['\n', '\r'], "<br>")
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
    fn parse_defaults_to_csv() {
        assert_eq!(OutputFormat::parse(None), Some(OutputFormat::Csv));
        assert_eq!(OutputFormat::parse(Some("")), Some(OutputFormat::Csv));
    }

    #[test]
    fn parse_accepts_all_formats_case_insensitively() {
        assert_eq!(OutputFormat::parse(Some("csv")), Some(OutputFormat::Csv));
        assert_eq!(OutputFormat::parse(Some("JSON")), Some(OutputFormat::Json));
        assert_eq!(
            OutputFormat::parse(Some("Markdown")),
            Some(OutputFormat::Markdown)
        );
        assert_eq!(
            OutputFormat::parse(Some("md")),
            Some(OutputFormat::Markdown)
        );
        assert_eq!(
            OutputFormat::parse(Some(" json ")),
            Some(OutputFormat::Json)
        );
    }

    #[test]
    fn parse_rejects_unknown() {
        assert_eq!(OutputFormat::parse(Some("xml")), None);
        assert_eq!(OutputFormat::parse(Some("yaml")), None);
    }

    #[test]
    fn json_renders_objects_keyed_by_column() {
        let json = render_resultset(
            &resultset(&["id", "name"], vec![vec!["1", "alice"], vec!["2", "bob"]]),
            OutputFormat::Json,
        );
        let parsed: Vec<serde_json::Map<String, serde_json::Value>> =
            serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed.len(), 2);
        assert_eq!(parsed[0]["id"], "1");
        assert_eq!(parsed[0]["name"], "alice");
        assert_eq!(parsed[1]["name"], "bob");
    }

    #[test]
    fn json_preserves_values_with_commas_and_quotes() {
        // 回归点：JSON 格式的意义正是让含分隔符的值无需转义即可无歧义解析。
        let json = render_resultset(
            &resultset(&["note"], vec![vec!["a,b"], vec!["say \"hi\""]]),
            OutputFormat::Json,
        );
        let parsed: Vec<serde_json::Map<String, serde_json::Value>> =
            serde_json::from_str(&json).expect("valid JSON");
        assert_eq!(parsed[0]["note"], "a,b");
        assert_eq!(parsed[1]["note"], "say \"hi\"");
    }

    #[test]
    fn json_renders_empty_rows_as_empty_array() {
        let json = render_resultset(&resultset(&["a"], vec![]), OutputFormat::Json);
        assert_eq!(json, "[]");
    }

    #[test]
    fn markdown_renders_header_separator_and_rows() {
        let md = render_resultset(
            &resultset(&["id", "name"], vec![vec!["1", "alice"]]),
            OutputFormat::Markdown,
        );
        assert_eq!(md, "| id | name |\n| --- | --- |\n| 1 | alice |");
    }

    #[test]
    fn markdown_escapes_pipes_and_newlines() {
        // 回归点：单元格里的 `|` 会撕裂 Markdown 表格，必须转义。
        let md = render_resultset(
            &resultset(&["expr"], vec![vec!["a|b"], vec!["l1\nl2"]]),
            OutputFormat::Markdown,
        );
        assert!(md.contains("a\\|b"), "pipe must be escaped: {md}");
        assert!(md.contains("l1<br>l2"), "newline must collapse: {md}");
        // 每行仍是恰好 3 个未转义的 `|`（两个边界 + 无额外列）。
        let data_line = md.lines().nth(2).expect("data line");
        assert_eq!(
            data_line.matches('|').count() - data_line.matches("\\|").count(),
            2
        );
    }

    #[test]
    fn markdown_renders_nothing_for_empty_columns() {
        assert_eq!(
            render_resultset(&Resultset::empty(), OutputFormat::Markdown),
            ""
        );
    }

    #[test]
    fn csv_format_matches_existing_renderer() {
        let rs = resultset(&["a", "b"], vec![vec!["1", "x,y"]]);
        assert_eq!(
            render_resultset(&rs, OutputFormat::Csv),
            crate::resultset::resultset_to_csv(&rs)
        );
    }
}
