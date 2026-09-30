//! SQL 辅助函数：表名校验（防注入）与 SELECT 语句判断。

/// 判断是否为合法的 SQL 标识符（字母、数字、下划线）。
fn is_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// 校验并转义表名，防止 SQL 注入。
///
/// 仅允许 `table` 或 `schema.table` 两种形式，返回方括号转义后的名称，
/// 例如 `dbo.users` -> `[dbo].[users]`。
pub fn validate_table_name(table_name: &str) -> Result<String, String> {
    let parts: Vec<&str> = table_name.split('.').collect();
    let bracketed = match parts.as_slice() {
        [table] if is_identifier(table) => format!("[{table}]"),
        [schema, table] if is_identifier(schema) && is_identifier(table) => {
            format!("[{schema}].[{table}]")
        }
        _ => return Err(format!("Invalid table name: {table_name}")),
    };
    Ok(bracketed)
}

/// 移除 SQL 块注释 `/* ... */`（未闭合的注释吞掉其后全部内容）。
fn strip_block_comments(input: &str) -> String {
    let mut result = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("/*") {
        result.push_str(&rest[..start]);
        match rest[start + 2..].find("*/") {
            Some(end) => rest = &rest[start + 2 + end + 2..],
            None => return result,
        }
    }
    result.push_str(rest);
    result
}

/// 移除单行注释 `-- ...`（每行 `--` 之后的内容）。
fn strip_line_comments(input: &str) -> String {
    input
        .lines()
        .map(|line| match line.find("--") {
            Some(pos) => &line[..pos],
            None => line,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 判断查询是否为 SELECT 语句，正确处理 `--` 与 `/* */` 注释。
pub fn is_select_query(query: &str) -> bool {
    let cleaned = strip_line_comments(&strip_block_comments(query));
    cleaned
        .split_whitespace()
        .next()
        .is_some_and(|word| word.eq_ignore_ascii_case("select"))
}

/// 判断查询是否为对 `INFORMATION_SCHEMA.TABLES` 的查询（输出格式特判）。
pub fn is_tables_listing_query(query: &str) -> bool {
    is_select_query(query) && query.to_ascii_uppercase().contains("INFORMATION_SCHEMA.TABLES")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validate_table_name_accepts_plain_table() {
        assert_eq!(validate_table_name("users").unwrap(), "[users]");
    }

    #[test]
    fn validate_table_name_accepts_qualified_table() {
        assert_eq!(
            validate_table_name("dbo.users").unwrap(),
            "[dbo].[users]"
        );
    }

    #[test]
    fn validate_table_name_rejects_too_many_parts() {
        assert!(validate_table_name("a.b.c").is_err());
    }

    #[test]
    fn validate_table_name_rejects_injection() {
        assert!(validate_table_name("users; DROP TABLE x").is_err());
        assert!(validate_table_name("user-name").is_err());
        assert!(validate_table_name("").is_err());
        assert!(validate_table_name("dbo.").is_err());
    }

    #[test]
    fn is_select_query_detects_select() {
        assert!(is_select_query("SELECT * FROM t"));
        assert!(is_select_query("  select 1"));
        assert!(is_select_query("SELECT TOP 100 * FROM t"));
    }

    #[test]
    fn is_select_query_ignores_comments() {
        assert!(is_select_query("-- leading comment\nSELECT 1"));
        assert!(is_select_query("/* block */ SELECT 1"));
        assert!(is_select_query("/* multi\nline */ SELECT 1"));
    }

    #[test]
    fn is_select_query_rejects_non_select() {
        assert!(!is_select_query("UPDATE t SET x = 1"));
        assert!(!is_select_query("INSERT INTO t VALUES (1)"));
        assert!(!is_select_query("DELETE FROM t"));
    }

    #[test]
    fn is_select_query_rejects_empty_and_comment_only() {
        assert!(!is_select_query("   \n "));
        assert!(!is_select_query("-- only a comment"));
    }

    #[test]
    fn is_tables_listing_query_matches_only_select_on_information_schema() {
        assert!(is_tables_listing_query(
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_TYPE = 'BASE TABLE'"
        ));
        assert!(!is_tables_listing_query(
            "DROP TABLE INFORMATION_SCHEMA.TABLES"
        ));
        assert!(!is_tables_listing_query("SELECT * FROM users"));
    }
}
