//! SQL 辅助函数：表名校验（防注入）、SELECT 判断与只读查询白名单。

use anyhow::{Result, bail};

/// 判断是否为合法的 SQL 标识符（字母、数字、下划线）。
fn is_identifier(s: &str) -> bool {
    !s.is_empty() && s.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_')
}

/// 校验并转义表/视图名，防止 SQL 注入。
///
/// 仅允许 `table` 或 `schema.table` 两种形式，返回方括号转义后的名称，
/// 例如 `dbo.users` -> `[dbo].[users]`。
pub fn validate_table_name(table_name: &str) -> Result<String> {
    let parts: Vec<&str> = table_name.split('.').collect();
    let bracketed = match parts.as_slice() {
        [table] if is_identifier(table) => format!("[{table}]"),
        [schema, table] if is_identifier(schema) && is_identifier(table) => {
            format!("[{schema}].[{table}]")
        }
        _ => bail!("Invalid table name: {table_name}"),
    };
    Ok(bracketed)
}

/// 解析 `table` 或 `schema.table` 为 `(schema, table)`，仅接受合法标识符。
/// 与 `validate_table_name` 使用同一套校验规则，但保留结构信息供参数化查询使用。
pub fn parse_table_name(table_name: &str) -> Result<(Option<String>, String)> {
    let parts: Vec<&str> = table_name.split('.').collect();
    match parts.as_slice() {
        [table] if is_identifier(table) => Ok((None, table.to_string())),
        [schema, table] if is_identifier(schema) && is_identifier(table) => {
            Ok((Some(schema.to_string()), table.to_string()))
        }
        _ => bail!("Invalid table name: {table_name}"),
    }
}

/// 判断查询是否为对 `INFORMATION_SCHEMA.TABLES` 的查询（输出格式特判）。
///
/// 判定走词法分析而非子串搜索：必须跳过注释**与字符串字面量**，否则
/// 一条普通查询只要在注释或字符串里提到 `INFORMATION_SCHEMA.TABLES`，
/// 就会被误判成表清单查询而输出 mysql 风格的 `Tables_in_{database}` 表头，
/// 丢掉真正的列名。这也保证它与 [`is_read_only_query`] 共用同一套词法，
/// 不会出现两套注释/字符串处理规则各自解释一段 SQL。
pub fn is_tables_listing_query(query: &str) -> bool {
    let tokens = tokenize(query);
    if !tokens
        .first()
        .is_some_and(|token| matches!(token, Token::Ident(word) if word == "select"))
    {
        return false;
    }
    // `INFORMATION_SCHEMA.TABLES` 被切成「标识符 + Opaque(点) + 标识符」
    // 三段，故按这个形状匹配。
    tokens.windows(3).any(|window| {
        matches!(
            window,
            [
                Token::Ident(schema),
                Token::Opaque,
                Token::Ident(table)
            ] if schema == "information_schema" && table == "tables"
        )
    })
}

/// 判断语句是否为只读查询：单条语句，且顶层语句为 `SELECT`，
/// 或 `WITH name [(列)] AS (子查询) [, ...] SELECT ...` 形式的 CTE 查询，
/// 且不含写入型语法（见 [`has_write_side_effect`]）。
///
/// 只读保护的核心：INSERT/UPDATE/DELETE/DDL/EXEC 等修改语句、
/// 多语句批次（`;` 之后还有内容）、以及藏在字符串/注释里的分号
/// 都会被正确识别并拒绝。
pub fn is_read_only_query(query: &str) -> bool {
    let tokens = tokenize(query);
    !has_multiple_statements(&tokens)
        && starts_with_read_only_statement(&tokens)
        && !has_write_side_effect(&tokens)
}

