//! Rendering of statement results in the golden-test format, shared by the
//! golden runner and the README example test.

use cairn_exec::{ExecError, QueryResult, Value};

pub fn escape(text: &str) -> String {
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

fn render_value(value: &Value) -> String {
    match value {
        Value::Text(text) => escape(text),
        other => other.to_string(),
    }
}

fn render_error(error: &ExecError) -> String {
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

fn render_result(result: &Result<QueryResult, ExecError>) -> String {
    match result {
        Ok(QueryResult::Affected(n)) => format!("affected {n}"),
        Ok(QueryResult::Rows { columns, rows }) => {
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
        Err(error) => render_error(error),
    }
}

/// Renders the outcome of `Database::execute_each`.
pub fn render(outcome: &Result<Vec<Result<QueryResult, ExecError>>, ExecError>) -> String {
    let blocks: Vec<String> = match outcome {
        Ok(results) => results.iter().map(render_result).collect(),
        Err(error) => vec![render_error(error)],
    };
    let mut out = blocks.join("\n\n");
    out.push('\n');
    out
}
