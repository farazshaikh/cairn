//! State shared by every [`crate::Pager`] handle on one database file.
//!
//! A process-wide registry maps `(vfs id, canonical path)` to one
//! [`SharedFile`]. It owns the main file and the log, the published
//! [`Snapshot`] of committed state, the single writer lock, the count of
//! registered readers and the poison flag. Handles keep only their own
//! buffer pool and uncommitted transaction.
//!
//! The first handle to open a file in the process runs recovery
//! ([`SharedFile::open`]); later handles attach to the live state. The
//! entry disappears with the last handle.
//!
//! Ordering guarantee: the main file is written only by [`State::checkpoint`]
//! and by recovery, and both sync the log before their first main-file
//! write, so a page reaches the main file only after the commit record of
//! its transaction is durable.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError, Weak};
use std::time::{SystemTime, UNIX_EPOCH};

use crate::error::{Result, StorageError};
use crate::header::Header;
use crate::page::{PAGE_SIZE, Page, PageId};
use crate::vfs::{Vfs, VfsFile, wal_path};
use crate::wal::{
    self, COMMIT_SIZE, FRAME_SIZE, LOG_HEADER_SIZE, RECORD_HEADER_SIZE, TxnRecord, le_u32,
};

/// Offset of the page count inside the header page.
const HEADER_PAGE_COUNT_AT: usize = 16;

/// An immutable view of committed state: the header and where each page
/// changed since the last checkpoint lives in the log.
#[derive(Debug)]
pub(crate) struct Snapshot {
    pub(crate) seq: u64,
    pub(crate) header: Header,
    pub(crate) index: HashMap<PageId, u64>,
}

/// Position of the log tail. `end == 0` means the log is logically empty
/// and the next commit starts a new generation.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct WalState {
    pub(crate) salt: u64,
    pub(crate) end: u64,
    pub(crate) chain: u32,
    pub(crate) frames: u32,
    pub(crate) next_txn: u64,
}

pub(crate) struct State {
    pub(crate) main: Box<dyn VfsFile>,
    pub(crate) log: Option<Box<dyn VfsFile>>,
    log_path: PathBuf,
    vfs: Arc<dyn Vfs>,
    pub(crate) wal: WalState,
    last_salt: Option<u64>,
    pub(crate) published: Arc<Snapshot>,
    pub(crate) writer: Option<u64>,
    pub(crate) readers: usize,
    pub(crate) poisoned: bool,
}

pub(crate) struct SharedFile {
    state: Mutex<State>,
}

type Registry = Mutex<HashMap<(u64, PathBuf), Weak<SharedFile>>>;

fn registry() -> &'static Registry {
    static REGISTRY: OnceLock<Registry> = OnceLock::new();
    REGISTRY.get_or_init(|| Mutex::new(HashMap::new()))
}

fn lock_registry() -> MutexGuard<'static, HashMap<(u64, PathBuf), Weak<SharedFile>>> {
    let mut map = registry().lock().unwrap_or_else(PoisonError::into_inner);
    map.retain(|_, weak| weak.strong_count() > 0);
    map
}

/// Pages changed by one transaction, in ascending id order, plus the page
/// count it commits.
pub(crate) struct Commit<'a> {
    pub(crate) pages: &'a [(PageId, Page)],
    pub(crate) header: Header,
}

impl SharedFile {
    /// Creates a new database file holding only a header page. An orphan
    /// log left at the same path by an earlier database is emptied.
    pub(crate) fn create(vfs: Arc<dyn Vfs>, path: &Path) -> Result<Arc<SharedFile>> {
        let mut main = vfs.create_new(path)?;
        let header = Header::new();
        main.write_at(0, header.encode()?.bytes())?;
        main.set_len(PAGE_SIZE as u64)?;
        main.sync()?;
        let log_path = wal_path(path);
        let mut log = None;
        if vfs.exists(&log_path)? {
            let mut file = vfs.open(&log_path)?;
            file.set_len(0)?;
            file.sync()?;
            log = Some(file);
        }
        let key = (vfs.id(), vfs.canonicalize(path)?);
        let state = State::new(main, log, log_path, vfs, header, None);
        let shared = Arc::new(SharedFile {
            state: Mutex::new(state),
        });
        lock_registry().insert(key, Arc::downgrade(&shared));
        Ok(shared)
    }

    /// Attaches to the file's live state, or recovers it if no handle in
    /// this process has it open.
    pub(crate) fn open(vfs: Arc<dyn Vfs>, path: &Path) -> Result<Arc<SharedFile>> {
        let key = (vfs.id(), vfs.canonicalize(path)?);
        let mut map = lock_registry();
        if let Some(live) = map.get(&key).and_then(Weak::upgrade) {
            return Ok(live);
        }
        let state = recover(vfs, path)?;
        let shared = Arc::new(SharedFile {
            state: Mutex::new(state),
        });
        map.insert(key, Arc::downgrade(&shared));
        Ok(shared)
    }