/// 识别「语法上是 SELECT、实际会写入」的语句。
///
/// 仅看首关键字会把下面两类放行，它们都真的改了数据库：
///
/// - `SELECT ... INTO <新表> FROM ...`：建表并插入数据（DDL + DML）。
///   这里拒绝**任何** `INTO` 词元——T-SQL 中 `INTO` 只出现在 `SELECT INTO`
///   与 `INSERT INTO` 两种写入语法里，不存在只读用法，故无需判断括号深度。
///   字符串与 `[...]` 括起标识符已被词法分析折叠成 [`Token::Opaque`]，
///   因此 `SELECT 'into' AS x`、`SELECT [into] FROM t` 不会误伤。
/// - `SELECT NEXT VALUE FOR <序列>`：**推进**序列对象，是带副作用的读。
///   三连词 `next value for` 无歧义，不存在只读用法。
fn has_write_side_effect(tokens: &[Token]) -> bool {
    tokens
        .iter()
        .any(|token| matches!(token, Token::Ident(word) if word == "into"))
        || tokens.windows(3).any(|window| {
            matches!(
                window,
                [Token::Ident(a), Token::Ident(b), Token::Ident(c)]
                    if a == "next" && b == "value" && c == "for"
            )
        })
}

/// 词法单元：只保留判断语句结构所需的类别。
#[derive(Debug, Clone, PartialEq)]
enum Token {
    /// 标识符或关键字（已小写化）。
    Ident(String),
    OpenParen,
    CloseParen,
    Comma,
    Semicolon,
    /// 字符串字面量、`[...]`/`"..."` 括起标识符、数字、运算符等
    /// 不参与结构判断的内容。
    Opaque,
}

/// 词法扫描：跳过注释与字面量，识别标识符与结构性符号。
fn tokenize(input: &str) -> Vec<Token> {
    let chars: Vec<char> = input.chars().collect();
    let mut tokens = Vec::new();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        if c.is_whitespace() {
            i += 1;
        } else if c == '-' && chars.get(i + 1) == Some(&'-') {
            i = skip_line_comment(&chars, i + 2);
        } else if c == '/' && chars.get(i + 1) == Some(&'*') {
            i = skip_block_comment(&chars, i + 2);
        } else if matches!(c, '\'' | '"' | '[') {
            i = skip_quoted(&chars, i, if c == '[' { ']' } else { c });
            tokens.push(Token::Opaque);
        } else if c.is_ascii_alphabetic() || matches!(c, '_' | '@' | '#') {
            let mut end = i;
            while end < chars.len()
                && (chars[end].is_ascii_alphanumeric()
                    || matches!(chars[end], '_' | '@' | '#' | '$'))
            {
                end += 1;
            }
            tokens.push(Token::Ident(
                chars[i..end]
                    .iter()
                    .collect::<String>()
                    .to_ascii_lowercase(),
            ));
            i = end;
        } else {
            tokens.push(match c {
                '(' => Token::OpenParen,
                ')' => Token::CloseParen,
                ',' => Token::Comma,
                ';' => Token::Semicolon,
                _ => Token::Opaque,
            });
            i += 1;
        }
    }
    tokens
}

/// 跳过 `--` 行注释（从 `--` 之后开始），返回下一有效字符的索引。
fn skip_line_comment(chars: &[char], from: usize) -> usize {
    chars[from..]
        .iter()
        .position(|&c| c == '\n')
        .map_or(chars.len(), |pos| from + pos + 1)
}

/// 跳过 `/* ... */` 块注释（从 `/*` 之后开始，未闭合则吞到结尾）。
fn skip_block_comment(chars: &[char], from: usize) -> usize {
    chars[from..]
        .windows(2)
        .position(|w| w[0] == '*' && w[1] == '/')
        .map_or(chars.len(), |pos| from + pos + 2)
}

/// 跳过字面量（`'...'`、`"..."`、`[...]`），引号加倍（`''`、`]]`）视为转义；
/// 未闭合则吞到结尾。
fn skip_quoted(chars: &[char], start: usize, quote: char) -> usize {
    let mut i = start + 1;
    while i < chars.len() {
        if chars[i] == quote {
            if chars.get(i + 1) == Some(&quote) {
                i += 2;
            } else {
                return i + 1;
            }
        } else {
            i += 1;
        }
    }
    chars.len()
}

/// 分号之后还有内容即视为多语句（单个尾随分号允许）。
fn has_multiple_statements(tokens: &[Token]) -> bool {
    tokens
        .iter()
        .position(|t| *t == Token::Semicolon)
        .is_some_and(|pos| pos + 1 < tokens.len())
}

