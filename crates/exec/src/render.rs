//! The text format for statement results shared by the golden tests and the
//! `cairn` shell.
//!
//! - Rows: a tab-separated header line of column names, then one line per
//!   row. Values print as `NULL`, `TRUE`/`FALSE`, decimal integers, reals in
//!   canonical form (`1.0`, `1e300`), and raw text with `\` as `\\`, tab as
//!   `\t`, newline as `\n` and carriage return as `\r`. Column names are
//!   escaped the same way.
//! - Affected counts: `affected N`.
//! - Errors: `error: line L, column C: message`, or `error: message` when
//!   the error has no span.
//!
//! [`render_outcome`] joins blocks with a blank line and ends with a
//! newline.

use crate::database::QueryResult;
use crate::error::ExecError;
use crate::value::Value;

/// The whole outcome of [`crate::Database::execute_each`]: one block per
/// statement, or the single syntax error block, joined by blank lines and
/// followed by a newline.
pub fn render_outcome(outcome: &Result<Vec<Result<QueryResult, ExecError>>, ExecError>) -> String {
    let blocks: Vec<String> = match outcome {
        Ok(results) => results.iter().map(render_result).collect(),
        Err(error) => vec![render_error(error)],
    };
    let mut out = blocks.join("\n\n");
    out.push('\n');
    out
}

/// One result block, without a trailing newline.
pub fn render_result(result: &Result<QueryResult, ExecError>) -> String {
    match result {
        Ok(QueryResult::Affected(n)) => format!("affected {n}"),
        Ok(QueryResult::Rows { columns, rows }) => render_rows(columns, rows),
        Err(error) => render_error(error),
    }
}

/// `error: line L, column C: message`, or `error: message` without a span.
pub fn render_error(error: &ExecError) -> String {
    match error.span() {
        Some(span) => format!(
            "error: line {}, column {}: {}",
            span.line,
            span.column,
            error.message()
        ),
        None => format!("error: {}", error.message()),
    }
}

fn render_rows(columns: &[String], rows: &[Vec<Value>]) -> String {
    let mut out = columns
        .iter()
        .map(|c| escape(c))
        .collect::<Vec<_>>()
        .join("\t");
    for row in rows {
        out.push('\n');
        out.push_str(&row.iter().map(render_value).collect::<Vec<_>>().join("\t"));
    }
    out
}

fn render_value(value: &Value) -> String {
    match value {
        Value::Text(text) => escape(text),
        other => other.to_string(),
    }
}

fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Database;

    fn rows(columns: &[&str], rows: Vec<Vec<Value>>) -> Result<QueryResult, ExecError> {
        Ok(QueryResult::Rows {
            columns: columns.iter().map(|c| c.to_string()).collect(),
            rows,
        })
    }

    #[test]
    fn values_render_in_canonical_form() {
        let result = rows(
            &["a", "b", "c", "d", "e", "f", "g"],
            vec![vec![
                Value::Null,
                Value::Boolean(true),
                Value::Boolean(false),
                Value::Integer(-5),
                Value::Real(1.0),
                Value::Real(1e300),
                Value::Text("plain".to_string()),
            ]],
        );
        assert_eq!(
            render_result(&result),
            "a\tb\tc\td\te\tf\tg\nNULL\tTRUE\tFALSE\t-5\t1.0\t1e300\tplain"
        );
    }

    #[test]
    fn text_and_column_names_are_escaped() {
        let text = "a\\b\tc\nd\re".to_string();
        let result = rows(&["x\ty"], vec![vec![Value::Text(text)]]);
        assert_eq!(render_result(&result), "x\\ty\na\\\\b\\tc\\nd\\re");
    }

    #[test]
    fn header_only_rows_and_affected_counts() {
        assert_eq!(render_result(&rows(&["a", "b"], vec![])), "a\tb");
        assert_eq!(render_result(&Ok(QueryResult::Affected(3))), "affected 3");
    }

    #[test]
    fn errors_with_and_without_a_span() {
        let missing = std::env::temp_dir().join("cairn-render-missing-file.db");
        let Err(without) = Database::open(&missing) else {
            panic!("opening a missing file succeeded");
        };
        assert!(without.span().is_none());
        assert_eq!(
            render_error(&without),
            format!("error: {}", without.message())
        );

        let path = std::env::temp_dir().join(format!("cairn-render-{}.db", std::process::id()));
        let _ = std::fs::remove_file(&path);
        let mut db = Database::create(&path).expect("create");
        let outcome = db.execute_each("SELEC 1;");
        db.close().expect("close");
        let _ = std::fs::remove_file(&path);
        let Err(with) = &outcome else {
            panic!("syntax error expected");
        };
        assert!(render_error(with).starts_with("error: line 1, column 1: "));
        assert_eq!(
            render_outcome(&outcome),
            format!("{}\n", render_error(with))
        );
    }

    #[test]
    fn outcomes_join_blocks_with_a_blank_line() {
        let outcome = Ok(vec![
            Ok(QueryResult::Affected(1)),
            Ok(QueryResult::Affected(2)),
        ]);
        assert_eq!(render_outcome(&outcome), "affected 1\n\naffected 2\n");
        assert_eq!(render_outcome(&Ok(vec![])), "\n");
    }
}
