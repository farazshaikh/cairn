//! Bounded cache of committed pages with least-recently-used eviction and
//! pinning.
//!
//! The pool holds at most `capacity` frames. A frame caches one committed
//! page plus a pin count. Unpinned frames are ordered by last use in `lru` (a
//! map from a monotonically increasing tick to the frame slot), so both a hit
//! and a victim lookup are O(log n). Pinned frames are absent from `lru` and
//! therefore never evicted; when no unpinned frame remains and a new page is
//! needed, the pool returns `StorageError::PoolExhausted`.
//!
//! The pool never writes. Uncommitted changes live in the pager's
//! transaction overlay and reach the file only through the write-ahead log,
//! so evicting a frame simply drops it. Pages are loaded through a closure
//! supplied by the pager, which reads the log or the main file.

use std::collections::{BTreeMap, HashMap};

use crate::error::{Result, StorageError};
use crate::page::{Page, PageId};

/// Smallest buffer pool capacity accepted at open.
pub const MIN_POOL_PAGES: usize = 8;
/// Capacity used by `Pager::create` and `Pager::open`.
pub const DEFAULT_POOL_PAGES: usize = 256;

/// Counters exposed for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Requests served from a resident frame.
    pub hits: u64,
    /// Requests that had to load the page.
    pub misses: u64,
    /// Pages read from the log or the main file.
    pub page_reads: u64,
    /// Pages this handle copied into the main file (checkpoints, replay).
    pub page_writes: u64,
    /// Frames reclaimed to make room for another page.
    pub evictions: u64,
    /// Page frames this handle appended to the write-ahead log.
    pub wal_frames: u64,
    /// Checkpoints this handle completed.
    pub checkpoints: u64,
}

/// Loads a committed page on a miss.
pub(crate) type Loader<'a> = dyn FnMut(PageId) -> Result<Page> + 'a;

struct Frame {
    id: PageId,
    page: Page,
    pins: u32,
    tick: u64,
}

pub(crate) struct BufferPool {
    capacity: usize,
    frames: Vec<Frame>,
    index: HashMap<PageId, usize>,
    lru: BTreeMap<u64, usize>,
    clock: u64,
    stats: PoolStats,
}

impl BufferPool {
    pub(crate) fn new(capacity: usize) -> Result<BufferPool> {
        if capacity < MIN_POOL_PAGES {
            return Err(StorageError::PoolTooSmall {
                requested: capacity,
                min: MIN_POOL_PAGES,
            });
        }
        Ok(BufferPool {
            capacity,
            frames: Vec::with_capacity(capacity),
            index: HashMap::with_capacity(capacity),
            lru: BTreeMap::new(),
            clock: 0,
            stats: PoolStats::default(),
        })
    }

    pub(crate) fn capacity(&self) -> usize {
        self.capacity
    }

    pub(crate) fn resident(&self) -> usize {
        self.index.len()
    }

    pub(crate) fn is_cached(&self, id: PageId) -> bool {
        self.index.contains_key(&id)
    }

    pub(crate) fn stats(&self) -> PoolStats {
        self.stats
    }

    /// The cached copy of `id`, loaded on a miss.
    pub(crate) fn page(&mut self, id: PageId, load: &mut Loader<'_>) -> Result<&Page> {
        let slot = self.fetch(id, load)?;
        Ok(&self.frame(slot)?.page)
    }

    /// Updates a resident frame after this handle committed a new image of
    /// it; a page that is not resident stays absent.
    pub(crate) fn replace_if_cached(&mut self, id: PageId, page: &Page) {
        let slot = self.index.get(&id).copied();
        if let Some(frame) = slot.and_then(|slot| self.frames.get_mut(slot)) {
            frame.page = page.clone();
        }
    }

