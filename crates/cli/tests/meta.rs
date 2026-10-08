//! Meta-commands (milestone M5 AC5, AC6): `.help`, `.quit`, `.exit`,
//! `.tables` and `.schema` in interactive mode.

mod common;

use cairn_exec::Database;
use common::{TempDb, arg, cairn, code, stderr, stdout};

const SCHEMA: &str = "\
CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);
CREATE TABLE books (title TEXT NOT NULL, author_id INTEGER, year INTEGER, rating REAL);
CREATE INDEX books_author ON books (author_id);
CREATE UNIQUE INDEX books_title ON books (title);
CREATE TABLE \"Odd Name\" (\"select\" BOOLEAN, \"Mixed\" TEXT);
";

fn interactive(db: &TempDb, input: &str) -> std::process::Output {
    cairn(&[arg("--create"), arg("--interactive"), db.arg()], input)
}

/// The output of one command line: the transcript without its first
/// prompt and the final prompt after end of input.
fn command_output(db: &TempDb, command: &str) -> String {
    let output = interactive(db, command);
    assert_eq!(code(&output), 0, "{}", stderr(&output));
    let out = stdout(&output);
    out.strip_prefix("cairn> ")
        .and_then(|rest| rest.strip_suffix("cairn> "))
        .unwrap_or_else(|| panic!("unexpected transcript {out:?}"))
        .to_string()
}

fn with_schema(tag: &str) -> TempDb {
    let db = TempDb::new(tag);
    let mut setup = Database::create(db.path()).expect("create");
    setup.execute(SCHEMA).expect("schema");
    setup.close().expect("close");
    db
}

#[test]
fn help_lists_every_command() {
    let db = TempDb::new("help");
    let out = command_output(&db, ".help\n");
    for command in [".help", ".quit", ".exit", ".tables", ".schema [TABLE]"] {
        assert!(out.contains(command), "{command} missing from {out}");
    }
}

#[test]
fn tables_lists_names_in_ascending_order() {
    let empty = TempDb::new("tables-empty");
    assert_eq!(command_output(&empty, ".tables\n"), "");
    let db = with_schema("tables");
    assert_eq!(
        command_output(&db, ".tables\n"),
        "\"Odd Name\"\nauthors\nbooks\n"
    );
}

#[test]
fn schema_prints_create_statements_for_all_or_one_table() {
    let db = with_schema("schema");
    assert_eq!(
        command_output(&db, ".schema\n"),
        "CREATE TABLE \"Odd Name\" (\"select\" BOOLEAN, \"Mixed\" TEXT);\n\
         CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);\n\
         CREATE TABLE books (title TEXT NOT NULL, author_id INTEGER, year INTEGER, rating REAL);\n\
         CREATE INDEX books_author ON books (author_id);\n\
         CREATE UNIQUE INDEX books_title ON books (title);\n"
    );
    assert_eq!(
        command_output(&db, ".schema AUTHORS\n"),
        "CREATE TABLE authors (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE);\n"
    );
    assert_eq!(
        command_output(&db, ".schema \"Odd Name\"\n"),
        "CREATE TABLE \"Odd Name\" (\"select\" BOOLEAN, \"Mixed\" TEXT);\n"
    );
    assert_eq!(
        command_output(&db, ".schema missing\n"),
        "error: no such table: missing\n"
    );
    assert_eq!(
        command_output(&db, ".schema a b\n"),
        "error: usage: .schema [TABLE]\n"
    );
}

#[test]
fn schema_output_replays_to_the_same_schema() {
    let db = with_schema("replay-source");
    let sql = command_output(&db, ".schema\n");
    let copy = TempDb::new("replay-copy");
    let replayed = cairn(&[arg("--create"), copy.arg()], &sql);
    assert_eq!(code(&replayed), 0, "{}", stdout(&replayed));
    assert_eq!(command_output(&copy, ".schema\n"), sql);
    let mut source = Database::open(db.path()).expect("open source");
    let mut target = Database::open(copy.path()).expect("open copy");
    assert_eq!(
        source.schema().expect("source"),
        target.schema().expect("copy")
    );
    source.close().expect("close");
    target.close().expect("close");
}

#[test]
fn unknown_commands_and_bad_arguments_keep_the_session_going() {
    let db = TempDb::new("unknown");
    let output = interactive(&db, ".foo\n.tables extra\nSELECT 1;\n");
    assert_eq!(code(&output), 0);
    assert_eq!(
        stdout(&output),
        "cairn> error: unknown command .foo (enter .help for a list of commands)\n\
         cairn> error: usage: .tables\n\
         cairn> 1\n1\n\
         cairn> "
    );
}

#[test]
fn quit_and_exit_end_the_session_like_end_of_input() {
    for command in [".quit", ".exit"] {
        let db = TempDb::new("quit");
        let output = interactive(
            &db,
            &format!(
                "CREATE TABLE t (a INTEGER);\nBEGIN;\nINSERT INTO t VALUES (1);\n{command}\nCREATE TABLE never (a INTEGER);\n"
            ),
        );
        assert_eq!(code(&output), 0, "{command}");
        assert_eq!(
            stderr(&output),
            "warning: open transaction rolled back\n",
            "{command}"
        );
        assert!(
            stdout(&output).ends_with("affected 1\ncairn> "),
            "{command}"
        );
        assert_eq!(
            std::fs::metadata(db.wal()).map(|m| m.len()).unwrap_or(0),
            0,
            "{command} checkpoints"
        );
        let mut reopened = Database::open(db.path()).expect("reopen");
        let names: Vec<String> = reopened
            .schema()
            .expect("schema")
            .into_iter()
            .map(|t| t.name)
            .collect();
        assert_eq!(names, ["t"], "{command}");
        assert_eq!(
            reopened.execute("SELECT a FROM t;").expect("select")[0],
            cairn_exec::QueryResult::Rows {
                columns: vec!["a".to_string()],
                rows: vec![]
            }
        );
        reopened.close().expect("close");
    }
}

#[test]
fn a_dot_line_inside_a_pending_statement_is_sql() {
    let db = with_schema("pending-dot");
    let output = interactive(&db, "SELECT 1\n.5;\n");
    assert_eq!(code(&output), 0);
    assert!(
        stdout(&output).starts_with("cairn>    ...> error: line 2, column 1: "),
        "{}",
        stdout(&output)
    );
}

#[test]
fn script_mode_treats_a_dot_line_as_sql() {
    let db = with_schema("script-dot");
    let output = cairn(&[db.arg()], ".tables\n");
    assert_eq!(code(&output), 1);
    assert!(
        stdout(&output).starts_with("error: line 1, column 1: "),
        "{}",
        stdout(&output)
    );
}
