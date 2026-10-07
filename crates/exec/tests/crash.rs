//! Crash safety end to end (milestone M4 AC5, AC6, with AC1 and AC7).
//!
//! A fixed-seed workload of at least 200 committed transactions (explicit
//! and autocommit, rollbacks, failing statements, DDL) runs against the
//! fault-injecting file layer. A dry run counts its writes `W` and syncs
//! `S`. Then, for every position, the workload is run again and stopped
//! there:
//!
//! - sweep A: after write N, for N in 1..=W, checking both the process-crash
//!   image (every write kept) and the power-loss image (synced bytes only);
//! - sweep B: write N torn to a seeded byte prefix, process-crash image;
//! - sweep C: after sync N, for N in 1..=S, power-loss image.
//!
//! After each crash the database is reopened (which replays the log) and
//! must equal the model after exactly k committed transactions, where k is
//! the number whose COMMIT returned Ok (durability) or one more (a commit
//! record written but not acknowledged). `check()` must pass and
//! `PRAGMA integrity_check` must return `ok`. A crash before `create`
//! returned may leave no database at all.

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;
use std::time::Instant;

use cairn_exec::{Database, ErrorKind, QueryResult, Value};
use cairn_storage::Options;
use cairn_storage::fault::{CrashMode, Fault, FaultVfs, Op};

const DB: &str = "/crash.db";
const WAL: &str = "/crash.db-wal";
const SEED: u64 = 0x9E37_79B9_7F4A_7C15;
/// Committed transactions in the workload, above AC6's 200.
const COMMITTED: usize = 230;
/// A small checkpoint threshold so the sweeps also crash inside many
/// checkpoints; the 1000-frame default has its own storage test.
const CHECKPOINT_FRAMES: u32 = 64;

fn options() -> Options {
    Options::new(64, CHECKPOINT_FRAMES)
}

/// Marsaglia's xorshift64 (13, 7, 17), as in the storage tests.
struct XorShift64(u64);

impl XorShift64 {
    fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    fn below(&mut self, n: usize) -> usize {
        (self.next() % n as u64) as usize
    }
}

/// Table name to rows ordered by the first column; absent tables are absent.
type Model = BTreeMap<String, BTreeMap<i64, Vec<Value>>>;

struct Statement {
    sql: String,
    /// Whether the statement fails by design (a constraint violation).
    fails: bool,
    /// Whether an Ok result makes a transaction durable (COMMIT, or an
    /// autocommit write).
    commits: bool,
}

struct Workload {
    statements: Vec<Statement>,
    /// `models[k]` is the state after `k` committed transactions.
    models: Vec<Model>,
    /// Every table name the workload ever uses.
    tables: Vec<String>,
}

fn generate() -> Workload {
    let mut rng = XorShift64(SEED);
    let mut w = Workload {
        statements: Vec::new(),
        models: vec![Model::new()],
        tables: vec!["kv".to_string()],
    };
    let mut model = Model::new();
    for sql in [
        "CREATE TABLE kv (k INTEGER PRIMARY KEY, v TEXT NOT NULL, n INTEGER)",
        "CREATE INDEX kv_n ON kv (n)",
    ] {
        model.entry("kv".to_string()).or_default();
        w.statements.push(stmt(sql, false, true));
        w.models.push(model.clone());
    }
    let mut sides = 0usize;
    while w.models.len() <= COMMITTED {
        let roll = rng.below(100);
        if roll < 70 {
            let commit = roll < 55;
            let mut scratch = model.clone();
            w.statements.push(stmt("BEGIN", false, false));
            for _ in 0..1 + rng.below(4) {
                if rng.below(10) == 0 {
                    w.statements.push(stmt(
                        "INSERT INTO kv VALUES (1000000, NULL, 0)",
                        true,
                        false,
                    ));
                }
                let sql = dml(&mut rng, &mut scratch);
                w.statements.push(stmt(&sql, false, false));
            }
            if commit {
                w.statements.push(stmt("COMMIT", false, true));
                model = scratch;
                w.models.push(model.clone());
            } else {
                w.statements.push(stmt("ROLLBACK", false, false));
            }
        } else if roll < 95 {
            let sql = dml(&mut rng, &mut model);
            w.statements.push(stmt(&sql, false, true));
            w.models.push(model.clone());
        } else {
            let existing: Vec<String> = model.keys().filter(|t| *t != "kv").cloned().collect();
            if existing.is_empty() || rng.below(2) == 0 {
                let name = format!("side_{sides}");
                sides += 1;
                let x = text(&mut rng);
                let x = x.get(..x.len().min(100)).unwrap_or_default().to_string();
                for sql in [
                    "BEGIN".to_string(),
                    format!("CREATE TABLE {name} (id INTEGER PRIMARY KEY, x TEXT)"),
                    format!("INSERT INTO {name} VALUES (1, '{x}')"),
                    format!("CREATE INDEX {name}_x ON {name} (x)"),
                ] {
                    w.statements.push(stmt(&sql, false, false));
                }
                w.statements.push(stmt("COMMIT", false, true));
                let row = vec![Value::Integer(1), Value::Text(x)];
                model.insert(name.clone(), BTreeMap::from([(1, row)]));
                w.tables.push(name);
            } else {
                let name = existing[rng.below(existing.len())].clone();
                w.statements
                    .push(stmt(&format!("DROP TABLE {name}"), false, true));
                model.remove(&name);
            }
            w.models.push(model.clone());
        }
    }
    w
}

