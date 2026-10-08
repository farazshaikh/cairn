//! An in-memory, fault-injecting [`Vfs`] for crash tests.
//!
//! [`FaultVfs`] keeps every file in memory twice: the *current* bytes (what
//! the process has written) and the *durable* bytes (what the last `sync` of
//! that file made permanent). It counts operations across all of its files:
//! `write_at` and `set_len` are writes, `sync` is a sync, numbered from 1.
//!
//! Once armed with a [`Fault`], it stops at the chosen operation. After a
//! stop every write, sync and file creation fails with "injected crash",
//! while reads keep working. [`FaultVfs::crash`] then builds the files a
//! restarted process would find:
//!
//! - [`CrashMode::KeepWrites`]: every write that was applied, including a
//!   torn prefix (the process died, the operating system survived);
//! - [`CrashMode::SyncedOnly`]: only what each file had synced (power loss).
//!
//! Unsynced writes are never reordered, and directory entries never vanish.
//! The double is part of the library, not `cfg(test)`, so integration tests
//! in other crates can use it; production code has no reason to.

use std::collections::HashMap;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use crate::vfs::{Vfs, VfsFile};

/// Where to stop. Positions are 1-based counts of writes or syncs.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Fault {
    /// Write N succeeds, then the file layer stops.
    StopAfterWrite(u64),
    /// Sync N succeeds, then the file layer stops.
    StopAfterSync(u64),
    /// Write N applies only a prefix of its bytes, chosen from `keep_seed`
    /// (at least 1 byte, never all of them), fails, and the layer stops. A
    /// `set_len` or a one-byte write cannot be torn; it is applied whole.
    TearWrite {
        /// The 1-based write to tear.
        write: u64,
        /// Chooses how many bytes are applied.
        keep_seed: u64,
    },
}

/// Which bytes survive [`FaultVfs::crash`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CrashMode {
    /// Every applied write survives (process crash).
    KeepWrites,
    /// Only synced bytes survive (power loss).
    SyncedOnly,
}

/// One applied operation, in order, for ordering assertions.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Event {
    /// The file the operation applied to.
    pub path: PathBuf,
    /// What was done.
    pub op: Op,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
/// A file operation recorded by [`FaultVfs`].
pub enum Op {
    /// Bytes were written.
    Write {
        /// Where the write started.
        offset: u64,
        /// How many bytes were applied.
        len: usize,
    },
    /// The file was truncated or extended to this length.
    SetLen(u64),
    /// The file was synced.
    Sync,
}

/// The fault-injecting file layer. Clones share the same files and counters.
#[derive(Clone)]
pub struct FaultVfs {
    id: u64,
    state: Arc<Mutex<FaultState>>,
}

#[derive(Default)]
struct FaultState {
    files: HashMap<PathBuf, SimFile>,
    writes: u64,
    syncs: u64,
    fault: Option<Fault>,
    stopped: bool,
    events: Vec<Event>,
}

#[derive(Clone, Default)]
struct SimFile {
    current: Vec<u8>,
    durable: Vec<u8>,
    pending: Vec<Pending>,
}

#[derive(Clone)]
enum Pending {
    Write { offset: u64, data: Vec<u8> },
    SetLen(u64),
}

struct FaultFile {
    path: PathBuf,
    state: Arc<Mutex<FaultState>>,
}

fn next_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

fn crashed() -> io::Error {
    io::Error::other("injected crash")
}

fn lock(state: &Mutex<FaultState>) -> MutexGuard<'_, FaultState> {
    state.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Default for FaultVfs {
    fn default() -> FaultVfs {
        FaultVfs::new()
    }
}

impl FaultVfs {
    /// An empty file system with a fresh registry identity and no fault.
    pub fn new() -> FaultVfs {
        FaultVfs {
            id: next_id(),
            state: Arc::new(Mutex::new(FaultState::default())),
        }
    }

    /// Arms a fault; replaces any previous one. Counters keep running.
    pub fn arm(&self, fault: Fault) {
        lock(&self.state).fault = Some(fault);
    }

    /// Writes (including `set_len`) applied or attempted so far.
    pub fn writes(&self) -> u64 {
        lock(&self.state).writes
    }

    /// Syncs applied or attempted so far.
    pub fn syncs(&self) -> u64 {
        lock(&self.state).syncs
    }

    /// Whether the armed fault has fired.
    pub fn stopped(&self) -> bool {
        lock(&self.state).stopped
    }

    /// Every applied operation so far, in order.
    pub fn events(&self) -> Vec<Event> {
        lock(&self.state).events.clone()
    }

    /// The files a restarted process would see, as a new instance with a
    /// new identity, zeroed counters and no fault.
    pub fn crash(&self, mode: CrashMode) -> FaultVfs {
        let state = lock(&self.state);
        let files = state
            .files
            .iter()
            .map(|(path, file)| {
                let image = match mode {
                    CrashMode::KeepWrites => file.current.clone(),
                    CrashMode::SyncedOnly => file.durable.clone(),
                };
                let sim = SimFile {
                    current: image.clone(),
                    durable: image,
                    pending: Vec::new(),
                };
                (path.clone(), sim)
            })
            .collect();
        FaultVfs {
            id: next_id(),
            state: Arc::new(Mutex::new(FaultState {
                files,
                ..FaultState::default()
            })),
        }
    }

