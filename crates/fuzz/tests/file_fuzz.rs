//! Damaged database and log files (milestone M6 AC5).
//!
//! Two valid images are built in memory: A, closed (empty log), and B,
//! with its last transactions still only in the log. Each case mutates the
//! database or log bytes of one of them (`cairn_fuzz::mutate`), writes them
//! into a fresh in-memory file system, and opens the result. Then:
//!
//! - nothing panics or runs past the time limit (the runner reports both);
//! - a failed open is a `Corrupt` or `Storage` error; with an empty log it
//!   leaves the database file byte-for-byte as it was (with committed
//!   transactions in the log, open replays them into the database file
//!   before it validates the header, as recovery after a crash does);
//! - after a successful open, `check()`, the fixed queries and `close()`
//!   either succeed or fail with a `Corrupt` or `Storage` error. Pages have
//!   no checksums, so damage can leave a valid catalog that names a table
//!   or column differently (a bit flip turns `person` into `persof`). A
//!   query that then fails with `NotFound` passes only if the schema the
//!   damaged file reports differs from the undamaged schema.
//!
//! A sample of cases repeats the same bytes through real files.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

use cairn_exec::{Database, ErrorKind, ExecError, TableSchema};
use cairn_fuzz::mutate::{apply, pick};
use cairn_fuzz::rng::Rng;
use cairn_fuzz::runner::{Budget, CaseResult, run};
use cairn_storage::fault::FaultVfs;
use cairn_storage::{Options, OsVfs, Vfs};

const BUDGET: Budget = Budget {
    seed: 0xF11E_F022_0000_0005,
    cases: 300,
    timeout: Duration::from_secs(10),
};

const DB: &str = "/fuzz.db";
const WAL: &str = "/fuzz.db-wal";

/// Every 15th case also runs through real files.
const REAL_FILE_EVERY: u32 = 15;

const QUERIES: [&str; 7] = [
    "SELECT COUNT(*) FROM people",
    "SELECT * FROM people",
    "SELECT name FROM people WHERE id = 17",
    "SELECT COUNT(*) FROM orders WHERE person = 3",
    "SELECT p.name, SUM(o.amount) FROM people AS p JOIN orders AS o ON o.person = p.id GROUP BY p.name",
    "SELECT * FROM tags ORDER BY tag",
    "PRAGMA integrity_check",
];

struct Images {
    closed: (Vec<u8>, Vec<u8>),
    logged: (Vec<u8>, Vec<u8>),
}

fn workload(db: &mut Database, from: u32, to: u32) -> Result<(), ExecError> {
    for id in from..to {
        db.execute(&format!(
            "INSERT INTO people VALUES ({id}, 'person {id}', {});
             INSERT INTO orders VALUES ({id}, {}, {}.5)",
            id % 7,
            id % 20,
            id
        ))?;
    }
    Ok(())
}

fn build(close: bool) -> Result<(Vec<u8>, Vec<u8>), ExecError> {
    let vfs = Arc::new(FaultVfs::new());
    let mut db = Database::create_with(vfs.clone(), Path::new(DB), Options::default())?;
    db.execute(
        "CREATE TABLE people (id INTEGER PRIMARY KEY, name TEXT NOT NULL UNIQUE, age INTEGER);
         CREATE TABLE orders (id INTEGER PRIMARY KEY, person INTEGER, amount REAL);
         CREATE INDEX orders_person ON orders (person);
         CREATE TABLE tags (tag TEXT PRIMARY KEY);
         INSERT INTO tags VALUES ('a'), ('b'), ('c')",
    )?;
    workload(&mut db, 0, 120)?;
    db.execute("DELETE FROM people WHERE id % 5 = 0; PRAGMA checkpoint")?;
    workload(&mut db, 120, 140)?;
    let read = |path: &str| vfs.read_file(Path::new(path)).unwrap_or_default();
    if close {
        db.close()?;
        return Ok((read(DB), read(WAL)));
    }
    let images = (read(DB), read(WAL));
    drop(db);
    Ok(images)
}

fn images() -> &'static Images {
    static IMAGES: OnceLock<Images> = OnceLock::new();
    IMAGES.get_or_init(|| Images {
        closed: build(true).expect("build the closed image"),
        logged: build(false).expect("build the logged image"),
    })
}

/// The schema both undamaged images report.
fn undamaged_schema() -> &'static [TableSchema] {
    static SCHEMA: OnceLock<Vec<TableSchema>> = OnceLock::new();
    SCHEMA.get_or_init(|| {
        let (db_bytes, _) = &images().closed;
        let vfs = Arc::new(FaultVfs::new());
        vfs.write_file(Path::new(DB), db_bytes.clone());
        let mut db = Database::open_with(vfs, Path::new(DB), Options::default()).expect("open");
        let schema = db.schema().expect("schema");
        db.close().expect("close");
        schema
    })
}

/// A query's outcome must be a result, a `Corrupt` or `Storage` error, or
/// `NotFound` when the damage renamed or removed a catalog name.
fn query_ok(db: &mut Database, sql: &str) -> Result<(), String> {
    let error = match db.execute(sql) {
        Ok(_) => return Ok(()),
        Err(e) => e,
    };
    match error.kind() {
        ErrorKind::Corrupt | ErrorKind::Storage => Ok(()),
        ErrorKind::NotFound => match db.schema() {
            Ok(schema) if schema.as_slice() != undamaged_schema() => Ok(()),
            Ok(_) => Err(format!(
                "{sql}: NotFound with the undamaged schema: {error}"
            )),
            Err(e) => typed(Err(e), &[ErrorKind::Corrupt, ErrorKind::Storage], "schema"),
        },
        kind => Err(format!("{sql}: unexpected {kind:?} error: {error}")),
    }
}

