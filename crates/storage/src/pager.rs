//! Page store with write-ahead-log transactions, a free list and a buffer
//! pool.
//!
//! # Free page layout
//!
//! A freed page is overwritten with this layout and pushed onto a LIFO list
//! whose head and length live in the header.
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 1    | tag = 3 |
//! | 1      | 3    | reserved, zero |
//! | 4      | 4    | next free page id, `u32` (0 = end of list) |
//! | 8      | 4088 | zero |
//!
//! `free` refuses a page that already has exactly this layout, which detects
//! double frees. A caller page that happens to match it byte for byte is
//! indistinguishable from a free page.
//!
//! # Transactions and durability
//!
//! Every change happens inside a transaction: [`Pager::begin`] starts one
//! explicitly, and the first `allocate`, `write`, `free`, `set_root` or
//! `remove_root` outside one starts one implicitly. Changed pages and the
//! working header live in a per-handle overlay and never reach the main
//! file directly. [`Pager::commit`] appends their full images and a commit
//! record to `<file>-wal`, syncs it and publishes the new state to every
//! handle on the file; [`Pager::rollback`] discards the overlay.
//! [`Pager::savepoint`] marks a point that [`Pager::rollback_to`] returns
//! to. One handle per file may hold a transaction at a time; another
//! handle's `begin` (or implicit begin) fails with `StorageError::Busy`.
//!
//! A checkpoint copies committed pages from the log into the main file,
//! syncs it and empties the log. It runs when a commit leaves more than
//! `Options::checkpoint_frames` frames in the log, from [`Pager::sync`] and
//! [`Pager::close`], and on demand through [`Pager::checkpoint`]; it is
//! skipped while any handle is registered as a reader ([`Pager::begin_read`]).
//!
//! `sync` commits the open transaction (if any) and checkpoints, so after it
//! returns the main file alone holds the database, as before the log
//! existed. `close` does the same and reports errors. `Drop` commits an
//! implicit transaction, rolls back an explicit one, checkpoints and ignores
//! errors.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::error::{Result, StorageError};
use crate::header::{Header, MAX_ROOT_NAME_LEN, RootSlot};
use crate::page::{PAGE_SIZE, Page, PageId, Reader, Writer};
use crate::pool::{BufferPool, DEFAULT_POOL_PAGES, Loader, PoolStats};
use crate::shared::{Commit, SharedFile, Snapshot};
use crate::vfs::{OsVfs, Vfs};

const TAG_FREE: u8 = 3;
const FREE_NEXT: usize = 4;
const FREE_BODY: usize = 8;

/// Log frames after which a commit triggers a checkpoint.
pub const DEFAULT_CHECKPOINT_FRAMES: u32 = 1000;

/// Settings chosen when a handle is opened.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Options {
    /// Buffer pool capacity in pages (at least `MIN_POOL_PAGES`).
    pub pool_pages: usize,
    /// A commit that leaves more than this many frames in the log runs a
    /// checkpoint.
    pub checkpoint_frames: u32,
}

impl Options {
    pub fn new(pool_pages: usize, checkpoint_frames: u32) -> Options {
        Options {
            pool_pages,
            checkpoint_frames,
        }
    }
}

impl Default for Options {
    fn default() -> Options {
        Options::new(DEFAULT_POOL_PAGES, DEFAULT_CHECKPOINT_FRAMES)
    }
}

/// A savepoint inside the open transaction, from [`Pager::savepoint`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Savepoint(usize);

/// One handle on a database file. Several handles in one process share the
/// file's committed state; each has its own pool and transaction.
pub struct Pager {
    shared: Arc<SharedFile>,
    id: u64,
    checkpoint_frames: u32,
    pool: BufferPool,
    pool_seq: u64,
    txn: Option<Txn>,
    reader: Option<Arc<Snapshot>>,
    read_depth: u32,
    stats: PoolStats,
}

/// An open transaction: changed pages, the working header and one undo map
/// per savepoint level (the overlay entry a page had before the level first
/// touched it).
struct Txn {
    explicit: bool,
    overlay: BTreeMap<PageId, Page>,
    header: Header,
    base_header: Header,
    base_seq: u64,
    levels: Vec<Level>,
}

