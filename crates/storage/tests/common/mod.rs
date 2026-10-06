//! Std-only helpers shared by the unit and integration tests.

#![allow(dead_code)]

use std::fs::OpenOptions;
use std::io::{Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

/// Fixed seed for every shuffled workload, so runs are reproducible.
pub const SEED: u64 = 0x9E37_79B9_7F4A_7C15;

/// A unique path under `std::env::temp_dir()`, removed when dropped (also
/// while unwinding from a failed assertion). The file itself is created by
/// the test, normally through `Pager::create`, which refuses existing paths.
pub struct TempPath {
    path: PathBuf,
}

impl TempPath {
    pub fn new(tag: &str) -> TempPath {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::Relaxed);
        let nanos = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let name = format!("cairn-{tag}-{}-{nanos}-{n}.db", std::process::id());
        TempPath {
            path: std::env::temp_dir().join(name),
        }
    }

    pub fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempPath {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

/// Marsaglia's xorshift64 with the (13, 7, 17) shift triple.
pub struct XorShift64 {
    state: u64,
}

impl XorShift64 {
    pub fn new(seed: u64) -> XorShift64 {
        assert_ne!(seed, 0, "xorshift seed must be non-zero");
        XorShift64 { state: seed }
    }

    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.state;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.state = x;
        x
    }

    /// A value in `0..n`.
    pub fn below(&mut self, n: usize) -> usize {
        (self.next_u64() % n as u64) as usize
    }
}

/// Fisher-Yates shuffle driven by `rng`.
pub fn shuffle<T>(items: &mut [T], rng: &mut XorShift64) {
    for i in (1..items.len()).rev() {
        items.swap(i, rng.below(i + 1));
    }
}

pub fn read_raw(path: &Path) -> Vec<u8> {
    std::fs::read(path).expect("read raw file")
}

/// Overwrites bytes of an existing file in place.
pub fn write_raw(path: &Path, offset: u64, bytes: &[u8]) {
    let mut file = OpenOptions::new()
        .write(true)
        .open(path)
        .expect("open raw file");
    file.seek(SeekFrom::Start(offset)).expect("seek");
    file.write_all(bytes).expect("write raw bytes");
}
