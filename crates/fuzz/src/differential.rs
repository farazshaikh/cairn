//! Running one differential case on cairn and on a reference, and
//! shrinking it when they disagree.

use std::path::Path;
use std::sync::Arc;

use cairn_exec::{Database, QueryResult, Value};
use cairn_reference::{Engine, Outcome, RErrorKind};
use cairn_storage::Options;
use cairn_storage::fault::FaultVfs;

use crate::compare::{Norm, equal, from_cairn, from_reference};
use crate::generate::CaseStep;
use crate::minimize::{Step, minimize, to_script};

/// Something that runs SQL like the reference evaluator. The self-test
/// wraps the reference in a deliberately wrong one.
pub trait Oracle {
    /// Runs a script with `execute_each` semantics.
    fn run(&mut self, sql: &str) -> Result<Vec<Outcome>, RErrorKind>;
}

impl Oracle for Engine {
    fn run(&mut self, sql: &str) -> Result<Vec<Outcome>, RErrorKind> {
        self.run_script(sql)
    }
}

/// What a passing case observed, for the coverage assertions.
#[derive(Debug, Clone, Default)]
pub struct Observed {
    /// Statements that failed with a constraint error on both sides.
    pub constraint_errors: u32,
    /// Statements that failed on both sides.
    pub errors: u32,
    /// Statements that ran.
    pub statements: u32,
    /// Indexable SELECTs, and how many of them cairn served from an index.
    pub indexable: u32,
    /// Indexable SELECTs that cairn served from an index.
    pub index_used: u32,
}

/// The first disagreement in a case.
#[derive(Debug, Clone)]
pub struct Mismatch {
    /// The step that disagreed.
    pub step: usize,
    /// What each side produced.
    pub detail: String,
}

const PATH: &str = "/fuzz.db";

fn open(vfs: &Arc<FaultVfs>, create: bool) -> Result<Database, String> {
    let path = Path::new(PATH);
    let result = if create {
        Database::create_with(vfs.clone(), path, Options::default())
    } else {
        Database::open_with(vfs.clone(), path, Options::default())
    };
    result.map_err(|e| format!("cairn could not open the database: {e}"))
}

/// Runs `steps` on a fresh in-memory cairn database and a fresh oracle.
pub fn run_case<O: Oracle>(steps: &[CaseStep], mut oracle: O) -> Result<Observed, Mismatch> {
    let vfs = Arc::new(FaultVfs::new());
    let fail = |step, detail| Mismatch { step, detail };
    let mut db = Some(open(&vfs, true).map_err(|d| fail(0, d))?);
    let mut seen = Observed::default();
    for (index, case_step) in steps.iter().enumerate() {
        let sql = match &case_step.step {
            Step::Reopen => {
                if let Some(old) = db.take() {
                    old.close()
                        .map_err(|e| fail(index, format!("close failed: {e}")))?;
                }
                db = Some(open(&vfs, false).map_err(|d| fail(index, d))?);
                continue;
            }
            Step::Sql(sql) => sql,
        };
        let Some(database) = db.as_mut() else {
            return Err(fail(index, "no open database".into()));
        };
        if case_step.indexable {
            seen.indexable += 1;
            if served_by_index(database, sql) {
                seen.index_used += 1;
            }
        }
        let theirs = from_reference(&oracle.run(sql));
        let ours = from_cairn(&database.execute_each(sql));
        if ours.len() != theirs.len() {
            return Err(fail(
                index,
                format!("statement counts differ:\ncairn: {ours:?}\nreference: {theirs:?}"),
            ));
        }
        for (a, b) in ours.iter().zip(&theirs) {
            equal(a, b, case_step.order_keys.as_deref(), case_step.multi_error)
                .map_err(|d| fail(index, d))?;
            seen.statements += 1;
            if let Norm::Error(kind) = a {
                seen.errors += 1;
                if kind == "Constraint" {
                    seen.constraint_errors += 1;
                }
            }
        }
    }
    if let Some(database) = db {
        database
            .close()
            .map_err(|e| fail(steps.len(), format!("close failed: {e}")))?;
    }
    Ok(seen)
}

/// Whether cairn's plan for `select` reads its table through the primary
/// key or an index.
fn served_by_index(db: &mut Database, select: &str) -> bool {
    let Ok(results) = db.execute(&format!("EXPLAIN {select}")) else {
        return false;
    };
    let Some(QueryResult::Rows { rows, .. }) = results.last() else {
        return false;
    };
    rows.iter().any(|row| {
        matches!(row.get(2), Some(Value::Text(op))
            if op.starts_with("PRIMARY KEY") || op.starts_with("INDEX"))
    })
}

/// Shrinks a failing case to fewer steps that still disagree and returns
/// the reproducer script and the disagreement it shows.
pub fn shrink<O: Oracle>(steps: &[CaseStep], oracle: impl Fn() -> O) -> (String, String) {
    let kept = minimize(steps.to_vec(), |s| run_case(s, oracle()).is_err(), 2000);
    let detail = match run_case(&kept, oracle()) {
        Err(m) => format!("step {}: {}", m.step + 1, m.detail),
        Ok(_) => "the shrunk case passed; see the full case".to_string(),
    };
    let plain: Vec<Step> = kept.into_iter().map(|s| s.step).collect();
    (to_script(&plain), detail)
}
