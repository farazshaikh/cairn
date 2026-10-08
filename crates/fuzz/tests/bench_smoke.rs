//! Every benchmark workload at a tiny scale, so `cargo test` keeps them
//! working (milestone M6 AC6). Each workload checks its own result.

use cairn_fuzz::workloads::{Scale, WORKLOADS, remove_database};

#[test]
fn every_workload_runs_and_checks_its_result() {
    let mut failures = Vec::new();
    for workload in WORKLOADS {
        let path = std::env::temp_dir().join(format!(
            "cairn-bench-smoke-{}-{}.db",
            std::process::id(),
            workload.name
        ));
        let result = (workload.run)(&path, Scale::TINY);
        remove_database(&path);
        if let Err(problem) = result {
            failures.push(format!("{}: {problem}", workload.name));
        }
    }
    assert!(failures.is_empty(), "{}", failures.join("\n"));
}

#[test]
fn workload_names_are_unique() {
    let mut names: Vec<&str> = WORKLOADS.iter().map(|w| w.name).collect();
    names.sort_unstable();
    names.dedup();
    assert_eq!(names.len(), WORKLOADS.len());
}
