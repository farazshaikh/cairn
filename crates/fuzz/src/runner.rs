//! The fuzz runner: seeds and case budgets from the environment, every
//! case in its own thread with a timeout, and one report on the first
//! failure.
//!
//! - `CAIRN_FUZZ_SEED`: the run seed, decimal or `0x` hex, non-zero;
//! - `CAIRN_FUZZ_CASES`: the number of cases, positive;
//! - `CAIRN_FUZZ_CASE`: run only this case index (replay).
//!
//! Case `i` draws from [`crate::rng::case_seed`]`(seed, i)`, so it replays
//! alone. Cases run on threads with a 16 MiB stack (deep expressions in
//! debug builds) and a time limit, so a panic or a hang is reported with
//! the seed instead of aborting the run.

use std::sync::Arc;
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use crate::rng::{Rng, case_seed};

/// The stack of each case thread.
pub const CASE_STACK: usize = 16 << 20;

/// A target's defaults, used when the environment does not override them.
#[derive(Debug, Clone, Copy)]
pub struct Budget {
    /// The default run seed.
    pub seed: u64,
    /// The default number of cases.
    pub cases: u32,
    /// The time limit for one case.
    pub timeout: Duration,
}

/// The settings of one run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Config {
    /// The run seed.
    pub seed: u64,
    /// How many cases to run.
    pub cases: u32,
    /// Run only this case, for replay.
    pub only_case: Option<u32>,
    /// The time limit for one case.
    pub timeout: Duration,
}

impl Config {
    /// Reads the process environment over `defaults`.
    pub fn from_env(defaults: Budget) -> Result<Config, String> {
        Config::from_lookup(defaults, |name| std::env::var(name).ok())
    }

    /// Like [`Config::from_env`] with an injected variable lookup, so tests
    /// never change the process environment.
    pub fn from_lookup(
        defaults: Budget,
        lookup: impl Fn(&str) -> Option<String>,
    ) -> Result<Config, String> {
        let seed = match lookup("CAIRN_FUZZ_SEED") {
            Some(text) => parse_u64(&text)
                .filter(|s| *s != 0)
                .ok_or_else(|| format!("CAIRN_FUZZ_SEED: expected a non-zero u64, got {text:?}"))?,
            None => defaults.seed,
        };
        let cases = match lookup("CAIRN_FUZZ_CASES") {
            Some(text) => text
                .trim()
                .parse::<u32>()
                .ok()
                .filter(|n| *n > 0)
                .ok_or_else(|| {
                    format!("CAIRN_FUZZ_CASES: expected a positive integer, got {text:?}")
                })?,
            None => defaults.cases,
        };
        let only_case =
            match lookup("CAIRN_FUZZ_CASE") {
                Some(text) => Some(text.trim().parse::<u32>().map_err(|_| {
                    format!("CAIRN_FUZZ_CASE: expected a case index, got {text:?}")
                })?),
                None => None,
            };
        Ok(Config {
            seed,
            cases,
            only_case,
            timeout: defaults.timeout,
        })
    }
}

fn parse_u64(text: &str) -> Option<u64> {
    let text = text.trim();
    match text.strip_prefix("0x").or_else(|| text.strip_prefix("0X")) {
        Some(hex) => u64::from_str_radix(hex, 16).ok(),
        None => text.parse().ok(),
    }
}

/// What one case found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaseResult {
    /// The property held.
    Pass,
    /// The property failed; `reproducer` replays it.
    Fail {
        /// Input that reproduces the failure, usually a minimized script.
        reproducer: String,
        /// What went wrong.
        detail: String,
    },
}

/// Why a case failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Cause {
    /// The case thread panicked.
    Panic(String),
    /// The case ran past the time limit.
    Timeout(Duration),
    /// The case reported a failed property.
    Mismatch {
        /// Input that reproduces the failure.
        reproducer: String,
        /// What went wrong.
        detail: String,
    },
}

/// The first failing case of a run.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The test target, as in `cargo test --test <target>`.
    pub target: String,
    /// The run seed.
    pub seed: u64,
    /// The failing case index.
    pub case: u32,
    /// What happened.
    pub cause: Cause,
}