/// 顶层语句白名单：`SELECT ...` 或 `WITH name [(列)] AS (体) [, ...] SELECT ...`。
/// 解析失败（残缺 CTE、其他关键字、括号不平衡）一律拒绝。
fn starts_with_read_only_statement(tokens: &[Token]) -> bool {
    match tokens.first() {
        Some(Token::Ident(w)) if w == "select" => return true,
        Some(Token::Ident(w)) if w == "with" => {}
        _ => return false,
    }
    let mut i = 1;
    loop {
        // CTE 名称
        match tokens.get(i) {
            Some(Token::Ident(_)) => i += 1,
            _ => return false,
        }
        // 可选列清单：name (a, b)
        if matches!(tokens.get(i), Some(Token::OpenParen)) {
            i = match skip_paren_block(tokens, i) {
                Some(end) => end,
                None => return false,
            };
        }
        // AS
        match tokens.get(i) {
            Some(Token::Ident(w)) if w == "as" => i += 1,
            _ => return false,
        }
        // CTE 主体（必须为括号块）
        if !matches!(tokens.get(i), Some(Token::OpenParen)) {
            return false;
        }
        i = match skip_paren_block(tokens, i) {
            Some(end) => end,
            None => return false,
        };
        // 逗号 → 下一个 CTE；顶层主语句必须是 SELECT
        match tokens.get(i) {
            Some(Token::Comma) => i += 1,
            Some(Token::Ident(w)) if w == "select" => return true,
            _ => return false,
        }
    }
}

