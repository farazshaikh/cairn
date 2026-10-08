//! Integer `SUM` overflows only when the total does not fit a 64-bit
//! integer, whatever order the access path reads the rows in (M6 issue
//! `m6-sum-partial-overflow`).

use cairn_exec::{Database, ErrorKind, QueryResult, Value};

mod common;

use common::remove_database;

struct TempDb(std::path::PathBuf, Database);

impl TempDb {
    fn new(tag: &str) -> TempDb {
        let path = std::env::temp_dir().join(format!("cairn-sum-{}-{tag}.db", std::process::id()));
        remove_database(&path);
        let db = Database::create(&path).expect("create");
        TempDb(path, db)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        remove_database(&self.0);
    }
}

fn sum(db: &mut Database, sql: &str) -> Result<Value, ErrorKind> {
    match db.execute(sql) {
        Ok(mut results) => match results.pop() {
            Some(QueryResult::Rows { mut rows, .. }) => Ok(rows.remove(0).remove(0)),
            other => panic!("expected rows, got {other:?}"),
        },
        Err(error) => Err(error.kind()),
    }
}

fn access_path(db: &mut Database, select: &str) -> String {
    let results = db.execute(&format!("EXPLAIN {select}")).expect("explain");
    let Some(QueryResult::Rows { rows, .. }) = results.last() else {
        panic!("explain returned no rows");
    };
    match rows.last().and_then(|row| row.get(2)) {
        Some(Value::Text(operation)) => operation.clone(),
        other => panic!("unexpected EXPLAIN row {other:?}"),
    }
}

#[test]
fn a_partial_sum_that_overflows_is_not_an_error_when_the_total_fits() {
    let mut tmp = TempDb::new("partial");
    let db = &mut tmp.1;
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, v INTEGER);
         INSERT INTO t VALUES (1, 9223372036854775807), (2, 1), (3, -1);
         CREATE INDEX t_v ON t (v);",
    )
    .expect("setup");
    let total = Ok(Value::Integer(i64::MAX));

    let scan = "SELECT SUM(v) FROM t";
    assert_eq!(access_path(db, scan), "SCAN");
    assert_eq!(sum(db, scan), total);

    let by_key = "SELECT SUM(v) FROM t WHERE id > 0";
    assert_eq!(access_path(db, by_key), "PRIMARY KEY RANGE");
    assert_eq!(sum(db, by_key), total);

    let by_index = "SELECT SUM(v) FROM t WHERE v > -10";
    assert_eq!(access_path(db, by_index), "INDEX RANGE");
    assert_eq!(sum(db, by_index), total);
}

#[test]
fn a_total_outside_i64_is_still_an_overflow_error() {
    let mut tmp = TempDb::new("total");
    let db = &mut tmp.1;
    db.execute(
        "CREATE TABLE t (v INTEGER);
         INSERT INTO t VALUES (9223372036854775807), (1);
         CREATE TABLE u (v INTEGER);
         INSERT INTO u VALUES (-9223372036854775807 - 1), (-1);",
    )
    .expect("setup");
    assert_eq!(sum(db, "SELECT SUM(v) FROM t"), Err(ErrorKind::Arithmetic));
    assert_eq!(sum(db, "SELECT SUM(v) FROM u"), Err(ErrorKind::Arithmetic));
    assert_eq!(
        sum(db, "SELECT SUM(v) FROM u WHERE v < 0"),
        Err(ErrorKind::Arithmetic)
    );
}

#[test]
fn sums_at_the_limits_are_exact() {
    let mut tmp = TempDb::new("limits");
    let db = &mut tmp.1;
    db.execute(
        "CREATE TABLE t (g INTEGER, v INTEGER);
         INSERT INTO t VALUES (1, -9223372036854775807 - 1), (1, 5), (1, -5),
                              (2, 9223372036854775807), (2, -9223372036854775807 - 1);",
    )
    .expect("setup");
    let results = db
        .execute("SELECT g, SUM(v) FROM t GROUP BY g ORDER BY g")
        .expect("group sum");
    assert_eq!(
        results,
        vec![QueryResult::Rows {
            columns: vec!["g".into(), "sum(v)".into()],
            rows: vec![
                vec![Value::Integer(1), Value::Integer(i64::MIN)],
                vec![Value::Integer(2), Value::Integer(-1)],
            ],
        }]
    );
}