impl Failure {
    /// The report printed for a failure: a summary line, a replay command,
    /// the detail and the reproducer.
    pub fn report(&self) -> String {
        let (what, detail, reproducer) = match &self.cause {
            Cause::Panic(message) => ("panic".to_string(), message.clone(), String::new()),
            Cause::Timeout(limit) => (
                format!("timeout after {}s", limit.as_secs_f64()),
                String::new(),
                String::new(),
            ),
            Cause::Mismatch { reproducer, detail } => {
                ("mismatch".to_string(), detail.clone(), reproducer.clone())
            }
        };
        format!(
            "fuzz failure: target={} seed=0x{:016x} case={} ({what})\n\
             replay: CAIRN_FUZZ_SEED=0x{:016x} CAIRN_FUZZ_CASE={} cargo test -p cairn-fuzz --test {}\n\
             {detail}\n--- reproducer ---\n{reproducer}",
            self.target, self.seed, self.case, self.seed, self.case, self.target
        )
    }
}

/// How a run went.
#[derive(Debug, Clone)]
pub struct Summary {
    /// Cases that ran.
    pub cases: u32,
    /// Wall time of the run.
    pub elapsed: Duration,
}

/// Runs the cases of `config` and stops at the first failure.
pub fn run_cases<F>(target: &str, config: &Config, case: F) -> Result<Summary, Failure>
where
    F: Fn(u32, Rng) -> CaseResult + Send + Sync + 'static,
{
    let case = Arc::new(case);
    let started = Instant::now();
    let indexes: Vec<u32> = match config.only_case {
        Some(index) => vec![index],
        None => (0..config.cases).collect(),
    };
    for &index in &indexes {
        let failure = |cause| Failure {
            target: target.to_string(),
            seed: config.seed,
            case: index,
            cause,
        };
        match run_one(target, config, index, Arc::clone(&case)) {
            Ok(CaseResult::Pass) => {}
            Ok(CaseResult::Fail { reproducer, detail }) => {
                return Err(failure(Cause::Mismatch { reproducer, detail }));
            }
            Err(cause) => return Err(failure(cause)),
        }
    }
    Ok(Summary {
        cases: indexes.len() as u32,
        elapsed: started.elapsed(),
    })
}

fn run_one<F>(target: &str, config: &Config, index: u32, case: Arc<F>) -> Result<CaseResult, Cause>
where
    F: Fn(u32, Rng) -> CaseResult + Send + Sync + 'static,
{
    let (sender, receiver) = mpsc::channel();
    let rng = Rng::new(case_seed(config.seed, index));
    let handle = thread::Builder::new()
        .name(format!("fuzz-{target}-{index}"))
        .stack_size(CASE_STACK)
        .spawn(move || {
            let result = case(index, rng);
            let _ = sender.send(result);
        })
        .map_err(|e| Cause::Panic(format!("cannot start the case thread: {e}")))?;
    match receiver.recv_timeout(config.timeout) {
        Ok(result) => {
            let _ = handle.join();
            Ok(result)
        }
        Err(mpsc::RecvTimeoutError::Timeout) => Err(Cause::Timeout(config.timeout)),
        Err(mpsc::RecvTimeoutError::Disconnected) => {
            let message = match handle.join() {
                Err(payload) => payload
                    .downcast_ref::<&str>()
                    .map(|s| (*s).to_string())
                    .or_else(|| payload.downcast_ref::<String>().cloned())
                    .unwrap_or_else(|| "non-string panic payload".to_string()),
                Ok(()) => "the case ended without a result".to_string(),
            };
            Err(Cause::Panic(message))
        }
    }
}

/// Runs a target under `cargo test`: reads the environment, runs, prints
/// a summary line, and panics with the report on the first failure.
///
/// # Panics
///
/// On an invalid environment variable or a failing case.
pub fn run<F>(target: &str, defaults: Budget, case: F) -> Summary
where
    F: Fn(u32, Rng) -> CaseResult + Send + Sync + 'static,
{
    let config = match Config::from_env(defaults) {
        Ok(config) => config,
        Err(problem) => panic!("{problem}"),
    };
    match run_cases(target, &config, case) {
        Ok(summary) => {
            println!(
                "fuzz {target}: {} cases, seed 0x{:016x}, {:.2}s",
                summary.cases,
                config.seed,
                summary.elapsed.as_secs_f64()
            );
            summary
        }
        Err(failure) => {
            let report = failure.report();
            eprintln!("{report}");
            panic!("{report}");
        }
    }
}
