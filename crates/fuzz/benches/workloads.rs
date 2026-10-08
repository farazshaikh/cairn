//! Std-only benchmarks of the `cairn_fuzz::workloads` (milestone M6 AC6).
//!
//! `cargo bench -p cairn-fuzz --bench workloads` runs each workload once to
//! warm up and then five timed runs at 20 000 rows. `--quick` (or
//! `CAIRN_BENCH_QUICK=1`, which also works for a workspace-wide
//! `cargo bench`) uses 2 000 rows and one timed run. Other arguments select
//! workloads by name. Times are reported, never judged: they depend on the
//! machine and its disk.

use std::time::Duration;

use cairn_fuzz::workloads::{Scale, WORKLOADS, remove_database};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let quick = args.iter().any(|a| a == "--quick")
        || std::env::var("CAIRN_BENCH_QUICK").is_ok_and(|v| v == "1");
    let names: Vec<&String> = args.iter().filter(|a| !a.starts_with("--")).collect();
    let (scale, runs) = if quick {
        (Scale::QUICK, 1)
    } else {
        (Scale::FULL, 5)
    };
    println!(
        "{:<18} {:>7} {:>5} {:>12} {:>12}  timed",
        "workload", "rows", "runs", "median ms", "min ms"
    );
    let mut failed = false;
    for workload in WORKLOADS {
        if !names.is_empty() && !names.iter().any(|n| *n == workload.name) {
            continue;
        }
        let path = std::env::temp_dir().join(format!(
            "cairn-bench-{}-{}.db",
            std::process::id(),
            workload.name
        ));
        let mut times: Vec<Duration> = Vec::new();
        let mut problem = None;
        for run in 0..=runs {
            match (workload.run)(&path, scale) {
                Ok(elapsed) if run > 0 => times.push(elapsed),
                Ok(_) => {}
                Err(e) => {
                    problem = Some(e);
                    break;
                }
            }
        }
        remove_database(&path);
        if let Some(problem) = problem {
            println!("{:<18} FAILED: {problem}", workload.name);
            failed = true;
            continue;
        }
        times.sort();
        let ms = |d: Duration| d.as_secs_f64() * 1000.0;
        let median = times.get(times.len() / 2).copied().unwrap_or_default();
        let min = times.first().copied().unwrap_or_default();
        println!(
            "{:<18} {:>7} {:>5} {:>12.3} {:>12.3}  {}",
            workload.name,
            scale.rows,
            times.len(),
            ms(median),
            ms(min),
            workload.timed
        );
    }
    if failed {
        std::process::exit(1);
    }
}
