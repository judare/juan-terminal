//! Small SQL lexer for highlighting (no tree-sitter SQL grammar is bundled): keywords,
//! types, functions, strings, quoted identifiers, numbers, comments, parameters.

use crate::highlight::HighlightSpan;
use crate::language::HIGHLIGHT_NAMES;

const KEYWORDS: &[&str] = &[
    "add", "all", "alter", "and", "any", "as", "asc", "begin", "between", "by", "cascade", "case", "check", "column", "commit", "constraint", "create",
    "cross", "database", "default", "delete", "desc", "distinct", "do", "drop", "else", "end", "except", "exists", "explain", "foreign", "from", "full",
    "function", "grant", "group", "having", "if", "ilike", "in", "index", "inner", "insert", "intersect", "into", "is", "join", "key", "language", "left",
    "like", "limit", "materialized", "not", "null", "offset", "on", "or", "order", "outer", "over", "partition", "primary", "procedure", "references",
    "replace", "returning", "returns", "revoke", "right", "rollback", "schema", "select", "sequence", "set", "table", "then", "to", "transaction",
    "trigger", "truncate", "union", "unique", "update", "using", "values", "view", "when", "where", "with", "window", "conflict", "nothing", "lateral",
    "recursive", "temporary", "temp", "unlogged", "extension", "type", "enum", "owner", "role", "user", "privileges", "public", "for", "each", "row",
    "execute", "declare", "loop", "return", "raise", "notice", "exception", "while", "elsif", "elseif", "auto_increment", "engine", "charset", "collate",
];
const TYPES: &[&str] = &[
    "bigint", "bigserial", "binary", "bit", "blob", "bool", "boolean", "bytea", "char", "character", "date", "datetime", "decimal", "double", "float",
    "int", "integer", "interval", "json", "jsonb", "money", "numeric", "real", "serial", "smallint", "text", "time", "timestamp", "timestamptz", "tinyint",
    "uuid", "varchar", "varying", "precision", "zone", "array", "inet", "citext", "mediumtext", "longtext",
];
const CONSTANTS: &[&str] = &["true", "false", "null", "current_date", "current_time", "current_timestamp", "now"];

fn kind(name: &str) -> u16 {
    HIGHLIGHT_NAMES.iter().position(|n| *n == name).unwrap_or(0) as u16
}

pub fn highlight_sql(src: &str) -> Vec<HighlightSpan> {
    let (k_kw, k_type, k_const, k_str, k_num, k_comment, k_fn, k_var, k_op) =
        (kind("keyword"), kind("type"), kind("constant.builtin"), kind("string"), kind("number"), kind("comment"), kind("function"), kind("variable.parameter"), kind("operator"));
    let b = src.as_bytes();
    let mut spans = Vec::new();
    let mut i = 0;
    let push = |start: usize, end: usize, k: u16, spans: &mut Vec<HighlightSpan>| spans.push(HighlightSpan { start, end, kind: k });
    while i < b.len() {
        let c = b[i];
        // -- line comment / # (MySQL)
        if (c == b'-' && b.get(i + 1) == Some(&b'-')) || c == b'#' {
            let end = src[i..].find('\n').map(|n| i + n).unwrap_or(b.len());
            push(i, end, k_comment, &mut spans);
            i = end;
        } else if c == b'/' && b.get(i + 1) == Some(&b'*') {
            let end = src[i + 2..].find("*/").map(|n| i + 2 + n + 2).unwrap_or(b.len());
            push(i, end, k_comment, &mut spans);
            i = end;
        } else if c == b'\'' || c == b'"' || c == b'`' {
            // '' and "" escape themselves.
            let q = c;
            let mut j = i + 1;
            while j < b.len() {
                if b[j] == q {
                    if b.get(j + 1) == Some(&q) {
                        j += 2;
                        continue;
                    }
                    j += 1;
                    break;
                }
                j += 1;
            }
            // Double quotes / backticks are identifiers, single quotes are strings.
            push(i, j, if q == b'\'' { k_str } else { k_var }, &mut spans);
            i = j;
        } else if c == b'$' && b.get(i + 1).is_some_and(|d| d.is_ascii_digit()) || c == b':' && b.get(i + 1).is_some_and(|d| d.is_ascii_alphabetic()) && b.get(i.wrapping_sub(1)) != Some(&b':') || c == b'?' {
            // Parameters: $1, :name, ?
            let mut j = i + 1;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            push(i, j, k_var, &mut spans);
            i = j;
        } else if c.is_ascii_digit() {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_digit() || b[j] == b'.') {
                j += 1;
            }
            push(i, j, k_num, &mut spans);
            i = j;
        } else if c.is_ascii_alphabetic() || c == b'_' {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
                j += 1;
            }
            let word = src[i..j].to_ascii_lowercase();
            // An identifier followed by "(" is a function call.
            let next = src[j..].trim_start().as_bytes().first().copied();
            let k = if CONSTANTS.contains(&word.as_str()) {
                Some(k_const)
            } else if TYPES.contains(&word.as_str()) {
                Some(k_type)
            } else if KEYWORDS.contains(&word.as_str()) {
                Some(k_kw)
            } else if next == Some(b'(') {
                Some(k_fn)
            } else {
                None
            };
            if let Some(k) = k {
                push(i, j, k, &mut spans);
            }
            i = j;
        } else if b"=<>!+-*/%|&".contains(&c) {
            push(i, i + 1, k_op, &mut spans);
            i += 1;
        } else {
            // Skip a whole UTF-8 char.
            i += src[i..].chars().next().map(|ch| ch.len_utf8()).unwrap_or(1);
        }
    }
    spans
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str) -> Vec<(String, &'static str)> {
        highlight_sql(src).iter().map(|s| (src[s.start..s.end].to_string(), HIGHLIGHT_NAMES[s.kind as usize])).collect()
    }

    #[test]
    fn highlights_common_sql() {
        let k = kinds("SELECT id, count(*) FROM \"users\" WHERE name = 'O''Brien' AND age > 30 -- adults\n/* x */ LIMIT $1;");
        assert!(k.contains(&("SELECT".into(), "keyword")), "{k:?}");
        assert!(k.contains(&("count".into(), "function")));
        assert!(k.contains(&("\"users\"".into(), "variable.parameter")));
        assert!(k.contains(&("'O''Brien'".into(), "string")));
        assert!(k.contains(&("30".into(), "number")));
        assert!(k.contains(&("-- adults".into(), "comment")));
        assert!(k.contains(&("/* x */".into(), "comment")));
        assert!(k.contains(&("$1".into(), "variable.parameter")));
        assert!(!k.iter().any(|(t, _)| t == "id"));
        let k = kinds("CREATE TABLE t (id uuid PRIMARY KEY DEFAULT now(), ok boolean NOT NULL)");
        assert!(k.contains(&("uuid".into(), "type")) && k.contains(&("now".into(), "constant.builtin")) && k.contains(&("NULL".into(), "constant.builtin")));
        // Unterminated string and non-ASCII don't panic.
        let _ = kinds("SELECT 'ñandú");
    }
}
