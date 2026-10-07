//! Database-level behaviour that one golden script cannot show: persistence
//! across close and reopen, B-tree roots that move, page reuse after DROP
//! TABLE, statement atomicity compared on full table dumps, and errors.

mod common;

use common::remove_database;

use std::fs;
use std::path::PathBuf;

use cairn_exec::{Database, ErrorKind, ExecError, QueryResult, Value};
use cairn_storage::Pager;

struct TempDb(PathBuf);

impl TempDb {
    fn new(name: &str) -> TempDb {
        let path =
            std::env::temp_dir().join(format!("cairn-exec-{}-{name}.db", std::process::id()));
        remove_database(&path);
        TempDb(path)
    }

    fn create(&self) -> Database {
        Database::create(&self.0).expect("create")
    }

    fn open(&self) -> Database {
        Database::open(&self.0).expect("open")
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        remove_database(&self.0);
    }
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

fn int(v: i64) -> Value {
    Value::Integer(v)
}

#[test]
fn catalog_and_rows_survive_reopen_after_roots_move() {
    let tmp = TempDb::new("reopen");
    let mut db = tmp.create();
    db.execute(
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, score REAL, ok BOOLEAN);
         CREATE TABLE log (msg TEXT, n INTEGER);
         CREATE INDEX people_score ON people (score);",
    )
    .expect("schema");
    for chunk in 0..20 {
        let values: Vec<String> = (0..100)
            .map(|i| {
                let id = chunk * 100 + i;
                format!(
                    "({id}, 'person number {id:05}', {}.5, {})",
                    id % 37,
                    id % 2 == 0
                )
            })
            .collect();
        db.execute(&format!("INSERT INTO people VALUES {}", values.join(", ")))
            .expect("insert");
        db.execute(&format!("INSERT INTO log VALUES ('chunk', {chunk})"))
            .expect("log");
    }
    db.check().expect("check before close");
    db.close().expect("close");

    let mut db = tmp.open();
    db.check().expect("check after reopen");
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM people"),
        vec![vec![int(2000)]]
    );
    assert_eq!(
        rows(
            &mut db,
            "SELECT name, score, ok FROM people WHERE id = 1234"
        ),
        vec![vec![
            Value::Text("person number 01234".into()),
            Value::Real(13.5),
            Value::Boolean(true)
        ]]
    );
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM people WHERE score = 0.5"),
        vec![vec![int(55)]]
    );
    let plan = rows(
        &mut db,
        "EXPLAIN SELECT id FROM people WHERE name = 'person number 00007'",
    );
    assert_eq!(
        plan.last().and_then(|r| r.get(2)).cloned(),
        Some(Value::Text("INDEX LOOKUP".into()))
    );
    let error = error(
        &mut db,
        "INSERT INTO people VALUES (5000, 'person number 00001', 1.0, TRUE)",
    );
    assert_eq!(error.message(), "UNIQUE constraint failed: people.name");
    let error = db
        .execute("INSERT INTO people (id) VALUES (9999)")
        .expect_err("not null");
    assert_eq!(error.message(), "NOT NULL constraint failed: people.name");
    assert_eq!(
        rows(&mut db, "SELECT SUM(n) FROM log"),
        vec![vec![int(190)]]
    );
}

#[test]
fn hidden_row_ids_are_never_reused_even_after_reopen() {
    let tmp = TempDb::new("rowid");
    let mut db = tmp.create();
    db.execute("CREATE TABLE t (v TEXT); INSERT INTO t VALUES ('a'), ('b'); DELETE FROM t")
        .expect("setup");
    db.close().expect("close");
    let mut db = tmp.open();
    db.execute("INSERT INTO t VALUES ('c')").expect("insert");
    db.check().expect("check");
    let plan = rows(&mut db, "EXPLAIN SELECT v FROM t");
    assert_eq!(plan.len(), 2);
    assert_eq!(
        rows(&mut db, "SELECT v FROM t"),
        vec![vec![Value::Text("c".into())]]
    );
}

