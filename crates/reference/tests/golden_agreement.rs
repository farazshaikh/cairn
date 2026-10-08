//! The reference evaluator against the golden scripts (milestone M6 AC3).
//!
//! Every `tests/sql/*.sql` script outside the EXPLAIN and PRAGMA scripts
//! runs through [`Engine::run_script`]. Its blocks are matched against the
//! `.expected` file, which cairn produced:
//!
//! - rows and `affected` blocks must be byte-identical;
//! - an error block must be an error on both sides (the reference has no
//!   messages or positions);
//! - a [`Outcome::Skipped`] block (EXPLAIN, PRAGMA) is skipped up to the
//!   next blank line.
//!
//! The expected text is consumed line by line using the reference block
//! sizes, so text that contains blank lines cannot shift the alignment.

use std::fs;
use std::path::PathBuf;

use cairn_reference::{Engine, Outcome, render_outcome};

/// Scripts whose subject is EXPLAIN or PRAGMA output.
const EXCLUDED: [(&str, &str); 6] = [
    ("039_explain_primary_key", "EXPLAIN output"),
    ("040_explain_index", "EXPLAIN output"),
    ("041_explain_scan", "EXPLAIN output"),
    ("042_explain_join", "EXPLAIN output"),
    ("048_explain_errors", "EXPLAIN output"),
    ("051_pragmas", "PRAGMA output"),
];

const MIN_COMPARED: usize = 40;

fn sql_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../tests/sql")
}

/// Compares one script; returns the number of compared rows or affected
/// blocks, or a description of the first mismatch.
fn compare(sql: &str, expected: &str) -> Result<usize, String> {
    let lines: Vec<&str> = expected.split('\n').collect();
    let outcomes = match Engine::new().run_script(sql) {
        Ok(outcomes) => outcomes,
        Err(kind) => {
            return match lines.as_slice() {
                [only, ""] if only.starts_with("error: ") => Ok(0),
                _ => Err(format!(
                    "reference: whole-script {kind:?}, expected:\n{expected}"
                )),
            };
        }
    };
    let mut at = 0;
    let mut compared = 0;
    for (n, outcome) in outcomes.iter().enumerate() {
        if n > 0 {
            if lines.get(at) != Some(&"") {
                return Err(format!(
                    "block {n}: no blank line before it at line {}",
                    at + 1
                ));
            }
            at += 1;
        }
        match outcome {
            Outcome::Error(kind) => {
                let line = lines.get(at).copied().unwrap_or("");
                if !line.starts_with("error: ") {
                    return Err(format!("block {n}: reference {kind:?}, expected `{line}`"));
                }
                at += 1;
            }
            Outcome::Skipped => {
                while lines.get(at).is_some_and(|l| !l.is_empty()) {
                    at += 1;
                }
            }
            _ => {
                let mine = render_outcome(outcome);
                let count = mine.split('\n').count();
                let theirs = lines.get(at..at + count).map(|l| l.join("\n"));
                if theirs.as_deref() != Some(mine.as_str()) {
                    return Err(format!(
                        "block {n}: reference\n{mine}\nexpected\n{}",
                        theirs.unwrap_or_else(|| "<end of file>".into())
                    ));
                }
                at += count;
                compared += 1;
            }
        }
    }
    if lines.get(at..) != Some(&[""][..]) {
        return Err(format!("expected has more output from line {}", at + 1));
    }
    Ok(compared)
}

#[test]
fn the_comparison_rejects_changed_rows_errors_and_extra_output() {
    let sql = "CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1); SELECT a FROM t; SELECT 1 / 0";
    let good = "affected 0\n\naffected 1\n\na\n1\n\nerror: line 1, column 74: division by zero\n";
    assert_eq!(compare(sql, good), Ok(3));
    let changed_row = good.replace("a\n1\n", "a\n2\n");
    assert!(compare(sql, &changed_row).is_err());
    let error_became_rows = good.replace("error: line 1, column 74: division by zero", "x\n1");
    assert!(compare(sql, &error_became_rows).is_err());
    assert!(compare(sql, &format!("{good}\naffected 0\n")).is_err());
    assert!(compare(sql, "error: line 1, column 1: syntax\n").is_err());
}

#[test]
fn reference_agrees_with_every_golden_script() {
    let mut scripts: Vec<PathBuf> = fs::read_dir(sql_dir())
        .expect("tests/sql")
        .map(|e| e.expect("entry").path())
        .filter(|p| p.extension().is_some_and(|x| x == "sql"))
        .collect();
    scripts.sort();
    let mut report = String::new();
    let mut failures = Vec::new();
    let mut scripts_compared = 0;
    let mut excluded = Vec::new();
    for path in &scripts {
        let stem = path.file_stem().and_then(|s| s.to_str()).expect("stem");
        if let Some((_, reason)) = EXCLUDED.iter().find(|(name, _)| *name == stem) {
            report.push_str(&format!("{stem:32} excluded: {reason}\n"));
            excluded.push(stem.to_string());
            continue;
        }
        let sql = fs::read_to_string(path).expect("read sql");
        let expected = fs::read_to_string(path.with_extension("expected")).expect("read expected");
        match compare(&sql, &expected) {
            Ok(blocks) => {
                report.push_str(&format!("{stem:32} ok, {blocks} blocks compared\n"));
                if blocks > 0 {
                    scripts_compared += 1;
                }
            }
            Err(problem) => {
                report.push_str(&format!("{stem:32} MISMATCH\n"));
                failures.push(format!("{stem}: {problem}"));
            }
        }
    }
    println!("{report}");
    assert!(failures.is_empty(), "{}", failures.join("\n\n"));
    let names: Vec<&str> = EXCLUDED.iter().map(|(n, _)| *n).collect();
    assert_eq!(
        excluded, names,
        "every exclusion must name an existing script"
    );
    assert!(
        scripts_compared >= MIN_COMPARED,
        "only {scripts_compared} scripts had compared blocks, need {MIN_COMPARED}"
    );
}
