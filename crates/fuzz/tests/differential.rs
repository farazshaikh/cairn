//! Differential fuzzing: cairn against the independent reference evaluator
//! (milestone M6 AC4).
//!
//! Each case is a generated schema, data and 5 to 25 statements, run
//! through `Database::execute_each` on an in-memory file and through
//! `cairn_reference::Engine`. Every statement must agree under the rules in
//! `cairn_fuzz::compare`. A disagreement is shrunk to the fewest statements
//! that still disagree and reported with a replay command.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use cairn_fuzz::differential::{Observed, Oracle, run_case, shrink};
use cairn_fuzz::generate::{Coverage, case};
use cairn_fuzz::minimize::{Step, from_script};
use cairn_fuzz::rng::Rng;
use cairn_fuzz::runner::{Budget, CaseResult, Cause, Config, run, run_cases};
use cairn_reference::{Engine, Outcome, RErrorKind};

const BUDGET: Budget = Budget {
    seed: 0xD1FF_E2E7_1A15_0001,
    cases: 500,
    timeout: Duration::from_secs(30),
};

#[derive(Default)]
struct Totals {
    cases: u32,
    coverage: Coverage,
    observed: Observed,
}

/// Runs one generated case against `oracle`, shrinking any disagreement.
fn check_case<O: Oracle>(
    rng: &mut Rng,
    oracle: impl Fn() -> O,
    totals: &Mutex<Totals>,
) -> CaseResult {
    let (steps, coverage) = case(rng);
    match run_case(&steps, oracle()) {
        Ok(seen) => {
            if let Ok(mut t) = totals.lock() {
                t.cases += 1;
                t.coverage.merge(&coverage);
                let o = &mut t.observed;
                o.statements += seen.statements;
                o.errors += seen.errors;
                o.constraint_errors += seen.constraint_errors;
                o.indexable += seen.indexable;
                o.index_used += seen.index_used;
            }
            CaseResult::Pass
        }
        Err(_) => {
            let (reproducer, detail) = shrink(&steps, oracle);
            CaseResult::Fail { reproducer, detail }
        }
    }
}

/// Features every default run must exercise in at least 1% of cases.
const FEATURES: [&str; 31] = [
    "select",
    "where",
    "inner_join",
    "left_join",
    "group_by",
    "having",
    "aggregate",
    "count",
    "sum",
    "avg",
    "min_max",
    "distinct",
    "order_by",
    "limit",
    "case",
    "like",
    "function",
    "division",
    "insert",
    "update",
    "delete",
    "begin",
    "commit",
    "rollback",
    "txn_misuse",
    "reopen",
    "create_index",
    "constraint_table",
    "type_error",
    "drop_table",
    "R-SUM",
];

/// Generator rules that must fire at least once.
const RULES: [&str; 5] = ["R-LIMIT", "R-SUM", "R-REAL", "R-ERR", "R-SIZE"];

#[test]
fn cairn_agrees_with_the_reference_evaluator() {
    let totals = Arc::new(Mutex::new(Totals::default()));
    let shared = Arc::clone(&totals);
    let summary = run("differential", BUDGET, move |_, mut rng| {
        check_case(&mut rng, Engine::new, &shared)
    });
    let t = totals.lock().expect("totals");
    let mut report = format!(
        "{} cases, {} statements ({} errors, {} constraint errors), index used for {} of {} indexable SELECTs\n",
        t.cases,
        t.observed.statements,
        t.observed.errors,
        t.observed.constraint_errors,
        t.observed.index_used,
        t.observed.indexable
    );
    for (name, count) in &t.coverage.counts {
        report.push_str(&format!("  {name}: {count}\n"));
    }
    println!("{report}");
    if summary.cases < 100 {
        return;
    }
    let floor = (summary.cases / 100).max(1);
    for feature in FEATURES {
        assert!(
            t.coverage.get(feature) >= floor,
            "feature {feature} in fewer than 1% of cases\n{report}"
        );
    }
    for rule in RULES {
        assert!(
            t.coverage.get(rule) >= 1,
            "rule {rule} never fired\n{report}"
        );
    }
    assert!(
        t.observed.constraint_errors >= floor,
        "too few constraint errors\n{report}"
    );
    assert!(
        t.observed.index_used * 4 >= t.observed.indexable && t.observed.indexable > 0,
        "fewer than 25% of indexable SELECTs used an index\n{report}"
    );
}

/// A reference that drops the last row of every result with two or more
/// rows: a divergence the harness must catch and shrink.
struct DropsLastRow(Engine);

impl Oracle for DropsLastRow {
    fn run(&mut self, sql: &str) -> Result<Vec<Outcome>, RErrorKind> {
        let mut outcomes = self.0.run_script(sql)?;
        for outcome in &mut outcomes {
            if let Outcome::Rows { rows, .. } = outcome
                && rows.len() >= 2
            {
                rows.pop();
            }
        }
        Ok(outcomes)
    }
}

#[test]
fn an_injected_divergence_is_caught_and_shrunk() {
    let config = Config {
        seed: BUDGET.seed,
        cases: 2000,
        only_case: None,
        timeout: BUDGET.timeout,
    };
    let totals = Arc::new(Mutex::new(Totals::default()));
    let failure = run_cases("differential", &config, move |_, mut rng| {
        check_case(&mut rng, || DropsLastRow(Engine::new()), &totals)
    })
    .expect_err("the broken reference must be caught");
    let Cause::Mismatch { reproducer, detail } = &failure.cause else {
        panic!("expected a mismatch, got {:?}", failure.cause);
    };
    let steps = from_script(reproducer);
    let statements = steps.iter().filter(|s| matches!(s, Step::Sql(_))).count();
    assert!(
        statements <= 5,
        "reproducer has {statements} statements:\n{reproducer}"
    );
    assert!(detail.contains("cairn: Rows"), "{detail}");
    let report = failure.report();
    assert!(report.contains("replay: CAIRN_FUZZ_SEED=0x"), "{report}");
    assert!(report.contains(reproducer.as_str()), "{report}");
}