struct Level {
    header: Header,
    undo: HashMap<PageId, Option<Page>>,
}

impl Txn {
    fn put(&mut self, id: PageId, page: Page) {
        let overlay = &self.overlay;
        if let Some(level) = self.levels.last_mut() {
            level
                .undo
                .entry(id)
                .or_insert_with(|| overlay.get(&id).cloned());
        }
        self.overlay.insert(id, page);
    }
}

fn next_handle_id() -> u64 {
    static NEXT: AtomicU64 = AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::Relaxed)
}

impl Pager {
    /// Creates a new database file with the default pool capacity. Fails with
    /// `StorageError::Io` (kind `AlreadyExists`) if `path` exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Pager> {
        Pager::create_with(Arc::new(OsVfs), path.as_ref(), Options::default())
    }

    /// Creates a new database file whose pool caches `pool_pages` pages.
    pub fn create_with_capacity(path: impl AsRef<Path>, pool_pages: usize) -> Result<Pager> {
        let options = Options::new(pool_pages, DEFAULT_CHECKPOINT_FRAMES);
        Pager::create_with(Arc::new(OsVfs), path.as_ref(), options)
    }

    /// Opens an existing database file with the default pool capacity.
    pub fn open(path: impl AsRef<Path>) -> Result<Pager> {
        Pager::open_with(Arc::new(OsVfs), path.as_ref(), Options::default())
    }

    /// Opens an existing database file whose pool caches `pool_pages` pages.
    pub fn open_with_capacity(path: impl AsRef<Path>, pool_pages: usize) -> Result<Pager> {
        let options = Options::new(pool_pages, DEFAULT_CHECKPOINT_FRAMES);
        Pager::open_with(Arc::new(OsVfs), path.as_ref(), options)
    }

    /// Creates a new database file through `vfs`. An existing `<path>-wal`
    /// is emptied, because it cannot belong to the new file.
    pub fn create_with(vfs: Arc<dyn Vfs>, path: &Path, options: Options) -> Result<Pager> {
        let pool = BufferPool::new(options.pool_pages)?;
        let shared = SharedFile::create(vfs, path)?;
        Ok(Pager::attach(shared, pool, options))
    }

    /// Opens an existing database file through `vfs`. If no other handle in
    /// this process has it open, committed transactions in `<path>-wal` are
    /// replayed into it first; then the file length, magic, version and
    /// every header field are validated.
    pub fn open_with(vfs: Arc<dyn Vfs>, path: &Path, options: Options) -> Result<Pager> {
        let pool = BufferPool::new(options.pool_pages)?;
        let shared = SharedFile::open(vfs, path)?;
        Ok(Pager::attach(shared, pool, options))
    }

    fn attach(shared: Arc<SharedFile>, pool: BufferPool, options: Options) -> Pager {
        Pager {
            shared,
            id: next_handle_id(),
            checkpoint_frames: options.checkpoint_frames,
            pool,
            pool_seq: 0,
            txn: None,
            reader: None,
            read_depth: 0,
            stats: PoolStats::default(),
        }
    }

    /// Starts an explicit write transaction. Fails with `Busy` if another
    /// handle holds one and `TransactionOpen` if this handle does.
    pub fn begin(&mut self) -> Result<()> {
        if self.txn.is_some() {
            return Err(StorageError::TransactionOpen);
        }
        self.start(true)
    }

    pub fn in_transaction(&self) -> bool {
        self.txn.is_some()
    }

    fn start(&mut self, explicit: bool) -> Result<()> {
        let mut state = self.shared.lock()?;
        if state.writer.is_some_and(|w| w != self.id) {
            return Err(StorageError::Busy {
                reason: "another handle has an open write transaction",
            });
        }
        state.writer = Some(self.id);
        let published = Arc::clone(&state.published);
        drop(state);
        self.txn = Some(Txn {
            explicit,
            overlay: BTreeMap::new(),
            header: published.header.clone(),
            base_header: published.header.clone(),
            base_seq: published.seq,
            levels: Vec::new(),
        });
        Ok(())
    }

    fn txn_mut(&mut self) -> Result<&mut Txn> {
        if self.txn.is_none() {
            self.start(false)?;
        }
        self.txn.as_mut().ok_or(StorageError::NoTransaction)
    }

    /// Makes the open transaction durable and visible to every handle, and
    /// returns the sequence number of the committed state it produced. An
    /// empty transaction only releases the write lock and returns the state
    /// it started from. Callers that cache derived state (such as a catalog)
    /// must record this value rather than `snapshot_seq()`, because another
    /// handle may commit as soon as the write lock is released. On a write
    /// error the transaction is rolled back; on a log sync error the file
    /// becomes unusable (the transaction may or may not survive a reopen).
    pub fn commit(&mut self) -> Result<u64> {
        let txn = self.txn.take().ok_or(StorageError::NoTransaction)?;
        let result = self.commit_txn(txn);
        self.shared.lock_any().writer = None;
        result
    }

    fn commit_txn(&mut self, txn: Txn) -> Result<u64> {
        let mut pages: Vec<(PageId, Page)> = Vec::with_capacity(txn.overlay.len() + 1);
        if txn.header != txn.base_header {
            pages.push((PageId(0), txn.header.encode()?));
        }
        pages.extend(txn.overlay);
        if pages.is_empty() {
            return Ok(txn.base_seq);
        }
        let mut state = self.shared.lock()?;
        let commit = Commit {
            pages: &pages,
            header: txn.header,
        };
        let seq = state.append(&commit)?;
        self.stats.wal_frames += pages.len() as u64;
        if self.pool_seq == txn.base_seq {
            for (id, page) in &pages {
                self.pool.replace_if_cached(*id, page);
            }
            self.pool_seq = seq;
        }
        if state.wal.frames > self.checkpoint_frames && state.readers == 0 {
            // The commit is durable; a failed checkpoint is retried by the
            // next one, and a failure that poisons the file shows up there.
            if let Ok(copied) = state.checkpoint() {
                drop(state);
                self.count_checkpoint(copied);
            }
        }
        Ok(seq)
    }

    /// Discards the open transaction and releases the write lock.
    pub fn rollback(&mut self) -> Result<()> {
        self.txn.take().ok_or(StorageError::NoTransaction)?;
        self.shared.lock_any().writer = None;
        Ok(())
    }

    /// Marks the current state of the open transaction.
    pub fn savepoint(&mut self) -> Result<Savepoint> {
        let txn = self.txn.as_mut().ok_or(StorageError::NoTransaction)?;
        txn.levels.push(Level {
            header: txn.header.clone(),
            undo: HashMap::new(),
        });
        Ok(Savepoint(txn.levels.len()))
    }

    /// Undoes every change since `sp` and discards `sp` and every later
    /// savepoint. The transaction stays open.
    pub fn rollback_to(&mut self, sp: Savepoint) -> Result<()> {
        let txn = self.valid_savepoint(sp)?;
        while txn.levels.len() >= sp.0 {
            let Some(level) = txn.levels.pop() else { break };
            for (id, previous) in level.undo {
                match previous {
                    Some(page) => txn.overlay.insert(id, page),
                    None => txn.overlay.remove(&id),
                };
            }
            txn.header = level.header;
        }
        Ok(())
    }

    /// Keeps the changes since `sp` and discards `sp` and every later
    /// savepoint; an enclosing savepoint can still undo them.
    pub fn release(&mut self, sp: Savepoint) -> Result<()> {
        let txn = self.valid_savepoint(sp)?;
        while txn.levels.len() >= sp.0 {
            let Some(level) = txn.levels.pop() else { break };
            let Some(lower) = txn.levels.last_mut() else {
                break;
            };
            for (id, previous) in level.undo {
                lower.undo.entry(id).or_insert(previous);
            }
        }
        Ok(())
    }

    fn valid_savepoint(&mut self, sp: Savepoint) -> Result<&mut Txn> {
        let txn = self.txn.as_mut().ok_or(StorageError::NoTransaction)?;
        if sp.0 == 0 || sp.0 > txn.levels.len() {
            return Err(StorageError::InvalidSavepoint);
        }
        Ok(txn)
    }

    /// Registers this handle as a reader: until the matching `end_read`, it
    /// sees the committed state of this moment, and checkpoints wait. Calls
    /// nest; inside a transaction they only count.
    pub fn begin_read(&mut self) -> Result<()> {
        if self.txn.is_none() && self.read_depth == 0 {
            let mut state = self.shared.lock()?;
            state.readers += 1;
            self.reader = Some(Arc::clone(&state.published));
        }
        self.read_depth += 1;
        Ok(())
    }

    pub fn end_read(&mut self) {
        if self.read_depth == 0 {
            return;
        }
        self.read_depth -= 1;
        if self.read_depth == 0 && self.reader.take().is_some() {
            let mut state = self.shared.lock_any();
            state.readers = state.readers.saturating_sub(1);
        }
    }

    /// Sequence number of the committed state this handle reads; it changes
    /// whenever any handle commits.
    pub fn snapshot_seq(&self) -> u64 {
        if let Some(txn) = &self.txn {
            return txn.base_seq;
        }
        if let Some(reader) = &self.reader {
            return reader.seq;
        }
        self.shared.lock_any().published.seq
    }

    /// Copies committed log frames into the main file, syncs it and empties
    /// the log; returns the number of pages copied. Fails with
    /// `TransactionOpen` inside a transaction and `Busy` while any handle is
    /// registered as a reader.
    pub fn checkpoint(&mut self) -> Result<u32> {
        if self.txn.is_some() {
            return Err(StorageError::TransactionOpen);
        }
        let mut state = self.shared.lock()?;
        if state.readers > 0 {
            return Err(StorageError::Busy {
                reason: "readers are active",
            });
        }
        let copied = state.checkpoint()?;
        drop(state);
        self.count_checkpoint(copied);
        Ok(copied)
    }

    fn count_checkpoint(&mut self, copied: u32) {
        if copied > 0 {
            self.stats.checkpoints += 1;
            self.stats.page_writes += u64::from(copied);
        }
    }

    /// Committed frames currently in the log.
    pub fn log_frames(&self) -> u32 {
        self.shared.lock_any().wal.frames
    }

    /// Returns a zeroed page, reusing the free list before growing the file.
    pub fn allocate(&mut self) -> Result<PageId> {
        if self.header(|h| h.free_head) != 0 {
            return self.pop_free();
        }
        let txn = self.txn_mut()?;
        if txn.header.page_count == u32::MAX {
            return Err(StorageError::DatabaseFull);
        }
        let id = PageId(txn.header.page_count);
        txn.put(id, Page::zeroed());
        txn.header.page_count += 1;
        Ok(id)
    }

    /// The page as this handle sees it: its own uncommitted change if any,
    /// else the committed image.
    pub fn read(&mut self, id: PageId) -> Result<Page> {
        self.check_id(id)?;
        if let Some(page) = self.txn.as_ref().and_then(|t| t.overlay.get(&id)) {
            return Ok(page.clone());
        }
        self.with_pool(|pool, load| Ok(pool.page(id, load)?.clone()))
    }

    pub fn write(&mut self, id: PageId, page: &Page) -> Result<()> {
        self.check_id(id)?;
        self.txn_mut()?.put(id, page.clone());
        Ok(())
    }

    /// Pushes `id` onto the free list stored in the freed pages themselves.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        self.check_id(id)?;
        let page_count = self.page_count();
        if decode_free_page(&self.read(id)?, id, page_count).is_ok() {
            return Err(StorageError::InvalidPage {
                page: id,
                reason: "already free",
            });
        }
        let txn = self.txn_mut()?;
        if txn.header.free_count >= page_count - 1 {
            return Err(corrupt_header("free list count"));
        }
        let page = encode_free_page(id, txn.header.free_head)?;
        txn.put(id, page);
        txn.header.free_head = id.0;
        txn.header.free_count += 1;
        Ok(())
    }

    /// Number of pages in the file, including the header.
    pub fn page_count(&self) -> u32 {
        self.header(|h| h.page_count)
    }

    /// Commits the open transaction, if any, then checkpoints unless readers
    /// are active, so the main file holds every committed change. A no-op
    /// when nothing changed.
    pub fn sync(&mut self) -> Result<()> {
        if self.txn.is_some() {
            self.commit()?;
        }
        let mut state = self.shared.lock()?;
        if state.readers > 0 {
            return Ok(());
        }
        let copied = state.checkpoint()?;
        drop(state);
        self.count_checkpoint(copied);
        Ok(())
    }

    /// Ends any read registration, syncs and closes the handle, reporting any
    /// error `Drop` would swallow.
    pub fn close(mut self) -> Result<()> {
        self.end_all_reads();
        self.sync()
    }

    /// Keeps `id` resident until a matching `unpin`. Pins nest.
    pub fn pin(&mut self, id: PageId) -> Result<()> {
        self.check_id(id)?;
        self.with_pool(|pool, load| pool.pin(id, load))
    }

    pub fn unpin(&mut self, id: PageId) -> Result<()> {
        self.check_id(id)?;
        self.pool.unpin(id)
    }

    /// The free list, head first. Fails with `Corrupt` if the list is longer
    /// than the header says, cycles, or contains a page that is not free.
    pub fn free_list(&mut self) -> Result<Vec<PageId>> {
        let (expected, page_count, mut next) =
            self.header(|h| (h.free_count as usize, h.page_count, h.free_head));
        let mut list = Vec::with_capacity(expected);
        let mut seen = HashSet::with_capacity(expected);
        while next != 0 {
            if list.len() >= expected || !seen.insert(next) {
                return Err(corrupt_header(
                    "free list is longer than its count or cycles",
                ));
            }
            let id = PageId(next);
            list.push(id);
            next = decode_free_page(&self.read(id)?, id, page_count)?;
        }
        if list.len() != expected {
            return Err(corrupt_header("free list is shorter than its count"));
        }
        Ok(list)
    }

    /// Records `root` under `name`, replacing an existing entry of that name.
    pub fn set_root(&mut self, name: &str, root: PageId) -> Result<()> {
        if name.is_empty() || name.len() > MAX_ROOT_NAME_LEN {
            return Err(StorageError::InvalidRootName);
        }
        self.check_id(root)?;
        let roots = &self.txn_mut()?.header.roots;
        let index = roots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|s| s.name == name))
            .or_else(|| roots.iter().position(Option::is_none))
            .ok_or(StorageError::RootTableFull)?;
        if let Some(slot) = self.txn_mut()?.header.roots.get_mut(index) {
            *slot = Some(RootSlot {
                name: name.to_owned(),
                root,
            });
        }
        Ok(())
    }

    pub fn root(&self, name: &str) -> Option<PageId> {
        self.header(|h| {
            h.roots
                .iter()
                .flatten()
                .find(|slot| slot.name == name)
                .map(|slot| slot.root)
        })
    }

    /// Removes the root called `name`; returns whether it existed.
    pub fn remove_root(&mut self, name: &str) -> Result<bool> {
        if self.root(name).is_none() {
            return Ok(false);
        }
        let header = &mut self.txn_mut()?.header;
        let found = header
            .roots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|s| s.name == name));
        if let Some(slot) = found {
            *slot = None;
        }
        Ok(true)
    }

    /// All named roots in slot order.
    pub fn roots(&self) -> Vec<(String, PageId)> {
        self.header(|h| {
            h.roots
                .iter()
                .flatten()
                .map(|slot| (slot.name.clone(), slot.root))
                .collect()
        })
    }

    pub fn pool_capacity(&self) -> usize {
        self.pool.capacity()
    }

    /// Number of pages currently cached.
    pub fn resident(&self) -> usize {
        self.pool.resident()
    }

    pub fn is_cached(&self, id: PageId) -> bool {
        self.pool.is_cached(id)
    }

    pub fn stats(&self) -> PoolStats {
        let pool = self.pool.stats();
        PoolStats {
            page_writes: self.stats.page_writes,
            wal_frames: self.stats.wal_frames,
            checkpoints: self.stats.checkpoints,
            ..pool
        }
    }

    /// Runs `f` on the header this handle sees: its transaction's working
    /// header, its registered reader snapshot, or the latest commit.
    fn header<T>(&self, f: impl FnOnce(&Header) -> T) -> T {
        if let Some(txn) = &self.txn {
            return f(&txn.header);
        }
        if let Some(reader) = &self.reader {
            return f(&reader.header);
        }
        f(&self.shared.lock_any().published.header)
    }

    /// Runs a pool operation with a loader that reads committed pages as
    /// this handle sees them, first dropping cached pages if another handle
    /// committed since they were loaded.
    fn with_pool<T>(
        &mut self,
        op: impl FnOnce(&mut BufferPool, &mut Loader<'_>) -> Result<T>,
    ) -> Result<T> {
        let mut state = self.shared.lock()?;
        let view = match (&self.txn, &self.reader) {
            (None, Some(reader)) => Arc::clone(reader),
            _ => Arc::clone(&state.published),
        };
        let mut load = |id: PageId| state.read_page(&view, id);
        if view.seq != self.pool_seq {
            self.pool.invalidate(&mut load)?;
            self.pool_seq = view.seq;
        }
        op(&mut self.pool, &mut load)
    }

    fn pop_free(&mut self) -> Result<PageId> {
        let (head, free_count, page_count) =
            self.header(|h| (h.free_head, h.free_count, h.page_count));
        let id = PageId(head);
        let next = decode_free_page(&self.read(id)?, id, page_count)?;
        let remaining = free_count
            .checked_sub(1)
            .ok_or_else(|| corrupt_header("free list count"))?;
        if (next == 0) != (remaining == 0) {
            return Err(corrupt_header("free list count"));
        }
        let txn = self.txn_mut()?;
        txn.put(id, Page::zeroed());
        txn.header.free_head = next;
        txn.header.free_count = remaining;
        Ok(id)
    }

    fn check_id(&self, id: PageId) -> Result<()> {
        if id.0 == 0 {
            return Err(StorageError::InvalidPage {
                page: id,
                reason: "page 0 is the header",
            });
        }
        if id.0 >= self.page_count() {
            return Err(StorageError::InvalidPage {
                page: id,
                reason: "beyond page count",
            });
        }
        Ok(())
    }

    fn end_all_reads(&mut self) {
        while self.read_depth > 0 {
            self.end_read();
        }
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
        self.end_all_reads();
        match &self.txn {
            Some(txn) if txn.explicit => {
                let _ = self.rollback();
            }
            Some(_) => {
                let _ = self.commit();
            }
            None => {}
        }
        let _ = self.sync();
    }
}

fn encode_free_page(id: PageId, next: u32) -> Result<Page> {
    let mut page = Page::zeroed();
    let mut w = Writer::new(page.bytes_mut(), id);
    w.put_u8(0, TAG_FREE)?;
    w.put_u32(FREE_NEXT, next)?;
    Ok(page)
}

/// Returns the next link of a well-formed free page.
fn decode_free_page(page: &Page, id: PageId, page_count: u32) -> Result<u32> {
    let corrupt = StorageError::Corrupt {
        page: id,
        reason: "not a well-formed free page",
    };
    let r = Reader::new(page.bytes(), id);
    if r.u8(0)? != TAG_FREE {
        return Err(corrupt);
    }
    r.zeros(1, FREE_NEXT - 1, "not a well-formed free page")?;
    let next = r.u32(FREE_NEXT)?;
    if next >= page_count || next == id.0 {
        return Err(corrupt);
    }
    r.zeros(
        FREE_BODY,
        PAGE_SIZE - FREE_BODY,
        "not a well-formed free page",
    )?;
    Ok(next)
}

fn corrupt_header(reason: &'static str) -> StorageError {
    StorageError::Corrupt {
        page: PageId(0),
        reason,
    }
}