fn stmt(sql: &str, fails: bool, commits: bool) -> Statement {
    Statement {
        sql: sql.to_string(),
        fails,
        commits,
    }
}

fn text(rng: &mut XorShift64) -> String {
    let letter = char::from(b'a' + rng.below(26) as u8);
    letter.to_string().repeat(8 + rng.below(393))
}

/// One INSERT, UPDATE or DELETE on `kv` that succeeds, applied to `model`.
fn dml(rng: &mut XorShift64, model: &mut Model) -> String {
    let kv = model.entry("kv".to_string()).or_default();
    let roll = rng.below(10);
    if roll < 5 || kv.is_empty() {
        let mut k = rng.below(500) as i64;
        while kv.contains_key(&k) {
            k = (k + 1) % 500;
        }
        let (v, n) = (text(rng), rng.below(20) as i64);
        let sql = format!("INSERT INTO kv VALUES ({k}, '{v}', {n})");
        kv.insert(
            k,
            vec![Value::Integer(k), Value::Text(v), Value::Integer(n)],
        );
        return sql;
    }
    let keys: Vec<i64> = kv.keys().copied().collect();
    let k = keys[rng.below(keys.len())];
    if roll < 8 {
        let (v, n) = (text(rng), rng.below(20) as i64);
        kv.insert(
            k,
            vec![Value::Integer(k), Value::Text(v.clone()), Value::Integer(n)],
        );
        return format!("UPDATE kv SET v = '{v}', n = {n} WHERE k = {k}");
    }
    kv.remove(&k);
    format!("DELETE FROM kv WHERE k = {k}")
}

/// What a run achieved before it stopped.
struct Outcome {
    created: bool,
    acked: usize,
}

/// Runs the workload until it ends or the file layer stops. Before the stop
/// every statement must behave as the model expects.
fn run(vfs: &FaultVfs, w: &Workload) -> Outcome {
    let mut outcome = Outcome {
        created: false,
        acked: 0,
    };
    let Ok(mut db) = Database::create_with(Arc::new(vfs.clone()), Path::new(DB), options()) else {
        assert!(vfs.stopped(), "create failed without an injected fault");
        return outcome;
    };
    outcome.created = true;
    if vfs.stopped() {
        return outcome;
    }
    for s in &w.statements {
        let result = db.execute(&s.sql);
        if result.is_ok() && s.commits {
            outcome.acked += 1;
        }
        if vfs.stopped() {
            return outcome;
        }
        match (&result, s.fails) {
            (Ok(_), false) => {}
            (Err(e), true) if e.kind() == ErrorKind::Constraint => {}
            _ => panic!("{}: unexpected {result:?}", s.sql),
        }
    }
    let _ = db.close();
    outcome
}

fn dump(db: &mut Database, tables: &[String]) -> Model {
    let mut model = Model::new();
    for table in tables {
        match db.execute(&format!("SELECT * FROM {table} ORDER BY 1")) {
            Ok(mut results) => {
                let Some(QueryResult::Rows { rows, .. }) = results.pop() else {
                    panic!("SELECT returned no rows result");
                };
                let rows = rows
                    .into_iter()
                    .map(|row| match row.first() {
                        Some(Value::Integer(k)) => (*k, row),
                        other => panic!("first column {other:?}"),
                    })
                    .collect();
                model.insert(table.clone(), rows);
            }
            Err(e) if e.kind() == ErrorKind::NotFound => {}
            Err(e) => panic!("dump {table}: {e}"),
        }
    }
    model
}

/// Reopens a crash image and checks it against the models.
fn verify(image: &FaultVfs, w: &Workload, outcome: &Outcome, label: &str) {
    let mut db = match Database::open_with(Arc::new(image.clone()), Path::new(DB), options()) {
        Ok(db) => db,
        Err(e) => {
            assert!(
                !outcome.created,
                "{label}: reopen failed after create returned: {e}"
            );
            return;
        }
    };
    let state = dump(&mut db, &w.tables);
    let candidates = [outcome.acked, outcome.acked + 1];
    let matched = candidates
        .iter()
        .any(|&k| w.models.get(k).is_some_and(|m| *m == state));
    assert!(
        matched,
        "{label}: state matches no committed prefix in {candidates:?} (acked {})",
        outcome.acked
    );
    if let Err(e) = db.check() {
        panic!("{label}: check failed: {e}");
    }
    let integrity = db
        .execute("PRAGMA integrity_check")
        .expect("integrity_check");
    let expected = vec![QueryResult::Rows {
        columns: vec!["integrity_check".into()],
        rows: vec![vec![Value::Text("ok".into())]],
    }];
    assert_eq!(integrity, expected, "{label}");
}

