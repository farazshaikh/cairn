//! Invocation, exit codes and mode selection of the `cairn` binary
//! (milestone M5 AC1, AC2).
//!
//! Where each milestone criterion is tested:
//!
//! | Criterion | Tests |
//! |-----------|-------|
//! | AC1 invocation, exit codes | `cli.rs`; `lock.rs` (locked file) |
//! | AC2 mode selection | `args.rs` unit test `mode_is_interactive_for_a_terminal_or_the_flag`; `cli.rs`, `script.rs` (no prompts) |
//! | AC3 script mode | `script.rs` (golden equivalence over `tests/sql`) |
//! | AC4 interactive mode | `interactive.rs`; `splitter.rs` unit tests |
//! | AC5 meta-commands | `meta.rs`; cairn-exec `tests/schema.rs` |
//! | AC6 ending a session | `interactive.rs`, `meta.rs`; `session.rs` unit test (failed close) |
//! | AC7 OS lock | `lock.rs`; cairn-storage `tests/lock.rs` |
//! | AC8 tests | all of the above |
//! | AC9 README | `readme_shell.rs` |

mod common;

use cairn_exec::Database;
use common::{TempDb, arg, cairn, code, stderr, stdout};

const USAGE: &str = "usage: cairn [--create] [--interactive] PATH\n";

fn assert_usage_error(args: &[&str], message: &str) {
    let args: Vec<_> = args.iter().map(|a| arg(a)).collect();
    let output = cairn(&args, "");
    assert_eq!(code(&output), 2, "args {args:?}");
    assert_eq!(stdout(&output), "", "args {args:?}");
    assert_eq!(stderr(&output), format!("error: {message}\n{USAGE}"));
}

#[test]
fn usage_errors_exit_2() {
    assert_usage_error(&[], "missing database path");
    assert_usage_error(&["a.db", "b.db"], "more than one database path");
    assert_usage_error(&["--bogus", "a.db"], "unknown option --bogus");
    assert_usage_error(
        &["--interactive", "--interactive", "a.db"],
        "option --interactive given twice",
    );
}

#[test]
fn help_prints_usage_to_stdout_and_exits_0() {
    let output = cairn(&[arg("--help")], "");
    assert_eq!(code(&output), 0);
    assert!(stdout(&output).starts_with(USAGE), "{}", stdout(&output));
    assert!(stdout(&output).contains("--create"));
    assert_eq!(stderr(&output), "");
}

#[test]
fn a_missing_file_without_create_is_an_error_and_creates_nothing() {
    let db = TempDb::new("missing");
    let output = cairn(&[db.arg()], "");
    assert_eq!(code(&output), 1);
    assert_eq!(stdout(&output), "");
    assert!(
        stderr(&output).starts_with(&format!("error: cannot open {}: ", db.path().display())),
        "{}",
        stderr(&output)
    );
    assert!(!db.path().exists());
    assert!(!db.wal().exists());
}

#[test]
fn create_makes_a_new_database() {
    let db = TempDb::new("create");
    let output = cairn(&[arg("--create"), db.arg()], "");
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    assert_eq!(stderr(&output), "");
    Database::open(db.path())
        .expect("reopen")
        .close()
        .expect("close");
}

#[test]
fn an_existing_database_is_opened_with_or_without_create() {
    let db = TempDb::new("existing");
    let mut setup = Database::create(db.path()).expect("create");
    setup
        .execute("CREATE TABLE kept (a INTEGER); INSERT INTO kept VALUES (7);")
        .expect("setup");
    setup.close().expect("close");
    for args in [vec![db.arg()], vec![arg("--create"), db.arg()]] {
        let output = cairn(&args, "SELECT a FROM kept;");
        assert_eq!(code(&output), 0, "{}", stderr(&output));
        assert_eq!(stdout(&output), "a\n7\n");
    }
}

#[test]
fn a_file_that_is_not_a_database_is_refused_and_left_unchanged() {
    let db = TempDb::new("foreign");
    let bytes: Vec<u8> = (0..10_000u32).map(|i| (i * 31 % 251) as u8).collect();
    std::fs::write(db.path(), &bytes).expect("write foreign file");
    for args in [vec![db.arg()], vec![arg("--create"), db.arg()]] {
        let output = cairn(&args, "CREATE TABLE t (a INTEGER);");
        assert_eq!(code(&output), 1);
        assert_eq!(stdout(&output), "");
        assert!(
            stderr(&output).starts_with(&format!("error: cannot open {}: ", db.path().display())),
            "{}",
            stderr(&output)
        );
        assert_eq!(std::fs::read(db.path()).expect("read back"), bytes);
        assert!(!db.wal().exists());
    }
}

#[test]
fn interactive_flag_prints_prompts_even_with_piped_input() {
    let db = TempDb::new("mode");
    let piped = cairn(&[arg("--create"), db.arg()], "SELECT 1;");
    assert_eq!(code(&piped), 0);
    assert_eq!(stdout(&piped), "1\n1\n");
    let prompted = cairn(&[db.arg(), arg("--interactive")], "SELECT 1;");
    assert_eq!(code(&prompted), 0);
    assert_eq!(stdout(&prompted), "cairn> 1\n1\ncairn> ");
}