fn free_pages(path: &PathBuf) -> (u32, usize) {
    let mut pager = Pager::open(path).expect("pager");
    let free = pager.free_list().expect("free list").len();
    (pager.page_count(), free)
}

#[test]
fn drop_table_frees_every_page_of_the_table_and_its_indexes() {
    let tmp = TempDb::new("drop");
    tmp.create().close().expect("close");
    let (pages_before, free_before) = free_pages(&tmp.0);

    let mut db = tmp.open();
    db.execute("CREATE TABLE big (id INTEGER PRIMARY KEY, tag TEXT UNIQUE, n INTEGER)")
        .expect("create");
    db.execute("CREATE INDEX big_n ON big (n)").expect("index");
    for chunk in 0..30 {
        let values: Vec<String> = (0..100)
            .map(|i| {
                let id = chunk * 100 + i;
                format!("({id}, 'tag-{id:06}-padding-padding-padding', {})", id % 11)
            })
            .collect();
        db.execute(&format!("INSERT INTO big VALUES {}", values.join(", ")))
            .expect("insert");
    }
    db.close().expect("close");
    let (pages_full, free_full) = free_pages(&tmp.0);
    assert_eq!(free_full, free_before);
    let used = pages_full - pages_before;
    assert!(used > 30, "the table should span many pages, used {used}");

    let mut db = tmp.open();
    db.execute("DROP TABLE big").expect("drop");
    db.close().expect("close");
    let (pages_dropped, free_dropped) = free_pages(&tmp.0);
    assert_eq!(
        pages_dropped, pages_full,
        "dropping does not shrink the file"
    );
    assert_eq!(
        free_dropped,
        free_before + used as usize,
        "every page used by the table and its indexes is free"
    );

    let mut db = tmp.open();
    db.execute("CREATE TABLE again (id INTEGER PRIMARY KEY, v TEXT)")
        .expect("create");
    let values: Vec<String> = (0..500).map(|i| format!("({i}, 'value {i}')")).collect();
    db.execute(&format!("INSERT INTO again VALUES {}", values.join(", ")))
        .expect("insert");
    db.check().expect("check");
    db.close().expect("close");
    let (pages_reused, _) = free_pages(&tmp.0);
    assert_eq!(
        pages_reused, pages_full,
        "new pages come from the free list"
    );
}

fn dump(db: &mut Database) -> Vec<Vec<Value>> {
    let mut all = rows(db, "SELECT * FROM t ORDER BY id");
    all.extend(rows(
        db,
        "SELECT id, u FROM t WHERE u IS NOT NULL ORDER BY u",
    ));
    all.extend(rows(db, "SELECT COUNT(*) FROM h"));
    all
}

