//! Benchmark workloads. Each one builds its database untimed, times the
//! operation it is named after, and checks the result, so a broken
//! workload fails instead of reporting a time. `benches/workloads.rs`
//! times them; `tests/bench_smoke.rs` runs them at a tiny scale under
//! `cargo test`.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use cairn_exec::{Database, QueryResult, Value};

/// How much data a workload uses.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Scale {
    /// Rows in the main table; a multiple of 100.
    pub rows: u32,
}

impl Scale {
    /// For the smoke test.
    pub const TINY: Scale = Scale { rows: 100 };
    /// For `--quick` runs.
    pub const QUICK: Scale = Scale { rows: 2_000 };
    /// For full runs.
    pub const FULL: Scale = Scale { rows: 20_000 };
}

/// A named workload.
#[derive(Clone, Copy)]
pub struct Workload {
    /// The name printed in the results table.
    pub name: &'static str,
    /// What is timed.
    pub timed: &'static str,
    /// Builds the database at `path`, times the operation and checks it.
    pub run: fn(&Path, Scale) -> Result<Duration, String>,
}

/// Every workload, in table order.
pub const WORKLOADS: [Workload; 9] = [
    Workload {
        name: "insert_autocommit",
        timed: "rows/10 single-row INSERTs, each committed",
        run: insert_autocommit,
    },
    Workload {
        name: "insert_txn",
        timed: "BEGIN, rows INSERTs in batches of 100, COMMIT",
        run: insert_txn,
    },
    Workload {
        name: "pk_lookup",
        timed: "1000 SELECTs by INTEGER PRIMARY KEY",
        run: pk_lookup,
    },
    Workload {
        name: "index_lookup",
        timed: "1000 SELECT COUNT(*) by an indexed column",
        run: index_lookup,
    },
    Workload {
        name: "scan_filter",
        timed: "one full scan with a filter",
        run: scan_filter,
    },
    Workload {
        name: "join",
        timed: "SUM over an index join of rows/10 and rows",
        run: join,
    },
    Workload {
        name: "group_by",
        timed: "GROUP BY into 100 groups with COUNT and SUM",
        run: group_by,
    },
    Workload {
        name: "checkpoint",
        timed: "PRAGMA checkpoint of rows committed rows",
        run: checkpoint,
    },
    Workload {
        name: "reopen_recovery",
        timed: "open with log replay of rows committed rows",
        run: reopen_recovery,
    },
];

/// Fails unless `actual` equals `expected`.
pub fn expect(what: &str, actual: i64, expected: i64) -> Result<(), String> {
    if actual == expected {
        Ok(())
    } else {
        Err(format!("{what}: got {actual}, expected {expected}"))
    }
}

/// Removes a database file and its log.
pub fn remove_database(path: &Path) {
    let _ = std::fs::remove_file(path);
    let _ = std::fs::remove_file(wal_path(path));
}

fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

fn err(e: impl std::fmt::Display) -> String {
    e.to_string()
}

fn create(path: &Path) -> Result<Database, String> {
    remove_database(path);
    Database::create(path).map_err(err)
}

/// The single INTEGER of a one-row, one-column result.
fn scalar(db: &mut Database, sql: &str) -> Result<i64, String> {
    let results = db.execute(sql).map_err(err)?;
    match results.last() {
        Some(QueryResult::Rows { rows, .. }) => match rows.first().and_then(|r| r.first()) {
            Some(Value::Integer(v)) => Ok(*v),
            Some(Value::Null) => Ok(0),
            other => Err(format!("{sql}: unexpected value {other:?}")),
        },
        other => Err(format!("{sql}: unexpected result {other:?}")),
    }
}

/// `t(id INTEGER PRIMARY KEY, a INTEGER, g INTEGER, b TEXT)` with
/// `a = id`, `g = id % 100`, filled in one transaction.
fn fill(db: &mut Database, rows: u32) -> Result<(), String> {
    db.execute(
        "CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, g INTEGER, b TEXT);
         CREATE INDEX t_g ON t (g);
         BEGIN",
    )
    .map_err(err)?;
    let mut id = 1;
    while id <= rows {
        let batch: Vec<String> = (id..(id + 100).min(rows + 1))
            .map(|i| format!("({i}, {i}, {}, 'row {i}')", i % 100))
            .collect();
        db.execute(&format!("INSERT INTO t VALUES {}", batch.join(", ")))
            .map_err(err)?;
        id += 100;
    }
    db.execute("COMMIT").map_err(err)?;
    Ok(())
}

fn timed(f: impl FnOnce() -> Result<(), String>) -> Result<Duration, String> {
    let start = Instant::now();
    f()?;
    Ok(start.elapsed())
}

fn insert_autocommit(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    db.execute("CREATE TABLE t (id INTEGER PRIMARY KEY, a INTEGER, b TEXT)")
        .map_err(err)?;
    let count = scale.rows / 10;
    let elapsed = timed(|| {
        for i in 1..=count {
            db.execute(&format!("INSERT INTO t VALUES ({i}, {i}, 'row {i}')"))
                .map_err(err)?;
        }
        Ok(())
    })?;
    expect(
        "rows",
        scalar(&mut db, "SELECT COUNT(*) FROM t")?,
        i64::from(count),
    )?;
    db.close().map_err(err)?;
    Ok(elapsed)
}

