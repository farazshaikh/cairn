//! Helpers for running the built `cairn` binary in tests.

#![allow(dead_code)]

use std::ffi::OsStr;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};

/// The binary under test, built by Cargo for integration tests.
pub const CAIRN: &str = env!("CARGO_BIN_EXE_cairn");

/// The golden SQL scripts at the repository root.
pub fn sql_dir() -> PathBuf {
    PathBuf::from(concat!(env!("CARGO_MANIFEST_DIR"), "/../../tests/sql"))
}

/// A unique database path, with the file and its log removed on drop.
pub struct TempDb {
    path: PathBuf,
}

impl TempDb {
    pub fn new(tag: &str) -> TempDb {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path =
            std::env::temp_dir().join(format!("cairn-cli-{tag}-{}-{n}.db", std::process::id()));
        remove_database(&path);
        TempDb { path }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn arg(&self) -> &OsStr {
        self.path.as_os_str()
    }

    pub fn wal(&self) -> PathBuf {
        let mut name = self.path.as_os_str().to_owned();
        name.push("-wal");
        PathBuf::from(name)
    }
}

impl Drop for TempDb {
    fn drop(&mut self) {
        remove_database(&self.path);
    }
}

pub fn remove_database(path: &Path) {
    let _ = std::fs::remove_file(path);
    let mut wal = path.as_os_str().to_owned();
    wal.push("-wal");
    let _ = std::fs::remove_file(wal);
}

/// Runs `cairn ARGS` with `stdin` piped in and waits for it.
pub fn cairn(args: &[&OsStr], stdin: &str) -> Output {
    let mut child = Command::new(CAIRN)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cairn");
    let mut input = child.stdin.take().expect("stdin");
    input.write_all(stdin.as_bytes()).expect("write stdin");
    drop(input);
    child.wait_with_output().expect("wait for cairn")
}

/// Shorthand for an `OsStr` argument.
pub fn arg(text: &str) -> &OsStr {
    OsStr::new(text)
}

pub fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).expect("stdout is UTF-8")
}

pub fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).expect("stderr is UTF-8")
}

pub fn code(output: &Output) -> i32 {
    output.status.code().expect("exit code")
}
