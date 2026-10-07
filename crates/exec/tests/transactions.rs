//! SQL transactions (milestone M4 AC2, AC3): BEGIN, COMMIT and ROLLBACK,
//! autocommit, statement-level rollback, the error rules, two handles on one
//! file, and the unusable state after a failed log sync.

use std::path::Path;
use std::sync::Arc;

use cairn_exec::{Database, ErrorKind, ExecError, QueryResult, Value};
use cairn_storage::fault::{CrashMode, Fault, FaultVfs};
use cairn_storage::{Options, Pager};

const DB: &str = "/tx.db";

fn options() -> Options {
    Options::new(64, 1000)
}

fn create(vfs: &FaultVfs) -> Database {
    Database::create_with(Arc::new(vfs.clone()), Path::new(DB), options()).expect("create")
}

fn open(vfs: &FaultVfs) -> Database {
    Database::open_with(Arc::new(vfs.clone()), Path::new(DB), options()).expect("open")
}

fn rows(db: &mut Database, sql: &str) -> Vec<Vec<Value>> {
    match db.execute(sql).expect(sql).pop() {
        Some(QueryResult::Rows { rows, .. }) => rows,
        other => panic!("{sql}: {other:?}"),
    }
}

fn error(db: &mut Database, sql: &str) -> ExecError {
    db.execute(sql).expect_err(sql)
}

fn ints(values: &[i64]) -> Vec<Vec<Value>> {
    values.iter().map(|&v| vec![Value::Integer(v)]).collect()
}

/// Page count and free-list length as committed, through a second handle.
fn pages(vfs: &FaultVfs) -> (u32, usize) {
    let mut pager =
        Pager::open_with(Arc::new(vfs.clone()), Path::new(DB), options()).expect("pager");
    let free = pager.free_list().expect("free list").len();
    (pager.page_count(), free)
}

#[test]
fn autocommit_statements_are_durable_and_failures_leave_no_trace() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT UNIQUE)")
        .expect("create");
    db.execute("INSERT INTO t VALUES (1, 'a'), (2, 'b')")
        .expect("insert");
    assert_eq!(
        error(&mut db, "INSERT INTO t VALUES (3, 'a')").kind(),
        ErrorKind::Constraint
    );
    let image = vfs.crash(CrashMode::SyncedOnly);
    let mut reopened = open(&image);
    assert_eq!(
        rows(&mut reopened, "SELECT id FROM t ORDER BY id"),
        ints(&[1, 2])
    );
    reopened.check().expect("check");
}

#[test]
fn rollback_discards_ddl_and_dml_including_the_catalog() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    db.execute("CREATE TABLE keep (a INTEGER); INSERT INTO keep VALUES (1)")
        .expect("setup");
    let before = pages(&vfs);
    db.execute(
        "BEGIN;
         CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT);
         CREATE INDEX t_v ON t (v);
         INSERT INTO t VALUES (1, 'x'), (2, 'y');
         INSERT INTO keep VALUES (2);
         DROP TABLE keep;",
    )
    .expect("transaction body");
    assert!(db.in_transaction());
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM t"), ints(&[2]));
    db.execute("ROLLBACK").expect("rollback");
    assert!(!db.in_transaction());
    assert_eq!(
        error(&mut db, "SELECT * FROM t").kind(),
        ErrorKind::NotFound
    );
    assert_eq!(rows(&mut db, "SELECT a FROM keep"), ints(&[1]));
    assert_eq!(
        pages(&vfs),
        before,
        "page count and free list are unchanged"
    );
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, v TEXT); CREATE INDEX t_v ON t (v)")
        .expect("the names are free again");
    db.check().expect("check");
}

#[test]
fn a_failing_statement_inside_begin_rolls_back_only_itself() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL)")
        .expect("create");
    db.execute("BEGIN; INSERT INTO t VALUES (1, 'a', 10)")
        .expect("begin");
    let failing = [
        (
            "INSERT INTO t VALUES (2, 'b', 20), (3, 'a', 30)",
            ErrorKind::Constraint,
        ),
        ("INSERT INTO t VALUES (2, 'b', NULL)", ErrorKind::Constraint),
        ("INSERT INTO t VALUES (2, 'b', 'text')", ErrorKind::Type),
        ("UPDATE t SET n = n / 0", ErrorKind::Arithmetic),
        ("CREATE TABLE t (x INTEGER)", ErrorKind::AlreadyExists),
        ("CREATE UNIQUE INDEX t_n ON t (nope)", ErrorKind::NotFound),
    ];
    for (sql, kind) in failing {
        assert_eq!(error(&mut db, sql).kind(), kind, "{sql}");
        assert!(db.in_transaction(), "{sql} ended the transaction");
    }
    db.execute("CREATE TABLE side (x INTEGER); INSERT INTO t VALUES (2, 'b', 20)")
        .expect("later statements still work");
    db.execute("COMMIT").expect("commit");
    assert_eq!(rows(&mut db, "SELECT id FROM t ORDER BY id"), ints(&[1, 2]));
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM side"), ints(&[0]));
    db.check().expect("check");
}