/// Runs `body` for every position in `1..=count` on all available cores.
fn sweep(count: u64, body: impl Fn(u64) + Sync) {
    let workers = std::thread::available_parallelism().map_or(1, |n| n.get()) as u64;
    std::thread::scope(|scope| {
        for worker in 0..workers {
            let body = &body;
            scope.spawn(move || {
                let mut n = 1 + worker;
                while n <= count {
                    body(n);
                    n += workers;
                }
            });
        }
    });
}

#[test]
fn a_crash_at_every_write_and_sync_recovers_a_committed_prefix() {
    let started = Instant::now();
    let w = generate();
    let committed = w.models.len() - 1;
    assert!(committed >= 200, "{committed} committed transactions");

    let dry = FaultVfs::new();
    let full = run(&dry, &w);
    assert_eq!(
        full.acked, committed,
        "the fault-free run commits everything"
    );
    let (writes, syncs) = (dry.writes(), dry.syncs());
    let again = FaultVfs::new();
    run(&again, &w);
    assert_eq!(
        (again.writes(), again.syncs()),
        (writes, syncs),
        "the workload is deterministic"
    );
    let events = dry.events();
    let checkpoints = events
        .iter()
        .filter(|e| e.path == Path::new(WAL) && e.op == Op::SetLen(0))
        .count();
    assert!(checkpoints >= 3, "{checkpoints} checkpoints");
    verify(&dry, &w, &full, "fault-free run");

    let crash_runs = std::sync::atomic::AtomicU64::new(0);
    sweep(writes, |n| {
        let vfs = FaultVfs::new();
        vfs.arm(Fault::StopAfterWrite(n));
        let outcome = run(&vfs, &w);
        assert!(vfs.stopped(), "write {n} of {writes} was never reached");
        for mode in [CrashMode::KeepWrites, CrashMode::SyncedOnly] {
            verify(
                &vfs.crash(mode),
                &w,
                &outcome,
                &format!("stop after write {n}, {mode:?}"),
            );
        }
        crash_runs.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    });
    assert_eq!(
        crash_runs.into_inner(),
        writes,
        "every write position was crashed"
    );

    sweep(writes, |n| {
        let vfs = FaultVfs::new();
        let keep_seed = n.wrapping_mul(0x2545_F491_4F6C_DD1D);
        vfs.arm(Fault::TearWrite {
            write: n,
            keep_seed,
        });
        let outcome = run(&vfs, &w);
        let image = vfs.crash(CrashMode::KeepWrites);
        verify(&image, &w, &outcome, &format!("torn write {n}"));
    });

    sweep(syncs, |n| {
        let vfs = FaultVfs::new();
        vfs.arm(Fault::StopAfterSync(n));
        let outcome = run(&vfs, &w);
        verify(
            &vfs.crash(CrashMode::SyncedOnly),
            &w,
            &outcome,
            &format!("stop after sync {n}"),
        );
    });

    eprintln!(
        "crash sweep: {committed} committed transactions, {} statements, {writes} writes, \
         {syncs} syncs, {checkpoints} checkpoints, {:.1?}",
        w.statements.len(),
        started.elapsed()
    );
}

/// Crashing while reopening replays the log, at every write and sync of the
/// replay, and reopening again gives the state of an uninterrupted reopen.
#[test]
fn a_crash_during_replay_converges_on_the_next_open() {
    let w = generate();
    let dry = FaultVfs::new();
    run(&dry, &w);
    let middle = dry.writes() / 2;
    let vfs = FaultVfs::new();
    vfs.arm(Fault::StopAfterWrite(middle));
    let outcome = run(&vfs, &w);
    let image = vfs.crash(CrashMode::KeepWrites);
    let log_len = image.read_file(Path::new(WAL)).map_or(0, |b| b.len());
    assert!(log_len > 0, "the crash image has a log to replay");
    let reference = {
        let clean = image.crash(CrashMode::KeepWrites);
        let mut db = Database::open_with(Arc::new(clean), Path::new(DB), options()).expect("open");
        dump(&mut db, &w.tables)
    };
    verify(
        &image.crash(CrashMode::KeepWrites),
        &w,
        &outcome,
        "clean reopen",
    );
    let mut positions = 0;
    for fault in [
        Fault::StopAfterWrite as fn(u64) -> Fault,
        Fault::StopAfterSync,
    ] {
        for n in 1.. {
            let attempt = image.crash(CrashMode::KeepWrites);
            attempt.arm(fault(n));
            drop(Database::open_with(
                Arc::new(attempt.clone()),
                Path::new(DB),
                options(),
            ));
            if !attempt.stopped() {
                break;
            }
            positions += 1;
            for mode in [CrashMode::KeepWrites, CrashMode::SyncedOnly] {
                let label = format!("{:?} during replay, {mode:?}", fault(n));
                let reopened = attempt.crash(mode);
                let mut db = Database::open_with(Arc::new(reopened), Path::new(DB), options())
                    .unwrap_or_else(|e| panic!("{label}: {e}"));
                assert_eq!(dump(&mut db, &w.tables), reference, "{label}");
                db.check().unwrap_or_else(|e| panic!("{label}: {e}"));
            }
        }
    }
    assert!(
        positions > 2,
        "replay performed {positions} writes and syncs"
    );
}
