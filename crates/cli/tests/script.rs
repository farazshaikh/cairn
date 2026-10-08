//! Script mode (milestone M5 AC2, AC3): piped input runs as one batch and
//! prints exactly what the golden tests expect.

mod common;

use std::fs;

use cairn_exec::Database;
use common::{TempDb, arg, cairn, code, sql_dir, stderr, stdout};

fn first_difference(expected: &str, actual: &str) -> String {
    for (n, (want, got)) in expected.lines().zip(actual.lines()).enumerate() {
        if want != got {
            return format!("line {}: expected {want:?}, got {got:?}", n + 1);
        }
    }
    format!(
        "line counts differ: expected {}, got {}",
        expected.lines().count(),
        actual.lines().count()
    )
}

#[test]
fn every_golden_script_reproduces_its_expected_output() {
    let mut scripts: Vec<_> = fs::read_dir(sql_dir())
        .expect("read tests/sql")
        .map(|entry| entry.expect("entry").path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "sql"))
        .collect();
    scripts.sort();
    assert!(scripts.len() >= 51, "scripts found: {}", scripts.len());
    let mut failures = Vec::new();
    for script in &scripts {
        let stem = script.file_stem().expect("stem").to_string_lossy();
        let sql = fs::read_to_string(script).expect("read script");
        let expected =
            fs::read_to_string(script.with_extension("expected")).expect("read expected");
        let db = TempDb::new(&format!("golden-{stem}"));
        let output = cairn(&[arg("--create"), db.arg()], &sql);
        let actual = stdout(&output);
        if actual != expected {
            failures.push(format!("{stem}: {}", first_difference(&expected, &actual)));
        }
        let want_code = i32::from(expected.lines().any(|line| line.starts_with("error: ")));
        if code(&output) != want_code {
            failures.push(format!(
                "{stem}: exit code {} instead of {want_code}",
                code(&output)
            ));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn a_clean_script_prints_no_prompts_and_nothing_on_stderr() {
    let db = TempDb::new("clean");
    let output = cairn(
        &[arg("--create"), db.arg()],
        "CREATE TABLE t (a INTEGER);\nINSERT INTO t VALUES (1), (2);\nSELECT a FROM t;\n",
    );
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "affected 0\n\naffected 2\n\na\n1\n2\n");
    assert_eq!(stderr(&output), "");
}

#[test]
fn a_syntax_error_anywhere_runs_nothing() {
    let db = TempDb::new("syntax");
    let output = cairn(
        &[arg("--create"), db.arg()],
        "CREATE TABLE t (a INTEGER);\nSELEC 1;\n",
    );
    assert_eq!(code(&output), 1);
    assert!(
        stdout(&output).starts_with("error: line 2, column 1: "),
        "{}",
        stdout(&output)
    );
    let mut reopened = Database::open(db.path()).expect("reopen");
    assert!(reopened.schema().expect("schema").is_empty());
    reopened.close().expect("close");
}

#[test]
fn changes_persist_and_the_log_is_checkpointed_on_exit() {
    let db = TempDb::new("persist");
    let output = cairn(
        &[arg("--create"), db.arg()],
        "CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (5);",
    );
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let wal_len = fs::metadata(db.wal()).map(|m| m.len()).unwrap_or(0);
    assert_eq!(wal_len, 0, "clean close leaves an empty log");
    let mut reopened = Database::open(db.path()).expect("reopen");
    let results = reopened.execute("SELECT a FROM t;").expect("select");
    assert_eq!(
        cairn_exec::render::render_outcome(&Ok(results.into_iter().map(Ok).collect())),
        "a\n5\n"
    );
    reopened.close().expect("close");
}

#[test]
fn a_failed_statement_sets_exit_code_1_but_later_ones_run() {
    let db = TempDb::new("failed");
    let output = cairn(
        &[arg("--create"), db.arg()],
        "SELECT * FROM missing;\nCREATE TABLE t (a INTEGER);\n",
    );
    assert_eq!(code(&output), 1);
    let out = stdout(&output);
    assert!(out.starts_with("error: line 1, column 15: "), "{out}");
    assert!(out.ends_with("\n\naffected 0\n"), "{out}");
}

#[test]
fn invalid_utf8_on_stdin_runs_nothing() {
    use std::io::Write;
    use std::process::{Command, Stdio};
    let db = TempDb::new("utf8");
    let mut child = Command::new(common::CAIRN)
        .args([arg("--create"), db.arg()])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn");
    let mut input = child.stdin.take().expect("stdin");
    input
        .write_all(b"CREATE TABLE t (a INTEGER); SELECT '\xff';")
        .expect("write");
    drop(input);
    let output = child.wait_with_output().expect("wait");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    assert!(stderr(&output).starts_with("error: cannot read stdin: "));
    let mut reopened = Database::open(db.path()).expect("reopen");
    assert!(reopened.schema().expect("schema").is_empty());
    reopened.close().expect("close");
}
