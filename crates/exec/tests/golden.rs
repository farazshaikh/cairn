//! Golden SQL tests: every `tests/sql/NNN_name.sql` script at the
//! repository root runs against a fresh database, and its rendered output
//! must equal `tests/sql/NNN_name.expected` byte for byte.
//!
//! Output format: one block per statement, blocks separated by a blank
//! line, and a final newline.
//!
//! - Rows: a header line of column names, then one line per row; values are
//!   tab-separated. NULL prints as `NULL`, booleans as `TRUE`/`FALSE`,
//!   integers in decimal, reals in the cairn-sql canonical form (`1.0`,
//!   `0.1`, `1e300`), and text raw with `\` as `\\`, tab as `\t`, newline
//!   as `\n` and carriage return as `\r`.
//! - Affected counts: `affected N`.
//! - Errors: `error: line L, column C: message`, or `error: message` when
//!   the error has no span. A syntax error stops the script before any
//!   statement runs and is the only block.
//!
//! `CAIRN_BLESS=1 cargo test -p cairn-exec --test golden` rewrites the
//! expected files from the actual output. Blessed files must be reviewed
//! against the semantics in the README before they are committed.
//!
//! Coverage (milestone criterion → scripts; Rust tests in parentheses):
//!
//! | Criterion | Scripts |
//! |-----------|---------|
//! | AC1 API, results, spans | 001, 002, 006, 029, 049, 050 (`database.rs`) |
//! | AC2 catalog, keys, records | 001, 008, 012 (`database.rs`: reopen, moved roots; unit tests in `codec`) |
//! | AC3 insert checks, atomicity | 004-011 |
//! | AC4 indexes | 013-015, 045 (`indexes.rs`) |
//! | AC5 query semantics | 016-038 |
//! | AC6 planner, EXPLAIN | 013, 039-042, 045, 048 (`planner.rs`) |
//! | AC7 UPDATE, DELETE, DROP | 003, 043-047 (`database.rs`: page reuse) |

use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

use cairn_exec::Database;

mod common;

use common::render;

const MIN_SCRIPTS: usize = 40;

/// Removes the database file when dropped, even if the test fails.
struct TempFile(PathBuf);

impl Drop for TempFile {
    fn drop(&mut self) {
        let _ = fs::remove_file(&self.0);
    }
}

fn run_script(path: &Path, stem: &str) -> String {
    let db_path =
        std::env::temp_dir().join(format!("cairn-golden-{}-{stem}.db", std::process::id()));
    let _ = fs::remove_file(&db_path);
    let _guard = TempFile(db_path.clone());
    let sql = fs::read_to_string(path).unwrap_or_else(|e| panic!("read {}: {e}", path.display()));
    let mut db = Database::create(&db_path).unwrap_or_else(|e| panic!("create database: {e}"));
    let output = render(&db.execute_each(&sql));
    db.close().unwrap_or_else(|e| panic!("close database: {e}"));
    output
}

fn first_difference(expected: &str, actual: &str) -> String {
    let expected_lines: Vec<&str> = expected.lines().collect();
    let actual_lines: Vec<&str> = actual.lines().collect();
    let len = expected_lines.len().max(actual_lines.len());
    let mut out = String::new();
    let mut shown = 0;
    for i in 0..len {
        let e = expected_lines.get(i).copied();
        let a = actual_lines.get(i).copied();
        if e != a && shown < 5 {
            shown += 1;
            let _ = writeln!(
                out,
                "  line {}:\n    expected: {e:?}\n    actual:   {a:?}",
                i + 1
            );
        }
    }
    if shown == 0 {
        out.push_str("  (differs only in trailing newlines)\n");
    }
    out
}

#[test]
fn golden_scripts() {
    let dir = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/sql");
    let mut scripts = Vec::new();
    let mut expected = Vec::new();
    for entry in fs::read_dir(&dir).unwrap_or_else(|e| panic!("read {}: {e}", dir.display())) {
        let path = entry.expect("directory entry").path();
        match path.extension().and_then(|e| e.to_str()) {
            Some("sql") => scripts.push(path),
            Some("expected") => expected.push(path),
            _ => panic!("unexpected file in tests/sql: {}", path.display()),
        }
    }
    scripts.sort();
    expected.sort();
    for path in &expected {
        assert!(
            path.with_extension("sql").exists(),
            "{} has no script",
            path.display()
        );
    }
    assert!(
        scripts.len() >= MIN_SCRIPTS,
        "expected at least {MIN_SCRIPTS} scripts, found {}",
        scripts.len()
    );
    let bless = std::env::var("CAIRN_BLESS").is_ok_and(|v| v == "1");
    let mut failures = Vec::new();
    for script in &scripts {
        let stem = script
            .file_stem()
            .and_then(|s| s.to_str())
            .unwrap_or("script");
        let actual = run_script(script, stem);
        let expected_path = script.with_extension("expected");
        if bless {
            fs::write(&expected_path, &actual).expect("write expected file");
            continue;
        }
        let Ok(expected) = fs::read_to_string(&expected_path) else {
            failures.push(format!("{stem}: missing {}", expected_path.display()));
            continue;
        };
        if expected != actual {
            failures.push(format!("{stem}:\n{}", first_difference(&expected, &actual)));
        }
    }
    assert!(
        failures.is_empty(),
        "{} of {} scripts differ:\n{}",
        failures.len(),
        scripts.len(),
        failures.join("\n")
    );
}
