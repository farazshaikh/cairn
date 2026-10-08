//! Interactive mode and ending a session (milestone M5 AC4, AC6), driven
//! through `--interactive` with piped input.

mod common;

use cairn_exec::Database;
use common::{TempDb, arg, cairn, code, stderr, stdout};

fn interactive(db: &TempDb, input: &str) -> std::process::Output {
    cairn(&[arg("--create"), arg("--interactive"), db.arg()], input)
}

fn rows(db: &TempDb, sql: &str) -> String {
    let mut handle = Database::open(db.path()).expect("reopen");
    let outcome = handle.execute_each(sql);
    handle.close().expect("close");
    cairn_exec::render::render_outcome(&outcome)
}

#[test]
fn prompts_continuations_and_blocks_follow_the_input() {
    let db = TempDb::new("transcript");
    let output = interactive(
        &db,
        "CREATE TABLE t (a INTEGER);\nINSERT INTO t\nVALUES (1);\nSELECT a FROM t;\n",
    );
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(
        stdout(&output),
        "cairn> affected 0\ncairn>    ...> affected 1\ncairn> a\n1\ncairn> "
    );
    assert_eq!(stderr(&output), "");
}

#[test]
fn several_statements_on_one_line_run_in_order() {
    let db = TempDb::new("one-line");
    let output = interactive(
        &db,
        "CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (3);\n",
    );
    assert_eq!(stdout(&output), "cairn> affected 0\naffected 1\ncairn> ");
}

#[test]
fn errors_are_relative_to_their_statement_and_do_not_end_the_session() {
    let db = TempDb::new("errors");
    let output = interactive(
        &db,
        "SELECT 1; SELEC 2;\nSELECT\n  nope FROM t;\nCREATE TABLE t (a INTEGER);\n",
    );
    assert_eq!(
        code(&output),
        0,
        "statement errors do not change the exit code"
    );
    let out = stdout(&output);
    let lines: Vec<&str> = out.lines().collect();
    assert_eq!(lines[0], "cairn> 1");
    assert_eq!(lines[1], "1");
    assert!(lines[2].starts_with("error: line 1, column 1: "), "{out}");
    assert!(
        lines[3].starts_with("cairn>    ...> error: line 2, column 13: no such table"),
        "{out}"
    );
    assert_eq!(lines[4], "cairn> affected 0");
    assert!(rows(&db, "SELECT a FROM t;").starts_with("a\n"));
}

#[test]
fn blank_lines_and_comments_at_the_prompt_do_nothing() {
    let db = TempDb::new("blank");
    let output = interactive(&db, "\n   \n-- note\n");
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "cairn> cairn> cairn> cairn> ");
    assert_eq!(stderr(&output), "");
}

#[test]
fn end_of_input_closes_and_checkpoints() {
    let db = TempDb::new("eof");
    let output = interactive(
        &db,
        "CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (9);\n",
    );
    assert_eq!(code(&output), 0);
    assert_eq!(std::fs::metadata(db.wal()).map(|m| m.len()).unwrap_or(0), 0);
    assert_eq!(rows(&db, "SELECT a FROM t;"), "a\n9\n");
}

#[test]
fn an_open_transaction_is_rolled_back_with_a_warning() {
    let db = TempDb::new("txn");
    let output = interactive(
        &db,
        "CREATE TABLE t (a INTEGER);\nBEGIN;\nINSERT INTO t VALUES (1);\n",
    );
    assert_eq!(code(&output), 0);
    assert_eq!(stderr(&output), "warning: open transaction rolled back\n");
    assert_eq!(rows(&db, "SELECT a FROM t;"), "a\n");
}

#[test]
fn an_incomplete_statement_is_discarded_with_a_warning() {
    let db = TempDb::new("incomplete");
    let output = interactive(&db, "CREATE TABLE t (a INTEGER)\n");
    assert_eq!(code(&output), 0);
    assert_eq!(stdout(&output), "cairn>    ...> ");
    assert_eq!(stderr(&output), "warning: incomplete statement discarded\n");
    let mut reopened = Database::open(db.path()).expect("reopen");
    assert!(reopened.schema().expect("schema").is_empty());
    reopened.close().expect("close");
}

#[test]
fn an_unterminated_string_waits_for_more_lines() {
    let db = TempDb::new("string");
    let output = interactive(&db, "SELECT 'a;\nb';\n");
    assert_eq!(stdout(&output), "cairn>    ...> 'a;\\nb'\na;\\nb\ncairn> ");
}