fn insert_txn(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    let elapsed = timed(|| fill(&mut db, scale.rows))?;
    expect(
        "rows",
        scalar(&mut db, "SELECT COUNT(*) FROM t")?,
        i64::from(scale.rows),
    )?;
    db.close().map_err(err)?;
    Ok(elapsed)
}

fn pk_lookup(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    fill(&mut db, scale.rows)?;
    let mut found = 0;
    let elapsed = timed(|| {
        for i in 0..1000 {
            let id = i % scale.rows + 1;
            let a = scalar(&mut db, &format!("SELECT a FROM t WHERE id = {id}"))?;
            expect("a", a, i64::from(id))?;
            found += 1;
        }
        Ok(())
    })?;
    expect("found", found, 1000)?;
    db.close().map_err(err)?;
    Ok(elapsed)
}

fn index_lookup(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    fill(&mut db, scale.rows)?;
    let mut total = 0;
    let elapsed = timed(|| {
        for i in 0..1000 {
            total += scalar(
                &mut db,
                &format!("SELECT COUNT(*) FROM t WHERE g = {}", i % 100),
            )?;
        }
        Ok(())
    })?;
    expect("matches", total, 1000 * i64::from(scale.rows / 100))?;
    db.close().map_err(err)?;
    Ok(elapsed)
}

fn scan_filter(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    fill(&mut db, scale.rows)?;
    let mut count = 0;
    let elapsed = timed(|| {
        count = scalar(&mut db, "SELECT COUNT(*) FROM t WHERE a % 7 = 0")?;
        Ok(())
    })?;
    expect("multiples of 7", count, i64::from(scale.rows / 7))?;
    db.close().map_err(err)?;
    Ok(elapsed)
}

fn join(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    fill(&mut db, scale.rows)?;
    let groups = scale.rows / 10;
    db.execute("CREATE TABLE u (id INTEGER PRIMARY KEY, w INTEGER); BEGIN")
        .map_err(err)?;
    for start in (1..=groups).step_by(100) {
        let batch: Vec<String> = (start..(start + 100).min(groups + 1))
            .map(|i| format!("({i}, 1)"))
            .collect();
        db.execute(&format!("INSERT INTO u VALUES {}", batch.join(", ")))
            .map_err(err)?;
    }
    db.execute("COMMIT; CREATE INDEX t_a ON t (a)")
        .map_err(err)?;
    let mut sum = 0;
    let elapsed = timed(|| {
        sum = scalar(&mut db, "SELECT SUM(t.a) FROM u JOIN t ON t.a = u.id")?;
        Ok(())
    })?;
    let n = i64::from(groups);
    expect("sum", sum, n * (n + 1) / 2)?;
    db.close().map_err(err)?;
    Ok(elapsed)
}

fn group_by(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    fill(&mut db, scale.rows)?;
    let mut result = Vec::new();
    let elapsed = timed(|| {
        result = db
            .execute("SELECT g, COUNT(*), SUM(a) FROM t GROUP BY g")
            .map_err(err)?;
        Ok(())
    })?;
    let Some(QueryResult::Rows { rows, .. }) = result.last() else {
        return Err("GROUP BY returned no rows".into());
    };
    expect("groups", rows.len() as i64, 100)?;
    let (mut count, mut sum) = (0, 0);
    for row in rows {
        if let [_, Value::Integer(c), Value::Integer(s)] = row.as_slice() {
            count += c;
            sum += s;
        }
    }
    let n = i64::from(scale.rows);
    expect("count", count, n)?;
    expect("sum", sum, n * (n + 1) / 2)?;
    db.close().map_err(err)?;
    Ok(elapsed)
}

fn checkpoint(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    fill(&mut db, scale.rows)?;
    let before = std::fs::metadata(wal_path(path)).map_err(err)?.len();
    let elapsed = timed(|| db.execute("PRAGMA checkpoint").map(|_| ()).map_err(err))?;
    let after = std::fs::metadata(wal_path(path)).map_err(err)?.len();
    expect("log bytes after", after as i64, 0)?;
    if before == 0 {
        return Err("the log was already empty before the checkpoint".into());
    }
    db.close().map_err(err)?;
    Ok(elapsed)
}

fn reopen_recovery(path: &Path, scale: Scale) -> Result<Duration, String> {
    let mut db = create(path)?;
    fill(&mut db, scale.rows)?;
    let copy = path.with_extension("copy.db");
    remove_database(&copy);
    std::fs::copy(path, &copy).map_err(err)?;
    std::fs::copy(wal_path(path), wal_path(&copy)).map_err(err)?;
    db.close().map_err(err)?;
    let mut reopened = None;
    let elapsed = timed(|| {
        reopened = Some(Database::open(&copy).map_err(err)?);
        Ok(())
    })?;
    let mut reopened = reopened.ok_or("open did not run")?;
    let count = scalar(&mut reopened, "SELECT COUNT(*) FROM t")?;
    reopened.close().map_err(err)?;
    remove_database(&copy);
    expect("rows after recovery", count, i64::from(scale.rows))?;
    Ok(elapsed)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn expect_reports_a_wrong_result() {
        assert!(expect("rows", 3, 3).is_ok());
        assert_eq!(
            expect("rows", 2, 3),
            Err("rows: got 2, expected 3".to_string())
        );
    }

    #[test]
    fn scales_are_multiples_of_100() {
        for scale in [Scale::TINY, Scale::QUICK, Scale::FULL] {
            assert_eq!(scale.rows % 100, 0);
        }
    }
}