    /// The state, or `Unusable` if it is poisoned (a log sync failed or a
    /// thread panicked while holding the lock).
    pub(crate) fn lock(&self) -> Result<MutexGuard<'_, State>> {
        match self.state.lock() {
            Ok(state) if !state.poisoned => Ok(state),
            Ok(_) => Err(StorageError::Unusable),
            Err(poison) => {
                poison.into_inner().poisoned = true;
                Err(StorageError::Unusable)
            }
        }
    }

    /// The state even when poisoned, for read-only accessors and cleanup.
    pub(crate) fn lock_any(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(|poison| {
            let mut state = poison.into_inner();
            state.poisoned = true;
            state
        })
    }
}

impl State {
    fn new(
        main: Box<dyn VfsFile>,
        log: Option<Box<dyn VfsFile>>,
        log_path: PathBuf,
        vfs: Arc<dyn Vfs>,
        header: Header,
        last_salt: Option<u64>,
    ) -> State {
        State {
            main,
            log,
            log_path,
            vfs,
            wal: WalState::default(),
            last_salt,
            published: Arc::new(Snapshot {
                seq: 1,
                header,
                index: HashMap::new(),
            }),
            writer: None,
            readers: 0,
            poisoned: false,
        }
    }

    /// Reads a committed page as seen by `view`: from the log if it changed
    /// since the last checkpoint, else from the main file.
    pub(crate) fn read_page(&mut self, view: &Snapshot, id: PageId) -> Result<Page> {
        let mut page = Page::zeroed();
        let (file, offset) = match (view.index.get(&id), self.log.as_mut()) {
            (Some(&frame), Some(log)) => (log, frame + RECORD_HEADER_SIZE as u64),
            (Some(_), None) => {
                return Err(StorageError::Corrupt {
                    page: id,
                    reason: "log frame without a log file",
                });
            }
            (None, _) => (&mut self.main, u64::from(id.0) * PAGE_SIZE as u64),
        };
        if file.read_at(offset, page.bytes_mut())? < PAGE_SIZE {
            return Err(StorageError::Corrupt {
                page: id,
                reason: "page lies beyond the end of the file",
            });
        }
        Ok(page)
    }

    /// Appends one transaction and makes it durable, then publishes it.
    /// Returns the new snapshot's sequence number. A failed write leaves the
    /// published state untouched; a failed sync poisons the file.
    pub(crate) fn append(&mut self, commit: &Commit<'_>) -> Result<u64> {
        let mut wal = self.wal;
        let fresh = (wal.end == 0).then(|| next_salt(self.last_salt));
        let log = self.open_log()?;
        if let Some(salt) = fresh {
            let (bytes, crc) = wal::encode_header(salt);
            log.write_at(0, &bytes)?;
            wal = WalState {
                salt,
                end: LOG_HEADER_SIZE,
                chain: crc,
                frames: 0,
                next_txn: 1,
            };
        }
        let txn = wal.next_txn;
        let mut offsets = Vec::with_capacity(commit.pages.len());
        for (id, page) in commit.pages {
            let (bytes, crc) = wal::encode_frame(*id, txn, wal.salt, page, wal.chain);
            log.write_at(wal.end, &bytes)?;
            offsets.push((*id, wal.end));
            wal.chain = crc;
            wal.end += FRAME_SIZE;
        }
        let count = commit.pages.len() as u32;
        let (bytes, crc) =
            wal::encode_commit(commit.header.page_count, txn, wal.salt, count, wal.chain);
        log.write_at(wal.end, &bytes)?;
        if let Err(e) = log.sync() {
            self.poisoned = true;
            return Err(e.into());
        }
        wal.chain = crc;
        wal.end += COMMIT_SIZE;
        wal.frames += count;
        wal.next_txn += 1;
        self.wal = wal;
        self.last_salt = Some(wal.salt);
        let mut index = self.published.index.clone();
        index.extend(offsets);
        let seq = self.published.seq + 1;
        self.published = Arc::new(Snapshot {
            seq,
            header: commit.header.clone(),
            index,
        });
        Ok(seq)
    }

    fn open_log(&mut self) -> Result<&mut Box<dyn VfsFile>> {
        if self.log.is_none() {
            self.log = Some(self.vfs.open_or_create(&self.log_path)?);
        }
        self.log.as_mut().ok_or(StorageError::Corrupt {
            page: PageId(0),
            reason: "log file missing",
        })
    }