/// 从指向 `(` 的 `start` 跳过整个平衡括号块，返回块之后的索引；不平衡返回 None。
fn skip_paren_block(tokens: &[Token], start: usize) -> Option<usize> {
    let mut depth: usize = 0;
    for (i, token) in tokens.iter().enumerate().skip(start) {
        match token {
            Token::OpenParen => depth += 1,
            Token::CloseParen => {
                depth = depth.checked_sub(1)?;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            _ => {}
        }
    }
    None
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
        assert_eq!(validate_table_name("dbo.users").unwrap(), "[dbo].[users]");
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
    fn parse_table_name_accepts_plain_table() {
        assert_eq!(
            parse_table_name("users").unwrap(),
            (None, "users".to_string())
        );
    }

    #[test]
    fn parse_table_name_accepts_qualified_table() {
        assert_eq!(
            parse_table_name("dbo.users").unwrap(),
            (Some("dbo".to_string()), "users".to_string())
        );
    }

    #[test]
    fn parse_table_name_rejects_invalid() {
        assert!(parse_table_name("").is_err());
        assert!(parse_table_name("a.b.c").is_err());
        assert!(parse_table_name("users; DROP TABLE x").is_err());
        assert!(parse_table_name("dbo.").is_err());
        assert!(parse_table_name("user-name").is_err());
    }

    #[test]
    fn is_tables_listing_query_needs_unbracketed_dotted_name() {
        // 已知取舍：方括号写法 `[INFORMATION_SCHEMA].[TABLES]` 的词元形状是
        // 一串 Opaque（整个括起标识符被折叠），不匹配「标识符+点+标识符」，
        // 因此不触发 mysql 风格表头——用户会拿到正常的列名与数据。
        // 相比旧的子串实现会把注释/字符串里的提词也算上，这是保守方向。
        for query in [
            "SELECT TABLE_NAME FROM [INFORMATION_SCHEMA].[TABLES]",
            "SELECT * FROM [information_schema].[tables]",
        ] {
            assert!(
                !is_tables_listing_query(query),
                "bracketed form is not treated as a listing query: {query}"
            );
        }
        // 未加方括号的常见写法照常识别。
        assert!(is_tables_listing_query(
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES"
        ));
    }

    #[test]
    fn is_tables_listing_query_requires_select_as_first_statement() {
        // 「首词必须是 SELECT」这一条件由 `is_tables_listing_query` 承担：
        // 非 SELECT 语句即使提到目标视图也不算表清单查询。
        for query in [
            "UPDATE t SET x = 1 -- INFORMATION_SCHEMA.TABLES",
            "INSERT INTO INFORMATION_SCHEMA.TABLES VALUES (1)",
            "DELETE FROM t /* INFORMATION_SCHEMA.TABLES */",
        ] {
            assert!(!is_tables_listing_query(query), "should reject: {query}");
        }
    }

    #[test]
    fn is_tables_listing_query_handles_leading_comments() {
        // 前置注释不影响判定（词法分析会跳过）。
        for query in [
            "-- pick tables\nSELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES",
            "/* block */ SELECT * FROM information_schema.tables",
            "/* multi\nline */ SELECT 1 FROM INFORMATION_SCHEMA.TABLES",
        ] {
            assert!(is_tables_listing_query(query), "should detect: {query}");
        }
    }

    #[test]
    fn is_tables_listing_query_ignores_mentions_in_string_literals() {
        // 回归点：旧实现按子串搜索清洗后的文本，字符串里的提词会误触发
        // mysql 风格表头，丢掉真正的列名。词法分析已把字面量折成 Opaque。
        for query in [
            "SELECT 'INFORMATION_SCHEMA.TABLES' AS note",
            "SELECT * FROM t WHERE n = 'read INFORMATION_SCHEMA.TABLES first'",
            "SELECT [INFORMATION_SCHEMA.TABLES] FROM t",
        ] {
            assert!(!is_tables_listing_query(query), "should reject: {query}");
        }
    }

    #[test]
    fn is_tables_listing_query_rejects_empty_and_comment_only() {
        assert!(!is_tables_listing_query("   \n "));
        assert!(!is_tables_listing_query("-- only a comment"));
        assert!(!is_tables_listing_query(""));
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

    #[test]
    fn is_read_only_query_accepts_select_statements() {
        assert!(is_read_only_query("SELECT * FROM users"));
        assert!(is_read_only_query("  select 1"));
        assert!(is_read_only_query("SELECT TOP 100 * FROM [dbo].[users];"));
        assert!(is_read_only_query("-- comment\n/* block */ SELECT 1"));
        assert!(is_read_only_query("SELECT 'a--b' AS c, N'x' AS n"));
    }

    #[test]
    fn is_read_only_query_accepts_cte_selects() {
        assert!(is_read_only_query(
            "WITH t AS (SELECT 1 AS x) SELECT * FROM t"
        ));
        assert!(is_read_only_query(
            "with a(x) as (select 1), b as (select 2) select * from a join b on a.x = b.x"
        ));
        // CTE 主体内嵌套括号与注释
        assert!(is_read_only_query(
            "WITH t AS (SELECT * FROM (SELECT 1 /* inner */ AS v) nested) SELECT * FROM t"
        ));
    }

    #[test]
    fn is_read_only_query_rejects_modifying_statements() {
        for query in [
            "INSERT INTO t VALUES (1)",
            "UPDATE t SET x = 1",
            "DELETE FROM t",
            "MERGE INTO t USING s ON t.id = s.id",
            "DROP TABLE t",
            "CREATE TABLE t (id INT)",
            "ALTER TABLE t ADD c INT",
            "TRUNCATE TABLE t",
            "GRANT SELECT ON t TO u",
            "EXEC sp_help",
            "EXECUTE sp_help",
            "USE master",
            "BEGIN TRANSACTION",
            "COMMIT",
            "BACKUP DATABASE master TO DISK = 'x'",
        ] {
            assert!(!is_read_only_query(query), "should reject: {query}");
        }
    }

    #[test]
    fn is_read_only_query_rejects_cte_modifiers() {
        assert!(!is_read_only_query(
            "WITH t AS (SELECT 1) INSERT INTO x SELECT * FROM t"
        ));
        assert!(!is_read_only_query("WITH t AS (SELECT 1) DELETE FROM x"));
        assert!(!is_read_only_query(
            "WITH t AS (SELECT 1) UPDATE x SET a = 1"
        ));
    }

    #[test]
    fn is_read_only_query_rejects_multiple_statements() {
        assert!(!is_read_only_query("SELECT 1; DROP TABLE x"));
        assert!(!is_read_only_query("SELECT 1; SELECT 2"));
        assert!(!is_read_only_query(
            "WITH t AS (SELECT 1) SELECT * FROM t; TRUNCATE TABLE x"
        ));
        // 单个尾随分号允许
        assert!(is_read_only_query("SELECT 1;"));
    }

    #[test]
    fn is_read_only_query_ignores_semicolons_and_comments_in_literals() {
        // 字符串里的分号/注释符号不构成语句边界
        assert!(is_read_only_query("SELECT ';' AS sep"));
        assert!(is_read_only_query("SELECT '/*' AS c"));
        assert!(is_read_only_query("SELECT 1 -- ; DROP TABLE x"));
        assert!(is_read_only_query("SELECT 1 /* ; DROP TABLE x */"));
        // 字符串里的注释开头不能掩盖后续语句
        assert!(!is_read_only_query("SELECT '/*'; DROP TABLE x"));
        assert!(!is_read_only_query("SELECT '--'; DROP TABLE x"));
    }

    #[test]
    fn is_read_only_query_rejects_select_into() {
        // 回归点：`SELECT ... INTO` 是 DDL + DML（建表并写入），
        // 首关键字是 SELECT 因此此前被放行——实际上完全绕过了只读保护。
        for query in [
            "SELECT * INTO archive FROM users",
            "select id into #tmp from users",
            "SELECT * INTO sales.orders_backup FROM sales.orders",
            // CTE 之后接 SELECT INTO 同样要拦住
            "WITH c AS (SELECT * FROM users) SELECT * INTO copied FROM c",
        ] {
            assert!(!is_read_only_query(query), "should reject: {query}");
        }
    }

    #[test]
    fn is_read_only_query_rejects_next_value_for() {
        // `NEXT VALUE FOR` 会推进序列对象，是带副作用的读。
        assert!(!is_read_only_query("SELECT NEXT VALUE FOR dbo.order_seq"));
        assert!(!is_read_only_query(
            "SELECT NEXT VALUE FOR dbo.order_seq AS id, name FROM users"
        ));
    }

    #[test]
    fn is_read_only_query_allows_into_in_literals_and_identifiers() {
        // 回归点：拒绝 INTO 不能误伤字符串/括起标识符/列名里的这个词。
        for query in [
            "SELECT 'into' AS note",
            "SELECT [into] FROM t",
            "SELECT * FROM t WHERE note = 'take into account'",
        ] {
            assert!(is_read_only_query(query), "should allow: {query}");
        }
    }

    #[test]
    fn is_read_only_query_still_allows_plain_selects() {
        // 防过度拦截：加了新的拒绝规则后，常规 SELECT 必须仍然通过。
        for query in [
            "SELECT * FROM users",
            "SELECT TOP 100 * FROM [dbo].[users];",
            "WITH t AS (SELECT 1 AS x) SELECT * FROM t",
            "SELECT COUNT(*) FROM users WHERE region = 'East'",
            // `INTO` 不作为独立词元出现的普通查询
            "SELECT * FROM information_schema.tables",
        ] {
            assert!(is_read_only_query(query), "should allow: {query}");
        }
    }

    #[test]
    fn is_tables_listing_query_ignores_mentions_in_comments() {
        // 回归点：此前用未清洗文本匹配，注释里提到就误触发 mysql 风格表头。
        assert!(!is_tables_listing_query(
            "SELECT 1 -- INFORMATION_SCHEMA.TABLES mentioned here"
        ));
        assert!(!is_tables_listing_query(
            "SELECT 1 /* INFORMATION_SCHEMA.TABLES */"
        ));
        assert!(!is_tables_listing_query(
            "-- INFORMATION_SCHEMA.TABLES\nSELECT 1"
        ));
    }

    #[test]
    fn is_tables_listing_query_still_detects_real_queries() {
        assert!(is_tables_listing_query(
            "SELECT TABLE_NAME FROM INFORMATION_SCHEMA.TABLES WHERE TABLE_TYPE = 'BASE TABLE'"
        ));
        assert!(is_tables_listing_query(
            "select * from information_schema.tables"
        ));
        assert!(!is_tables_listing_query("SELECT * FROM users"));
    }

    #[test]
    fn is_read_only_query_rejects_empty_and_malformed() {
        assert!(!is_read_only_query(""));
        assert!(!is_read_only_query("   \n "));
        assert!(!is_read_only_query("-- only a comment"));
        assert!(!is_read_only_query("WITH t AS (SELECT 1)"));
        assert!(!is_read_only_query("WITH t (SELECT 1) SELECT 1"));
        assert!(!is_read_only_query("WITH SELECT 1"));
    }
}
