//! PRAGMA checkpoint and PRAGMA integrity_check (milestone M4 AC4, AC7).
//! Corruptions are made through the storage API on a closed database and
//! must be reported, never panic.

use std::path::Path;
use std::sync::Arc;

use cairn_exec::{Database, ErrorKind, QueryResult, Value};
use cairn_storage::fault::FaultVfs;
use cairn_storage::{BTree, Options, Page, PageId, Pager};

const DB: &str = "/pragma.db";

fn options() -> Options {
    Options::new(64, 1000)
}

fn create(vfs: &FaultVfs) -> Database {
    Database::create_with(Arc::new(vfs.clone()), Path::new(DB), options()).expect("create")
}

fn open(vfs: &FaultVfs) -> Database {
    Database::open_with(Arc::new(vfs.clone()), Path::new(DB), options()).expect("open")
}

fn pager(vfs: &FaultVfs) -> Pager {
    Pager::open_with(Arc::new(vfs.clone()), Path::new(DB), options()).expect("pager")
}

fn integrity(db: &mut Database) -> Vec<String> {
    match db.execute("PRAGMA integrity_check").expect("pragma").pop() {
        Some(QueryResult::Rows { columns, rows }) => {
            assert_eq!(columns, ["integrity_check"]);
            rows.into_iter()
                .map(|row| match row.as_slice() {
                    [Value::Text(text)] => text.clone(),
                    other => panic!("unexpected row {other:?}"),
                })
                .collect()
        }
        other => panic!("{other:?}"),
    }
}

/// A database with a table `t` (INTEGER PRIMARY KEY, unique and plain
/// indexed columns), a dropped table and some deleted rows, closed.
fn populated() -> FaultVfs {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, u TEXT UNIQUE, n INTEGER);
         CREATE INDEX t_n ON t (n);
         CREATE TABLE gone (x TEXT);",
    )
    .expect("schema");
    for chunk in 0..6 {
        let values: Vec<String> = (0..50)
            .map(|i| {
                let id = chunk * 50 + i;
                format!("({id}, 'u{id:05}-{}', {})", "p".repeat(40), id % 7)
            })
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", values.join(", ")))
            .expect("insert");
        db.execute(&format!("INSERT INTO gone VALUES ('{}')", "g".repeat(300)))
            .expect("insert");
    }
    db.execute("DELETE FROM t WHERE id % 3 = 0; DROP TABLE gone")
        .expect("delete");
    db.close().expect("close");
    vfs
}

/// The root page recorded in the catalog entry `tag` + `name` (table `T`,
/// index `I`); bytes 1..5 of either entry hold the root.
fn root(pager: &mut Pager, tag: u8, name: &str) -> PageId {
    let catalog = BTree::open(pager.root("catalog").expect("catalog root"));
    let mut key = vec![tag];
    key.extend_from_slice(name.as_bytes());
    let value = catalog.get(pager, &key).expect("read").expect("entry");
    PageId(u32::from_le_bytes(
        value[1..5].try_into().expect("root bytes"),
    ))
}

#[test]
fn a_healthy_database_reports_ok() {
    let vfs = populated();
    let mut db = open(&vfs);
    assert_eq!(integrity(&mut db), ["ok"]);
    db.check().expect("check");
    let results = db
        .execute("pragma Integrity_Check; PRAGMA checkpoint;")
        .expect("pragmas");
    assert_eq!(results.len(), 2);
}

#[test]
fn checkpoint_empties_the_log_and_keeps_the_data() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    db.execute("CREATE TABLE t (a INTEGER); INSERT INTO t VALUES (1), (2)")
        .expect("setup");
    let wal = Path::new("/pragma.db-wal");
    assert!(vfs.read_file(wal).is_some_and(|b| !b.is_empty()));
    let result = db.execute("PRAGMA checkpoint").expect("checkpoint");
    assert_eq!(
        result,
        vec![QueryResult::Rows {
            columns: vec!["checkpoint".into()],
            rows: vec![vec![Value::Text("ok".into())]],
        }]
    );
    assert_eq!(vfs.read_file(wal).map(|b| b.len()), Some(0));
    let e = db
        .execute("BEGIN; PRAGMA checkpoint")
        .expect_err("inside a transaction");
    assert_eq!(e.kind(), ErrorKind::Transaction);
    assert_eq!(e.message(), "cannot checkpoint inside a transaction");
    db.execute("ROLLBACK").expect("rollback");

    let mut reader = pager(&vfs);
    db.execute("INSERT INTO t VALUES (3)").expect("insert");
    reader.begin_read().expect("register a reader");
    let busy = db
        .execute("PRAGMA checkpoint")
        .expect_err("a reader is active");
    assert_eq!(busy.kind(), ErrorKind::Busy);
    reader.end_read();
    db.execute("PRAGMA checkpoint").expect("checkpoint");
}