    /// Copies the latest committed image of every logged page into the main
    /// file, syncs it and empties the log. Returns the pages copied.
    /// Failures before the log is truncated leave everything valid; a
    /// failure while truncating poisons the file.
    pub(crate) fn checkpoint(&mut self) -> Result<u32> {
        if self.wal.frames == 0 {
            return Ok(0);
        }
        let mut frames: Vec<(PageId, u64)> =
            self.published.index.iter().map(|(&p, &o)| (p, o)).collect();
        frames.sort_unstable();
        let page_count = self.published.header.page_count;
        let State { main, log, .. } = &mut *self;
        let log = log.as_mut().ok_or(StorageError::Corrupt {
            page: PageId(0),
            reason: "log file missing",
        })?;
        copy_frames(log.as_mut(), main.as_mut(), &frames, page_count)?;
        if let Err(e) = log.set_len(0).and_then(|()| log.sync()) {
            self.poisoned = true;
            return Err(e.into());
        }
        self.last_salt = Some(self.wal.salt);
        self.wal = WalState::default();
        self.published = Arc::new(Snapshot {
            seq: self.published.seq,
            header: self.published.header.clone(),
            index: HashMap::new(),
        });
        Ok(frames.len() as u32)
    }
}

/// Syncs the log, then copies frames into the main file, sets its length
/// and syncs it. The log sync is the barrier that keeps the main file from
/// ever holding a page whose commit is not durable.
fn copy_frames(
    log: &mut dyn VfsFile,
    main: &mut dyn VfsFile,
    frames: &[(PageId, u64)],
    page_count: u32,
) -> Result<()> {
    log.sync()?;
    let mut page = Page::zeroed();
    for &(id, offset) in frames {
        let read = log.read_at(offset + RECORD_HEADER_SIZE as u64, page.bytes_mut())?;
        if read < PAGE_SIZE {
            return Err(StorageError::Corrupt {
                page: id,
                reason: "log frame lies beyond the end of the log",
            });
        }
        main.write_at(u64::from(id.0) * PAGE_SIZE as u64, page.bytes())?;
    }
    main.set_len(u64::from(page_count) * PAGE_SIZE as u64)?;
    main.sync()?;
    Ok(())
}

/// A salt for a new log generation: one more than the previous one, so old
/// records can never validate, or a time-based value when none is known.
fn next_salt(last: Option<u64>) -> u64 {
    match last {
        Some(salt) => salt.wrapping_add(1).max(1),
        None => {
            let nanos = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos() as u64);
            (nanos ^ (u64::from(std::process::id()) << 32)).max(1)
        }
    }
}

/// First open in this process: replays every committed transaction in the
/// log into the main file (a checkpoint), empties the log, then validates
/// the header exactly as before the log existed. Every write here is a
/// deterministic function of the log, and the log is emptied only after the
/// main file is synced, so a crash during recovery is repaired by the next
/// open.
fn recover(vfs: Arc<dyn Vfs>, path: &Path) -> Result<State> {
    let mut main = vfs.open(path)?;
    let log_path = wal_path(path);
    let mut log = None;
    let mut last_salt = None;
    if vfs.exists(&log_path)? {
        let mut file = vfs.open(&log_path)?;
        let scan = wal::scan(file.as_mut())?;
        last_salt = scan.salt;
        let txns = consistent_prefix(file.as_mut(), scan.txns)?;
        if let Some(last) = txns.last() {
            let frames: Vec<(PageId, u64)> = wal::latest_frames(&txns).into_iter().collect();
            copy_frames(file.as_mut(), main.as_mut(), &frames, last.page_count)?;
        }
        if file.size()? > 0 {
            file.set_len(0)?;
            file.sync()?;
        }
        log = Some(file);
    }
    let len = main.size()?;
    if len < PAGE_SIZE as u64 {
        return Err(StorageError::FileTooShort { len });
    }
    if len % PAGE_SIZE as u64 != 0 {
        return Err(StorageError::NotPageMultiple { len });
    }
    let mut page = Page::zeroed();
    main.read_at(0, page.bytes_mut())?;
    let header = Header::decode(&page, len)?;
    Ok(State::new(main, log, log_path, vfs, header, last_salt))
}

/// The transactions whose commit page count matches the page count in the
/// latest page 0 frame before or in them. The main file's header is not a
/// valid reference: a crash during a checkpoint can leave it holding a newer
/// header copied from the log. The first mismatch ends the list.
fn consistent_prefix(log: &mut dyn VfsFile, txns: Vec<TxnRecord>) -> Result<Vec<TxnRecord>> {
    let mut page_count: Option<u32> = None;
    let mut kept = Vec::with_capacity(txns.len());
    for txn in txns {
        let header_frame = txn.frames.iter().rev().find(|(id, _)| id.0 == 0);
        if let Some(&(_, offset)) = header_frame {
            let at = offset + RECORD_HEADER_SIZE as u64 + HEADER_PAGE_COUNT_AT as u64;
            let mut field = [0u8; 4];
            log.read_at(at, &mut field)?;
            page_count = Some(le_u32(&field, 0));
        }
        if page_count.is_some_and(|count| count != txn.page_count) {
            break;
        }
        kept.push(txn);
    }
    Ok(kept)
}