#[test]
fn failing_statements_change_nothing() {
    let tmp = TempDb::new("atomic");
    let mut db = tmp.create();
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER NOT NULL);
         CREATE TABLE h (v INTEGER);
         CREATE INDEX t_n ON t (n);
         INSERT INTO t VALUES (1, 'a', 10), (2, 'b', 20), (3, NULL, 30);
         INSERT INTO h VALUES (1);",
    )
    .expect("setup");
    let before = dump(&mut db);
    let failing = [
        (
            "INSERT INTO t VALUES (4, 'd', 40), (5, 'e', 50), (6, 'a', 60)",
            ErrorKind::Constraint,
        ),
        (
            "INSERT INTO t VALUES (4, 'd', 40), (4, 'e', 50)",
            ErrorKind::Constraint,
        ),
        (
            "INSERT INTO t VALUES (4, 'd', 40), (5, 'd', 50)",
            ErrorKind::Constraint,
        ),
        (
            "INSERT INTO t VALUES (4, 'd', 40), (5, 'e', NULL)",
            ErrorKind::Constraint,
        ),
        (
            "INSERT INTO t VALUES (4, 'd', 40), (5, 'e', 9223372036854775807 + 1)",
            ErrorKind::Arithmetic,
        ),
        ("INSERT INTO h VALUES (1), (2), ('x')", ErrorKind::Type),
        ("UPDATE t SET u = 'z'", ErrorKind::Constraint),
        ("UPDATE t SET n = n / (id - 3)", ErrorKind::Arithmetic),
        (
            "UPDATE t SET id = id * 2 WHERE id < 2",
            ErrorKind::Constraint,
        ),
        (
            "DELETE FROM t WHERE 10 / (n - 30) > 0",
            ErrorKind::Arithmetic,
        ),
    ];
    for (sql, kind) in failing {
        let error = error(&mut db, sql);
        assert_eq!(error.kind(), kind, "{sql}: {error}");
        assert_eq!(dump(&mut db), before, "{sql} changed the database");
        db.check().expect("check");
    }
    db.execute("INSERT INTO h VALUES (2), (3)")
        .expect("next row ids");
    db.execute("UPDATE t SET id = id + 1").expect("shift keys");
    assert_eq!(
        rows(&mut db, "SELECT id FROM t"),
        vec![vec![int(2)], vec![int(3)], vec![int(4)]]
    );
    db.check().expect("check");
}

#[test]
fn errors_carry_spans_and_execute_stops_at_the_first() {
    let tmp = TempDb::new("errors");
    let mut db = tmp.create();
    let error = db
        .execute("CREATE TABLE t (a INTEGER);\nINSERT INTO t VALUES (1);\nSELECT nope FROM t;\nINSERT INTO t VALUES (2)")
        .expect_err("unknown column");
    assert_eq!(error.kind(), ErrorKind::NotFound);
    let span = error.span().expect("span");
    assert_eq!((span.line, span.column), (3, 8));
    assert_eq!(error.source_line(), Some("SELECT nope FROM t;"));
    assert_eq!(
        error.to_string(),
        "line 3, column 8: no such column: nope\nSELECT nope FROM t;\n       ^"
    );
    assert_eq!(rows(&mut db, "SELECT a FROM t"), vec![vec![int(1)]]);

    let syntax = db.execute("SELECT 1; SELEC 2").expect_err("syntax");
    assert_eq!(syntax.kind(), ErrorKind::Syntax);
    let each = db
        .execute_each("SELECT 1; SELECT 1 / 0; SELECT 2")
        .expect("parses");
    assert_eq!(each.len(), 3);
    assert!(each.get(1).is_some_and(Result::is_err));
    assert!(each.get(2).is_some_and(Result::is_ok));
}

#[test]
fn create_and_open_report_bad_files() {
    let tmp = TempDb::new("files");
    tmp.create().close().expect("close");
    assert!(
        Database::create(&tmp.0).is_err(),
        "create refuses an existing file"
    );

    let missing = TempDb::new("missing");
    assert_eq!(
        Database::open(&missing.0).err().map(|e| e.kind()),
        Some(ErrorKind::Storage)
    );

    let raw = TempDb::new("raw");
    Pager::create(&raw.0)
        .expect("pager")
        .close()
        .expect("close");
    let error = Database::open(&raw.0).err().expect("no catalog");
    assert_eq!(error.kind(), ErrorKind::Corrupt);
    assert_eq!(error.message(), "not a cairn database: no catalog");

    fs::write(&raw.0, b"not a database").expect("write");
    assert_eq!(
        Database::open(&raw.0).err().map(|e| e.kind()),
        Some(ErrorKind::Corrupt)
    );
}

