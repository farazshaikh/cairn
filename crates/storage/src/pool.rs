//! Bounded page cache with least-recently-used eviction and pinning.
//!
//! The pool holds at most `capacity` frames. A frame caches one page plus a
//! dirty flag and a pin count. Unpinned frames are ordered by last use in
//! `lru` (a map from a monotonically increasing tick to the frame slot), so
//! both a hit and a victim lookup are O(log n). Pinned frames are absent from
//! `lru` and therefore never evicted; when no unpinned frame remains and a new
//! page is needed, the pool returns `StorageError::PoolExhausted`.
//!
//! Dirty frames are written back when they are evicted and on `flush`.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::{ErrorKind, Read, Seek, SeekFrom, Write};

use crate::error::{Result, StorageError};
use crate::page::{PAGE_SIZE, Page, PageId};

/// Smallest buffer pool capacity accepted at open.
pub const MIN_POOL_PAGES: usize = 8;
/// Capacity used by `Pager::create` and `Pager::open`.
pub const DEFAULT_POOL_PAGES: usize = 256;

/// Counters exposed for diagnostics and tests.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct PoolStats {
    /// Requests served from a resident frame.
    pub hits: u64,
    /// Requests that had to read the page from the file.
    pub misses: u64,
    /// Pages read from the file.
    pub page_reads: u64,
    /// Pages written to the file (eviction write-back and flushes).
    pub page_writes: u64,
    /// Frames reclaimed to make room for another page.
    pub evictions: u64,
}

struct Frame {
    id: PageId,
    page: Page,
    dirty: bool,
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
        self.frames.len()
    }

    pub(crate) fn is_cached(&self, id: PageId) -> bool {
        self.index.contains_key(&id)
    }

    pub(crate) fn stats(&self) -> PoolStats {
        self.stats
    }

    /// The cached copy of `id`, read from `file` on a miss.
    pub(crate) fn page(&mut self, file: &mut File, id: PageId) -> Result<&Page> {
        let slot = self.fetch(file, id)?;
        Ok(&self.frame(slot)?.page)
    }

    /// Replaces the cached contents of `id` and marks it dirty. A miss does
    /// not read the file, because the whole page is being overwritten.
    pub(crate) fn install(&mut self, file: &mut File, id: PageId, page: Page) -> Result<()> {
        if let Some(&slot) = self.index.get(&id) {
            let frame = self.frame_mut(slot)?;
            frame.page = page;
            frame.dirty = true;
            return self.touch(slot);
        }
        self.insert(file, id, page, true).map(|_| ())
    }

    pub(crate) fn pin(&mut self, file: &mut File, id: PageId) -> Result<()> {
        let slot = self.fetch(file, id)?;
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

    /// Writes every dirty frame in ascending page order. Clean frames are
    /// never written.
    pub(crate) fn flush(&mut self, file: &mut File) -> Result<()> {
        let mut dirty: Vec<usize> = (0..self.frames.len())
            .filter(|&slot| self.frames.get(slot).is_some_and(|f| f.dirty))
            .collect();
        dirty.sort_by_key(|&slot| self.frames.get(slot).map(|f| f.id));
        for slot in dirty {
            let frame = self.frames.get_mut(slot).ok_or_else(slot_error)?;
            write_page(file, frame.id, &frame.page)?;
            frame.dirty = false;
            self.stats.page_writes += 1;
        }
        Ok(())
    }

    fn fetch(&mut self, file: &mut File, id: PageId) -> Result<usize> {
        if let Some(&slot) = self.index.get(&id) {
            self.stats.hits += 1;
            self.touch(slot)?;
            return Ok(slot);
        }
        self.stats.misses += 1;
        let page = read_page(file, id)?;
        self.stats.page_reads += 1;
        self.insert(file, id, page, false)
    }

    fn insert(&mut self, file: &mut File, id: PageId, page: Page, dirty: bool) -> Result<usize> {
        let frame = Frame {
            id,
            page,
            dirty,
            pins: 0,
            tick: 0,
        };
        let slot = if self.frames.len() < self.capacity {
            self.frames.push(frame);
            self.frames.len() - 1
        } else {
            let slot = self.evict(file)?;
            *self.frame_mut(slot)? = frame;
            slot
        };
        self.index.insert(id, slot);
        self.touch(slot)?;
        Ok(slot)
    }

    /// Frees the least recently used unpinned frame, writing it back first
    /// if dirty. On a write failure the victim stays resident and dirty.
    fn evict(&mut self, file: &mut File) -> Result<usize> {
        let (&tick, &slot) = self
            .lru
            .first_key_value()
            .ok_or(StorageError::PoolExhausted)?;
        let frame = self.frames.get_mut(slot).ok_or_else(slot_error)?;
        if frame.dirty {
            write_page(file, frame.id, &frame.page)?;
            frame.dirty = false;
            self.stats.page_writes += 1;
        }
        let victim = frame.id;
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

fn offset(id: PageId) -> u64 {
    u64::from(id.0) * PAGE_SIZE as u64
}

/// Reads page `id` straight from the file. A page past the end of the file
/// is reported as corrupt rather than as an I/O error.
pub(crate) fn read_page(file: &mut File, id: PageId) -> Result<Page> {
    let mut page = Page::zeroed();
    file.seek(SeekFrom::Start(offset(id)))?;
    match file.read_exact(page.bytes_mut()) {
        Ok(()) => Ok(page),
        Err(e) if e.kind() == ErrorKind::UnexpectedEof => Err(StorageError::Corrupt {
            page: id,
            reason: "page lies beyond the end of the file",
        }),
        Err(e) => Err(e.into()),
    }
}

pub(crate) fn write_page(file: &mut File, id: PageId, page: &Page) -> Result<()> {
    file.seek(SeekFrom::Start(offset(id)))?;
    file.write_all(page.bytes())?;
    Ok(())
}