fn typed(result: Result<(), ExecError>, allowed: &[ErrorKind], what: &str) -> Result<(), String> {
    match result {
        Err(e) if !allowed.contains(&e.kind()) => {
            Err(format!("{what}: unexpected {:?} error: {e}", e.kind()))
        }
        _ => Ok(()),
    }
}

/// Opens the files at `path` through `vfs` and runs everything.
fn exercise(
    vfs: Arc<dyn Vfs>,
    path: &Path,
    read_db: impl Fn() -> Vec<u8>,
    db_bytes: &[u8],
    log_empty: bool,
) -> Result<(), String> {
    let mut db = match Database::open_with(vfs, path, Options::default()) {
        Ok(db) => db,
        Err(e) => {
            typed(Err(e), &[ErrorKind::Corrupt, ErrorKind::Storage], "open")?;
            if log_empty && read_db() != db_bytes {
                return Err("a failed open changed the database file".into());
            }
            return Ok(());
        }
    };
    typed(
        db.check(),
        &[ErrorKind::Corrupt, ErrorKind::Storage],
        "check",
    )?;
    for sql in QUERIES {
        query_ok(&mut db, sql)?;
    }
    typed(
        db.close(),
        &[ErrorKind::Corrupt, ErrorKind::Storage],
        "close",
    )
}

fn case(index: u32, rng: &mut Rng) -> CaseResult {
    let source = if rng.chance(1, 2) {
        &images().closed
    } else {
        &images().logged
    };
    let (mut db_bytes, mut wal_bytes) = source.clone();
    let mutation = pick(rng);
    let target = if !wal_bytes.is_empty() && rng.chance(1, 4) {
        apply(mutation, &mut wal_bytes, rng);
        "log"
    } else {
        apply(mutation, &mut db_bytes, rng);
        "database"
    };
    let reproducer = format!(
        "{mutation:?} of the {target} file of the {} image",
        if std::ptr::eq(source, &images().closed) {
            "closed"
        } else {
            "logged"
        }
    );
    let fail = |detail: String| CaseResult::Fail {
        reproducer: reproducer.clone(),
        detail,
    };
    let vfs = Arc::new(FaultVfs::new());
    vfs.write_file(Path::new(DB), db_bytes.clone());
    if !wal_bytes.is_empty() {
        vfs.write_file(Path::new(WAL), wal_bytes.clone());
    }
    let reader = vfs.clone();
    let read_db = move || reader.read_file(Path::new(DB)).unwrap_or_default();
    let log_empty = wal_bytes.is_empty();
    if let Err(detail) = exercise(vfs, Path::new(DB), read_db, &db_bytes, log_empty) {
        return fail(detail);
    }
    if index.is_multiple_of(REAL_FILE_EVERY)
        && let Err(detail) = on_disk(index, &db_bytes, &wal_bytes)
    {
        return fail(format!("real files: {detail}"));
    }
    CaseResult::Pass
}

/// Removes the files when dropped.
struct TempFiles(PathBuf);

impl Drop for TempFiles {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.0);
        let _ = std::fs::remove_file(wal(&self.0));
    }
}

fn wal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

fn on_disk(index: u32, db_bytes: &[u8], wal_bytes: &[u8]) -> Result<(), String> {
    let path =
        std::env::temp_dir().join(format!("cairn-filefuzz-{}-{index}.db", std::process::id()));
    let files = TempFiles(path.clone());
    std::fs::write(&files.0, db_bytes).map_err(|e| e.to_string())?;
    if !wal_bytes.is_empty() {
        std::fs::write(wal(&files.0), wal_bytes).map_err(|e| e.to_string())?;
    }
    let read_path = path.clone();
    let read_db = move || std::fs::read(&read_path).unwrap_or_default();
    exercise(
        Arc::new(OsVfs),
        &path,
        read_db,
        db_bytes,
        wal_bytes.is_empty(),
    )
}

#[test]
fn damaged_files_fail_cleanly() {
    let _ = images();
    run("file_fuzz", BUDGET, |index, mut rng| case(index, &mut rng));
}

#[test]
fn the_undamaged_images_open_and_pass_every_query() {
    for (db_bytes, wal_bytes) in [&images().closed, &images().logged] {
        let vfs = Arc::new(FaultVfs::new());
        vfs.write_file(Path::new(DB), db_bytes.clone());
        if !wal_bytes.is_empty() {
            vfs.write_file(Path::new(WAL), wal_bytes.clone());
        }
        let mut db = Database::open_with(vfs, Path::new(DB), Options::default()).expect("open");
        db.check().expect("check");
        for sql in QUERIES {
            db.execute(sql).unwrap_or_else(|e| panic!("{sql}: {e}"));
        }
        db.close().expect("close");
    }
    assert_eq!(undamaged_schema().len(), 3, "people, orders and tags");
    assert!(
        images().closed.1.is_empty(),
        "the closed image has an empty log"
    );
    assert!(
        !images().logged.1.is_empty(),
        "the logged image keeps frames in its log"
    );
}