#[test]
fn rows_and_index_keys_over_the_storage_limits_are_rejected() {
    let tmp = TempDb::new("limits");
    let mut db = tmp.create();
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, body TEXT, tag TEXT UNIQUE)")
        .expect("create");
    let long = "x".repeat(1100);
    let row = error(&mut db, &format!("INSERT INTO t VALUES (1, '{long}', 'a')"));
    assert_eq!(row.kind(), ErrorKind::TooLarge, "{row}");
    assert!(
        row.message().starts_with("row too large: "),
        "{}",
        row.message()
    );
    let tag = "y".repeat(300);
    let key = error(&mut db, &format!("INSERT INTO t VALUES (1, 'ok', '{tag}')"));
    assert_eq!(key.kind(), ErrorKind::TooLarge);
    assert!(
        key.message()
            .starts_with("index key too large for index cairn_autoindex_t_tag")
    );
    assert_eq!(rows(&mut db, "SELECT COUNT(*) FROM t"), vec![vec![int(0)]]);
    db.check().expect("check");
}

#[test]
fn maximum_length_names_with_implicit_indexes_survive_reopen() {
    let tmp = TempDb::new("long-names");
    let table = "t".repeat(64);
    let other = format!("{}u", "t".repeat(63));
    let column = "c".repeat(64);
    let mut db = tmp.create();
    db.execute(&format!(
        "CREATE TABLE {table} (id INTEGER PRIMARY KEY, {column} TEXT UNIQUE);
         CREATE TABLE {other} ({column} TEXT UNIQUE, k TEXT PRIMARY KEY);
         INSERT INTO {table} VALUES (1, 'a'), (2, 'b');
         INSERT INTO {other} VALUES ('a', 'x');"
    ))
    .expect("schema with 64-byte names");
    db.close().expect("close");

    let mut db = tmp.open();
    db.check().expect("check after reopen");
    let failed = error(&mut db, &format!("INSERT INTO {table} VALUES (3, 'a')"));
    assert_eq!(
        failed.message(),
        format!("UNIQUE constraint failed: {table}.{column}")
    );
    let failed = error(&mut db, &format!("INSERT INTO {other} VALUES ('a', 'y')"));
    assert_eq!(
        failed.message(),
        format!("UNIQUE constraint failed: {other}.{column}")
    );
    let failed = error(&mut db, &format!("INSERT INTO {other} VALUES ('b', 'x')"));
    assert_eq!(
        failed.message(),
        format!("PRIMARY KEY constraint failed: {other}.k")
    );
    let plan = rows(
        &mut db,
        &format!("EXPLAIN SELECT id FROM {table} WHERE {column} = 'b'"),
    );
    assert_eq!(
        plan.last().and_then(|r| r.get(2)).cloned(),
        Some(Value::Text("INDEX LOOKUP".into()))
    );
    assert_eq!(
        rows(
            &mut db,
            &format!("SELECT id FROM {table} WHERE {column} = 'b'")
        ),
        vec![vec![int(2)]]
    );
    db.execute(&format!("DROP TABLE {table}; DROP TABLE {other}"))
        .expect("drop");
    db.close().expect("close");
    tmp.open().check().expect("check after drop and reopen");
}

#[test]
fn explain_without_a_statement_is_a_syntax_error() {
    let tmp = TempDb::new("lone-explain");
    let mut db = tmp.create();
    db.execute("CREATE TABLE t (a INTEGER)").expect("create");
    for sql in [
        "EXPLAIN",
        "EXPLAIN;",
        "SELECT 1; EXPLAIN",
        "INSERT INTO t VALUES (1); EXPLAIN;",
    ] {
        let failed = error(&mut db, sql);
        assert_eq!(failed.kind(), ErrorKind::Syntax, "{sql}: {failed}");
        assert!(failed.span().is_some(), "{sql}");
    }
    let trailing = error(&mut db, "SELECT 1;\nEXPLAIN");
    assert_eq!(trailing.message(), "expected SELECT after EXPLAIN");
    assert_eq!(trailing.span().map(|s| (s.line, s.column)), Some((2, 1)));
    assert_eq!(
        rows(&mut db, "SELECT COUNT(*) FROM t"),
        vec![vec![int(0)]],
        "nothing ran"
    );
}