#[test]
fn nested_begin_and_commit_or_rollback_without_begin_are_errors() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    for (sql, message) in [
        ("COMMIT", "cannot COMMIT: no transaction is active"),
        ("ROLLBACK", "cannot ROLLBACK: no transaction is active"),
    ] {
        let e = error(&mut db, sql);
        assert_eq!((e.kind(), e.message()), (ErrorKind::Transaction, message));
        assert_eq!(e.span().map(|s| (s.line, s.column)), Some((1, 1)));
    }
    db.execute("CREATE TABLE t (a INTEGER); BEGIN; INSERT INTO t VALUES (1)")
        .expect("begin");
    let e = error(&mut db, "SELECT 1;\n  BEGIN");
    assert_eq!(e.kind(), ErrorKind::Transaction);
    assert_eq!(e.message(), "cannot BEGIN: a transaction is already active");
    assert_eq!(e.span().map(|s| (s.line, s.column)), Some((2, 3)));
    assert!(db.in_transaction(), "the outer transaction is unaffected");
    db.execute("COMMIT").expect("commit");
    assert_eq!(rows(&mut db, "SELECT a FROM t"), ints(&[1]));
}

#[test]
fn other_handles_see_only_committed_data_and_get_busy() {
    let vfs = FaultVfs::new();
    let mut a = create(&vfs);
    a.execute("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1)")
        .expect("setup");
    let mut b = open(&vfs);
    a.execute("BEGIN; INSERT INTO t VALUES (2); CREATE TABLE u (x INTEGER)")
        .expect("a writes");
    assert_eq!(
        rows(&mut b, "SELECT a FROM t"),
        ints(&[1]),
        "uncommitted rows are invisible"
    );
    assert_eq!(error(&mut b, "SELECT * FROM u").kind(), ErrorKind::NotFound);
    for sql in [
        "BEGIN",
        "INSERT INTO t VALUES (3)",
        "CREATE TABLE v (x INTEGER)",
    ] {
        let e = error(&mut b, sql);
        assert_eq!(e.kind(), ErrorKind::Busy, "{sql}: {e}");
    }
    assert!(!b.in_transaction());
    a.execute("COMMIT").expect("commit");
    assert_eq!(rows(&mut b, "SELECT a FROM t ORDER BY a"), ints(&[1, 2]));
    assert_eq!(
        rows(&mut b, "SELECT COUNT(*) FROM u"),
        ints(&[0]),
        "committed DDL is visible"
    );
    b.execute("BEGIN; INSERT INTO u VALUES (9); COMMIT")
        .expect("b writes now");
    assert_eq!(rows(&mut a, "SELECT x FROM u"), ints(&[9]));
    b.check().expect("check");
}

#[test]
fn close_and_drop_discard_an_open_transaction() {
    let vfs = FaultVfs::new();
    let mut a = create(&vfs);
    a.execute("CREATE TABLE t (a INTEGER)").expect("create");
    a.execute("BEGIN; INSERT INTO t VALUES (1)").expect("begin");
    a.close().expect("close");
    let mut b = open(&vfs);
    b.execute("BEGIN; INSERT INTO t VALUES (2)").expect("begin");
    drop(b);
    let mut c = open(&vfs);
    assert_eq!(rows(&mut c, "SELECT COUNT(*) FROM t"), ints(&[0]));
    c.execute("INSERT INTO t VALUES (3)")
        .expect("the write lock was released");
}

#[test]
fn execute_stops_at_an_error_inside_begin_and_keeps_the_transaction() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    db.execute("CREATE TABLE t (a INTEGER NOT NULL)")
        .expect("create");
    let e = error(
        &mut db,
        "BEGIN; INSERT INTO t VALUES (1); INSERT INTO t VALUES (NULL); INSERT INTO t VALUES (3)",
    );
    assert_eq!(e.kind(), ErrorKind::Constraint);
    assert!(db.in_transaction());
    assert_eq!(rows(&mut db, "SELECT a FROM t"), ints(&[1]));
    db.execute("ROLLBACK").expect("rollback");
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM t"), ints(&[0]));
}

#[test]
fn a_failed_log_sync_makes_the_file_unusable_until_reopened() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    db.execute("CREATE TABLE t (a INTEGER); PRAGMA checkpoint")
        .expect("create");
    // Count the writes of the INSERT's commit on an identical copy, then stop
    // the real file layer right after the last one (the commit record), so
    // the log sync that follows fails.
    let probe_vfs = vfs.crash(CrashMode::KeepWrites);
    let mut probe = open(&probe_vfs);
    let start = probe_vfs.writes();
    probe.execute("INSERT INTO t VALUES (1)").expect("probe");
    let commit_writes = probe_vfs.writes() - start;
    vfs.arm(Fault::StopAfterWrite(vfs.writes() + commit_writes));
    let e = error(&mut db, "INSERT INTO t VALUES (1)");
    assert_eq!(e.kind(), ErrorKind::Storage, "{e}");
    assert_eq!(
        error(&mut db, "SELECT * FROM t").kind(),
        ErrorKind::Unusable
    );
    assert_eq!(error(&mut db, "BEGIN").kind(), ErrorKind::Unusable);
    let image = vfs.crash(CrashMode::SyncedOnly);
    drop(db);
    let mut reopened = open(&image);
    assert_eq!(rows(&mut reopened, "SELECT COUNT(*) FROM t"), ints(&[0]));
    reopened.check().expect("check");
}
