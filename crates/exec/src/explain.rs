//! `EXPLAIN <select>` output. The `explain` marker itself is found by the
//! pre-pass in `prepass.rs`.
//!
//! [`render`] prints a plan as rows with columns `step` (0-based pre-order
//! index), `depth` (root is 0), `operation` and `detail`.

use std::ops::Bound;

use cairn_sql::Name;

use crate::database::QueryResult;
use crate::plan::{Access, Plan, TableAccess};
use crate::value::Value;

pub const COLUMNS: [&str; 4] = ["step", "depth", "operation", "detail"];

pub fn render(plan: &Plan) -> QueryResult {
    let mut rows = Vec::new();
    walk(plan, 0, &mut rows);
    let rows = rows
        .into_iter()
        .enumerate()
        .map(|(step, (depth, operation, detail))| {
            vec![
                Value::Integer(i64::try_from(step).unwrap_or(i64::MAX)),
                Value::Integer(depth),
                Value::Text(operation.to_string()),
                Value::Text(detail),
            ]
        })
        .collect();
    QueryResult::Rows {
        columns: COLUMNS.iter().map(ToString::to_string).collect(),
        rows,
    }
}

fn walk(plan: &Plan, depth: i64, rows: &mut Vec<(i64, &'static str, String)>) {
    let (operation, detail, children): (&str, String, Vec<&Plan>) = match plan {
        Plan::Values => ("VALUES", "1 row".to_string(), vec![]),
        Plan::Access(access) => access_row(access),
        Plan::NestedLoop {
            left, outer, inner, ..
        } => ("NESTED LOOP JOIN", join_kind(*left), vec![outer, inner]),
        Plan::IndexJoin {
            left,
            outer,
            label,
            probe,
            ..
        } => {
            let via = match &probe.index {
                None => "PRIMARY KEY".to_string(),
                Some(index) => format!("INDEX {}", Name(index.name.clone())),
            };
            let detail = format!("{} {label} USING {via} ({})", join_kind(*left), probe.text);
            ("INDEX JOIN", detail, vec![outer])
        }
        Plan::Filter { input, text, .. } => ("FILTER", text.clone(), vec![input]),
        Plan::Aggregate { input, text, .. } => ("AGGREGATE", text.clone(), vec![input]),
        Plan::Sort { input, text, .. } => ("SORT", text.clone(), vec![input]),
        Plan::Project { input, names, .. } => ("PROJECT", names.join(", "), vec![input]),
        Plan::Distinct { input } => ("DISTINCT", String::new(), vec![input]),
        Plan::Limit {
            input,
            count,
            offset,
        } => (
            "LIMIT",
            format!("limit {count} offset {offset}"),
            vec![input],
        ),
    };
    rows.push((depth, operation, detail));
    for child in children {
        walk(child, depth + 1, rows);
    }
}

fn join_kind(left: bool) -> String {
    if left { "LEFT" } else { "INNER" }.to_string()
}

fn access_row(access: &TableAccess) -> (&'static str, String, Vec<&'static Plan>) {
    let label = &access.label;
    let pk_name = || {
        access
            .table
            .pk
            .map(|p| Name(access.table.column_name(p).to_string()).to_string())
            .unwrap_or_default()
    };
    let column_name =
        |ordinal: usize| Name(access.table.column_name(ordinal).to_string()).to_string();
    match &access.access {
        Access::Scan => ("SCAN", label.clone(), vec![]),
        Access::PkLookup(value) => (
            "PRIMARY KEY LOOKUP",
            format!("{label} ({} = {})", pk_name(), value.to_sql_literal()),
            vec![],
        ),
        Access::PkRange(low, high) => (
            "PRIMARY KEY RANGE",
            format!("{label} ({})", range_text(&pk_name(), low, high)),
            vec![],
        ),
        Access::IndexLookup(index, value) => (
            "INDEX LOOKUP",
            format!(
                "{label} USING {} ({} = {})",
                Name(index.name.clone()),
                column_name(index.column),
                value.to_sql_literal()
            ),
            vec![],
        ),
        Access::IndexRange(index, low, high) => (
            "INDEX RANGE",
            format!(
                "{label} USING {} ({})",
                Name(index.name.clone()),
                range_text(&column_name(index.column), low, high)
            ),
            vec![],
        ),
    }
}

fn range_text(column: &str, low: &Bound<Value>, high: &Bound<Value>) -> String {
    let mut parts = Vec::new();
    match low {
        Bound::Included(v) => parts.push(format!("{column} >= {}", v.to_sql_literal())),
        Bound::Excluded(v) => parts.push(format!("{column} > {}", v.to_sql_literal())),
        Bound::Unbounded => {}
    }
    match high {
        Bound::Included(v) => parts.push(format!("{column} <= {}", v.to_sql_literal())),
        Bound::Excluded(v) => parts.push(format!("{column} < {}", v.to_sql_literal())),
        Bound::Unbounded => {}
    }
    parts.join(" AND ")
}
