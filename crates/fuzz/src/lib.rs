//! Deterministic, std-only fuzzing and benchmarks for cairn.
//!
//! - [`runner`]: seeds and case budgets from the environment, one thread
//!   per case with a time limit, and a replayable report on failure.
//! - [`minimize`]: shrinks a failing case and writes it as a script.
//! - [`generate`]: SQL generators for the robustness and differential targets.
//! - [`compare`] and [`differential`]: cairn against the independent
//!   reference evaluator in `cairn-reference`.
//! - [`mutate`]: seeded corruption of database and log files.
//! - [`workloads`]: the benchmark workloads, also smoke-tested.
//!
//! The targets themselves are the integration tests in `tests/`; the
//! README "Fuzzing" section explains how to run, replay and extend them.
#![warn(missing_docs)]

pub mod compare;
pub mod differential;
pub mod generate;
pub mod minimize;
pub mod mutate;
pub mod rng;
pub mod runner;
pub mod workloads;