#[test]
fn a_missing_or_extra_index_entry_is_reported() {
    let vfs = populated();
    let mut p = pager(&vfs);
    let index_root = root(&mut p, b'I', "t_n");
    let mut index = BTree::open(index_root);
    let first = index
        .range(
            &mut p,
            std::ops::Bound::Unbounded,
            std::ops::Bound::Unbounded,
        )
        .expect("range")
        .next()
        .expect("an entry")
        .expect("read");
    index.delete(&mut p, &first.0).expect("delete");
    assert_eq!(index.root(), index_root, "a single delete keeps the root");
    p.close().expect("close");
    let mut db = open(&vfs);
    assert_eq!(integrity(&mut db), ["index t_n does not match table t"]);
    let e = db.check().expect_err("check reports it too");
    assert_eq!(e.kind(), ErrorKind::Corrupt);
    drop(db);

    let mut p = pager(&vfs);
    let mut index = BTree::open(root(&mut p, b'I', "t_n"));
    index.insert(&mut p, &first.0, &[]).expect("restore");
    let mut extra = first.0.clone();
    *extra.last_mut().expect("row key byte") ^= 0x7F;
    index.insert(&mut p, &extra, &[]).expect("extra entry");
    p.close().expect("close");
    let mut db = open(&vfs);
    assert_eq!(integrity(&mut db), ["index t_n does not match table t"]);
}

#[test]
fn a_page_both_free_and_in_use_is_reported() {
    let vfs = populated();
    let mut p = pager(&vfs);
    let table_root = root(&mut p, b'T', "t");
    let pages = BTree::open(table_root).pages(&mut p).expect("pages");
    let leaf = *pages.last().expect("a leaf");
    p.free(leaf).expect("free a live leaf");
    p.close().expect("close");
    let mut db = open(&vfs);
    let problems = integrity(&mut db);
    let expected = format!("page {} is both free and in use by table t", leaf.0);
    assert!(problems.contains(&expected), "{problems:?}");
    assert!(
        problems.len() > 1,
        "the table check also fails: {problems:?}"
    );
}

#[test]
fn a_leaked_page_is_reported() {
    let vfs = populated();
    let mut p = pager(&vfs);
    let leaked = p.allocate().expect("allocate");
    p.close().expect("close");
    let mut db = open(&vfs);
    assert_eq!(
        integrity(&mut db),
        [format!("page {} is neither free nor in use", leaked.0)]
    );
}

#[test]
fn a_btree_order_violation_is_reported() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    db.execute("CREATE TABLE s (id INTEGER PRIMARY KEY); INSERT INTO s VALUES (1), (2), (3)")
        .expect("setup");
    db.close().expect("close");
    let mut p = pager(&vfs);
    let leaf = root(&mut p, b'T', "s");
    let mut page: Page = p.read(leaf).expect("read");
    // Leaf cell 0 starts at 8: key length, value length, then the 8-byte row
    // key. Raising its last byte makes key 1 sort after key 2.
    page.bytes_mut()[8 + 4 + 7] = 0xFF;
    p.write(leaf, &page).expect("write");
    p.close().expect("close");
    let mut db = open(&vfs);
    let problems = integrity(&mut db);
    assert!(
        problems
            .iter()
            .any(|p| p.starts_with("table s: page") && p.contains("not in ascending order")),
        "{problems:?}"
    );
    assert!(db.check().is_err());
}

#[test]
fn unknown_and_malformed_pragmas_are_errors_with_spans() {
    let vfs = FaultVfs::new();
    let mut db = create(&vfs);
    let e = db
        .execute("SELECT 1;\nPRAGMA journal_mode")
        .expect_err("unknown");
    assert_eq!(e.kind(), ErrorKind::Unsupported);
    assert_eq!(e.message(), "unknown pragma: journal_mode");
    assert_eq!(e.span().map(|s| (s.line, s.column)), Some((2, 8)));
    for (sql, kind) in [
        ("PRAGMA", ErrorKind::Syntax),
        ("PRAGMA;", ErrorKind::Syntax),
        ("PRAGMA a b", ErrorKind::Syntax),
        ("EXPLAIN PRAGMA checkpoint", ErrorKind::Unsupported),
    ] {
        assert_eq!(db.execute(sql).expect_err(sql).kind(), kind, "{sql}");
    }
    db.execute("CREATE TABLE \"pragma\" (x INTEGER); SELECT x FROM \"pragma\"")
        .expect("a quoted name is not a directive");
}