    /// Forgets every unpinned frame and reloads pinned ones, after another
    /// handle committed and the cached images may be stale.
    pub(crate) fn invalidate(&mut self, load: &mut Loader<'_>) -> Result<()> {
        self.lru.clear();
        let pinned: Vec<(PageId, usize)> = self.index.iter().map(|(&id, &s)| (id, s)).collect();
        let mut frames = Vec::with_capacity(self.capacity);
        let mut index = HashMap::with_capacity(self.capacity);
        for (id, slot) in pinned {
            let old = self.frame(slot)?;
            if old.pins == 0 {
                continue;
            }
            let (pins, tick) = (old.pins, old.tick);
            let page = load(id)?;
            self.stats.page_reads += 1;
            index.insert(id, frames.len());
            frames.push(Frame {
                id,
                page,
                pins,
                tick,
            });
        }
        self.frames = frames;
        self.index = index;
        Ok(())
    }

    pub(crate) fn pin(&mut self, id: PageId, load: &mut Loader<'_>) -> Result<()> {
        let slot = self.fetch(id, load)?;
        let frame = self.frames.get_mut(slot).ok_or_else(slot_error)?;
        if frame.pins == 0 {
            self.lru.remove(&frame.tick);
        }
        frame.pins = frame.pins.checked_add(1).ok_or(StorageError::InvalidPage {
            page: id,
            reason: "pin count overflow",
        })?;
        Ok(())
    }

    pub(crate) fn unpin(&mut self, id: PageId) -> Result<()> {
        let not_pinned = StorageError::InvalidPage {
            page: id,
            reason: "not pinned",
        };
        let slot = *self.index.get(&id).ok_or(not_pinned)?;
        let frame = self.frame_mut(slot)?;
        if frame.pins == 0 {
            return Err(StorageError::InvalidPage {
                page: id,
                reason: "not pinned",
            });
        }
        frame.pins -= 1;
        if frame.pins > 0 {
            return Ok(());
        }
        self.touch(slot)
    }

    fn fetch(&mut self, id: PageId, load: &mut Loader<'_>) -> Result<usize> {
        if let Some(&slot) = self.index.get(&id) {
            self.stats.hits += 1;
            self.touch(slot)?;
            return Ok(slot);
        }
        self.stats.misses += 1;
        if self.frames.len() >= self.capacity && self.lru.is_empty() {
            return Err(StorageError::PoolExhausted);
        }
        let page = load(id)?;
        self.stats.page_reads += 1;
        let frame = Frame {
            id,
            page,
            pins: 0,
            tick: 0,
        };
        let slot = self.free_slot()?;
        match self.frames.get_mut(slot) {
            Some(existing) => *existing = frame,
            None => self.frames.push(frame),
        }
        self.index.insert(id, slot);
        self.touch(slot)?;
        Ok(slot)
    }

    /// A slot for a new frame: an unused one while below capacity, else the
    /// least recently used unpinned frame, which is dropped.
    fn free_slot(&mut self) -> Result<usize> {
        if self.frames.len() < self.capacity {
            return Ok(self.frames.len());
        }
        let (&tick, &slot) = self
            .lru
            .first_key_value()
            .ok_or(StorageError::PoolExhausted)?;
        let victim = self.frame(slot)?.id;
        self.lru.remove(&tick);
        self.index.remove(&victim);
        self.stats.evictions += 1;
        Ok(slot)
    }

    /// Marks `slot` as most recently used.
    fn touch(&mut self, slot: usize) -> Result<()> {
        self.clock += 1;
        let tick = self.clock;
        let frame = self.frames.get_mut(slot).ok_or_else(slot_error)?;
        let old = std::mem::replace(&mut frame.tick, tick);
        if frame.pins == 0 {
            self.lru.remove(&old);
            self.lru.insert(tick, slot);
        }
        Ok(())
    }

    fn frame(&self, slot: usize) -> Result<&Frame> {
        self.frames.get(slot).ok_or_else(slot_error)
    }

    fn frame_mut(&mut self, slot: usize) -> Result<&mut Frame> {
        self.frames.get_mut(slot).ok_or_else(slot_error)
    }
}

fn slot_error() -> StorageError {
    StorageError::Corrupt {
        page: PageId(0),
        reason: "buffer pool slot index out of range",
    }
}
