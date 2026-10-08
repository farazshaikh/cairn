//! Rendering of statement results in the golden-test format, shared by the
//! golden runner and the README example test, and test-file cleanup.

#![allow(dead_code)]

use std::path::Path;

use cairn_exec::{ExecError, QueryResult};

/// Removes a database file and its write-ahead log, ignoring missing files.
pub fn remove_database(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let _ = std::fs::remove_file(wal);
}

/// Renders the outcome of `Database::execute_each` with the public renderer.
pub fn render(outcome: &Result<Vec<Result<QueryResult, ExecError>>, ExecError>) -> String {
    cairn_exec::render::render_outcome(outcome)
}
