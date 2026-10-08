//! The fuzz runner itself (milestone M6 AC1): deterministic seeds,
//! environment overrides, single-case replay, and the report for a failed
//! property, a panic and a timeout.

use std::collections::HashMap;
use std::time::Duration;

use cairn_fuzz::rng::{Rng, case_seed};
use cairn_fuzz::runner::{Budget, CaseResult, Cause, Config, run_cases};

const DEFAULTS: Budget = Budget {
    seed: 0xC0FF_EE00_0000_0001,
    cases: 25,
    timeout: Duration::from_secs(5),
};

fn lookup(vars: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
    let map: HashMap<String, String> = vars
        .iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
    move |name| map.get(name).cloned()
}

fn config(vars: &[(&str, &str)]) -> Result<Config, String> {
    Config::from_lookup(DEFAULTS, lookup(vars))
}

#[test]
fn the_default_seed_gives_the_same_sequence_every_run() {
    let first = config(&[]).expect("defaults");
    let second = config(&[]).expect("defaults");
    assert_eq!(first, second);
    let sequence = |seed| {
        let mut rng = Rng::new(case_seed(seed, 0));
        (0..1000).map(|_| rng.next_u64()).collect::<Vec<_>>()
    };
    assert_eq!(sequence(first.seed), sequence(second.seed));
    assert_ne!(sequence(first.seed), sequence(first.seed + 1));
}

#[test]
fn case_seeds_are_never_zero_and_differ_by_case() {
    let mut seen = std::collections::HashSet::new();
    for case in 0..100_000 {
        let seed = case_seed(DEFAULTS.seed, case);
        assert_ne!(seed, 0);
        assert!(seen.insert(seed), "case {case} repeats a seed");
    }
}

#[test]
fn environment_variables_override_the_defaults() {
    let c = config(&[
        ("CAIRN_FUZZ_SEED", "0x1F"),
        ("CAIRN_FUZZ_CASES", "7"),
        ("CAIRN_FUZZ_CASE", "3"),
    ])
    .expect("valid");
    assert_eq!((c.seed, c.cases, c.only_case), (31, 7, Some(3)));
    assert_eq!(
        config(&[("CAIRN_FUZZ_SEED", "12")]).expect("decimal").seed,
        12
    );
    let d = config(&[]).expect("defaults");
    assert_eq!((d.seed, d.cases, d.only_case), (DEFAULTS.seed, 25, None));
}

#[test]
fn invalid_values_are_rejected_with_a_clear_message() {
    assert_eq!(
        config(&[("CAIRN_FUZZ_SEED", "0")]),
        Err("CAIRN_FUZZ_SEED: expected a non-zero u64, got \"0\"".to_string())
    );
    assert_eq!(
        config(&[("CAIRN_FUZZ_SEED", "abc")]),
        Err("CAIRN_FUZZ_SEED: expected a non-zero u64, got \"abc\"".to_string())
    );
    assert_eq!(
        config(&[("CAIRN_FUZZ_CASES", "0")]),
        Err("CAIRN_FUZZ_CASES: expected a positive integer, got \"0\"".to_string())
    );
    assert_eq!(
        config(&[("CAIRN_FUZZ_CASES", "-3")]),
        Err("CAIRN_FUZZ_CASES: expected a positive integer, got \"-3\"".to_string())
    );
    assert!(config(&[("CAIRN_FUZZ_CASE", "x")]).is_err());
}

#[test]
fn a_failed_property_is_reported_with_a_replay_command_and_reproducer() {
    let c = config(&[]).expect("defaults");
    let failure = run_cases("demo", &c, |index, _| {
        if index == 4 {
            CaseResult::Fail {
                reproducer: "SELECT 1;\n".into(),
                detail: "cairn: 1, reference: 2".into(),
            }
        } else {
            CaseResult::Pass
        }
    })
    .expect_err("case 4 fails");
    assert_eq!(failure.case, 4);
    assert_eq!(
        failure.report(),
        format!(
            "fuzz failure: target=demo seed=0x{:016x} case=4 (mismatch)\n\
             replay: CAIRN_FUZZ_SEED=0x{:016x} CAIRN_FUZZ_CASE=4 cargo test -p cairn-fuzz --test demo\n\
             cairn: 1, reference: 2\n--- reproducer ---\nSELECT 1;\n",
            c.seed, c.seed
        )
    );
}

#[test]
fn replaying_one_case_gives_it_the_same_generator() {
    let c = config(&[]).expect("defaults");
    let all = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&all);
    run_cases("demo", &c, move |index, mut rng| {
        sink.lock().expect("sink").push((index, rng.next_u64()));
        CaseResult::Pass
    })
    .expect("passes");
    let replay = config(&[("CAIRN_FUZZ_CASE", "9")]).expect("valid");
    let one = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
    let sink = std::sync::Arc::clone(&one);
    let summary = run_cases("demo", &replay, move |index, mut rng| {
        sink.lock().expect("sink").push((index, rng.next_u64()));
        CaseResult::Pass
    })
    .expect("passes");
    assert_eq!(summary.cases, 1);
    let all = all.lock().expect("all");
    assert_eq!(one.lock().expect("one").as_slice(), &all[9..10]);
}

#[test]
fn panics_and_timeouts_are_reported_not_fatal() {
    let mut c = config(&[]).expect("defaults");
    let panicked = run_cases("demo", &c, |index, _| {
        assert!(index != 2, "boom in case {index}");
        CaseResult::Pass
    })
    .expect_err("case 2 panics");
    assert_eq!(panicked.case, 2);
    assert!(
        matches!(&panicked.cause, Cause::Panic(m) if m.contains("boom in case 2")),
        "{:?}",
        panicked.cause
    );
    assert!(panicked.report().contains("case=2 (panic)"));

    c.timeout = Duration::from_millis(100);
    let slow = run_cases("demo", &c, |index, _| {
        if index == 1 {
            std::thread::sleep(Duration::from_secs(2));
        }
        CaseResult::Pass
    })
    .expect_err("case 1 times out");
    assert_eq!(slow.case, 1);
    assert_eq!(slow.cause, Cause::Timeout(Duration::from_millis(100)));
    assert!(slow.report().contains("case=1 (timeout after 0.1s)"));
}