    /// The current bytes of a file, if it exists.
    pub fn read_file(&self, path: &Path) -> Option<Vec<u8>> {
        lock(&self.state).files.get(path).map(|f| f.current.clone())
    }

    /// Test setup: replaces a file's bytes, durably, without counting.
    pub fn write_file(&self, path: &Path, bytes: Vec<u8>) {
        let sim = SimFile {
            current: bytes.clone(),
            durable: bytes,
            pending: Vec::new(),
        };
        lock(&self.state).files.insert(path.to_path_buf(), sim);
    }

    fn file(&self, path: &Path) -> Box<dyn VfsFile> {
        Box::new(FaultFile {
            path: path.to_path_buf(),
            state: Arc::clone(&self.state),
        })
    }
}

impl Vfs for FaultVfs {
    fn id(&self) -> u64 {
        self.id
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        let mut state = lock(&self.state);
        if state.stopped {
            return Err(crashed());
        }
        if state.files.contains_key(path) {
            return Err(io::Error::from(io::ErrorKind::AlreadyExists));
        }
        state.files.insert(path.to_path_buf(), SimFile::default());
        Ok(self.file(path))
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        if !lock(&self.state).files.contains_key(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(self.file(path))
    }

    fn open_or_create(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        if lock(&self.state).files.contains_key(path) {
            return Ok(self.file(path));
        }
        self.create_new(path)
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        Ok(lock(&self.state).files.contains_key(path))
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        if !lock(&self.state).files.contains_key(path) {
            return Err(io::Error::from(io::ErrorKind::NotFound));
        }
        Ok(path.to_path_buf())
    }
}

impl FaultState {
    /// Counts a write and decides how much of it to apply: `Ok(None)` means
    /// all of it, `Ok(Some(n))` means only the first `n` bytes and then fail.
    fn admit_write(&mut self, len: usize) -> io::Result<Option<usize>> {
        if self.stopped {
            return Err(crashed());
        }
        self.writes += 1;
        match self.fault {
            Some(Fault::StopAfterWrite(n)) if n == self.writes => {
                self.stopped = true;
                Ok(None)
            }
            Some(Fault::TearWrite { write, keep_seed }) if write == self.writes => {
                self.stopped = true;
                if len < 2 {
                    return Ok(None);
                }
                let keep = 1 + (keep_seed % (len as u64 - 1)) as usize;
                Ok(Some(keep))
            }
            _ => Ok(None),
        }
    }

    fn sim(&mut self, path: &Path) -> io::Result<&mut SimFile> {
        self.files
            .get_mut(path)
            .ok_or_else(|| io::Error::from(io::ErrorKind::NotFound))
    }
}

fn apply_write(bytes: &mut Vec<u8>, offset: u64, data: &[u8]) {
    let start = offset as usize;
    let end = start + data.len();
    if bytes.len() < end {
        bytes.resize(end, 0);
    }
    if let Some(target) = bytes.get_mut(start..end) {
        target.copy_from_slice(data);
    }
}

impl VfsFile for FaultFile {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        let mut state = lock(&self.state);
        let file = state.sim(&self.path)?;
        let start = (offset as usize).min(file.current.len());
        let available = file.current.get(start..).unwrap_or(&[]);
        let n = available.len().min(buf.len());
        if let (Some(dst), Some(src)) = (buf.get_mut(..n), available.get(..n)) {
            dst.copy_from_slice(src);
        }
        Ok(n)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        let mut state = lock(&self.state);
        let torn = state.admit_write(data.len())?;
        let applied = data.get(..torn.unwrap_or(data.len())).unwrap_or(data);
        let file = state.sim(&self.path)?;
        apply_write(&mut file.current, offset, applied);
        file.pending.push(Pending::Write {
            offset,
            data: applied.to_vec(),
        });
        state.events.push(Event {
            path: self.path.clone(),
            op: Op::Write {
                offset,
                len: applied.len(),
            },
        });
        match torn {
            Some(_) => Err(crashed()),
            None => Ok(()),
        }
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        let mut state = lock(&self.state);
        state.admit_write(0)?;
        let file = state.sim(&self.path)?;
        file.current.resize(len as usize, 0);
        file.pending.push(Pending::SetLen(len));
        state.events.push(Event {
            path: self.path.clone(),
            op: Op::SetLen(len),
        });
        Ok(())
    }

    fn size(&mut self) -> io::Result<u64> {
        let mut state = lock(&self.state);
        Ok(state.sim(&self.path)?.current.len() as u64)
    }

    fn sync(&mut self) -> io::Result<()> {
        let mut state = lock(&self.state);
        if state.stopped {
            return Err(crashed());
        }
        state.syncs += 1;
        let file = state.sim(&self.path)?;
        for pending in std::mem::take(&mut file.pending) {
            match pending {
                Pending::Write { offset, data } => apply_write(&mut file.durable, offset, &data),
                Pending::SetLen(len) => file.durable.resize(len as usize, 0),
            }
        }
        state.events.push(Event {
            path: self.path.clone(),
            op: Op::Sync,
        });
        if state.fault == Some(Fault::StopAfterSync(state.syncs)) {
            state.stopped = true;
        }
        Ok(())
    }
    /// The in-memory layer has no other processes, so the lock is always
    /// granted. It is not recorded as an event and never fails.
    fn try_lock(&mut self) -> io::Result<bool> {
        Ok(true)
    }
}
