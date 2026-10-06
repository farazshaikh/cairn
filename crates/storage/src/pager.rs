//! Single-file page store: header, free list and buffer pool.
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
//! # Durability
//!
//! The header lives in memory. `sync` writes dirty pages in ascending order,
//! then the header, sets the file length to `page_count * 4096` and calls
//! `File::sync_all`. `close` does the same and reports errors; `Drop` makes a
//! best-effort sync and ignores errors. There is no crash atomicity: a crash
//! before `sync` completes can leave a file that `open` rejects.

use std::collections::HashSet;
use std::fs::File;
use std::path::Path;

use crate::error::{Result, StorageError};
use crate::header::{Header, MAX_ROOT_NAME_LEN, RootSlot};
use crate::page::{PAGE_SIZE, Page, PageId, Reader, Writer};
use crate::pool::{BufferPool, DEFAULT_POOL_PAGES, PoolStats, read_page, write_page};

const TAG_FREE: u8 = 3;
const FREE_NEXT: usize = 4;
const FREE_BODY: usize = 8;

/// A database file of fixed 4096-byte pages, cached by a bounded buffer pool.
pub struct Pager {
    file: File,
    header: Header,
    pool: BufferPool,
    needs_sync: bool,
}

impl Pager {
    /// Creates a new database file with the default pool capacity. Fails with
    /// `StorageError::Io` (kind `AlreadyExists`) if `path` exists.
    pub fn create(path: impl AsRef<Path>) -> Result<Pager> {
        Pager::create_with_capacity(path, DEFAULT_POOL_PAGES)
    }

    /// Creates a new database file whose pool caches `pool_pages` pages.
    pub fn create_with_capacity(path: impl AsRef<Path>, pool_pages: usize) -> Result<Pager> {
        let pool = BufferPool::new(pool_pages)?;
        let mut file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        let header = Header::new();
        write_page(&mut file, PageId(0), &header.encode()?)?;
        file.set_len(PAGE_SIZE as u64)?;
        file.sync_all()?;
        Ok(Pager {
            file,
            header,
            pool,
            needs_sync: false,
        })
    }

    /// Opens an existing database file with the default pool capacity.
    pub fn open(path: impl AsRef<Path>) -> Result<Pager> {
        Pager::open_with_capacity(path, DEFAULT_POOL_PAGES)
    }

    /// Opens an existing database file whose pool caches `pool_pages` pages.
    /// The file length, magic, version and every header field are validated
    /// before the file is used.
    pub fn open_with_capacity(path: impl AsRef<Path>, pool_pages: usize) -> Result<Pager> {
        let pool = BufferPool::new(pool_pages)?;
        let mut file = File::options().read(true).write(true).open(path)?;
        let len = file.metadata()?.len();
        if len < PAGE_SIZE as u64 {
            return Err(StorageError::FileTooShort { len });
        }
        if len % PAGE_SIZE as u64 != 0 {
            return Err(StorageError::NotPageMultiple { len });
        }
        let header = Header::decode(&read_page(&mut file, PageId(0))?, len)?;
        Ok(Pager {
            file,
            header,
            pool,
            needs_sync: false,
        })
    }

    /// Returns a zeroed page, reusing the free list before growing the file.
    pub fn allocate(&mut self) -> Result<PageId> {
        if self.header.free_head != 0 {
            return self.pop_free();
        }
        if self.header.page_count == u32::MAX {
            return Err(StorageError::DatabaseFull);
        }
        let id = PageId(self.header.page_count);
        self.pool.install(&mut self.file, id, Page::zeroed())?;
        self.header.page_count += 1;
        self.needs_sync = true;
        Ok(id)
    }

    pub fn read(&mut self, id: PageId) -> Result<Page> {
        self.check_id(id)?;
        Ok(self.pool.page(&mut self.file, id)?.clone())
    }

    pub fn write(&mut self, id: PageId, page: &Page) -> Result<()> {
        self.check_id(id)?;
        self.pool.install(&mut self.file, id, page.clone())?;
        self.needs_sync = true;
        Ok(())
    }

    /// Pushes `id` onto the free list stored in the freed pages themselves.
    pub fn free(&mut self, id: PageId) -> Result<()> {
        self.check_id(id)?;
        let page_count = self.header.page_count;
        if decode_free_page(self.pool.page(&mut self.file, id)?, id, page_count).is_ok() {
            return Err(StorageError::InvalidPage {
                page: id,
                reason: "already free",
            });
        }
        if self.header.free_count >= page_count - 1 {
            return Err(corrupt_header("free list count"));
        }
        let page = encode_free_page(id, self.header.free_head)?;
        self.pool.install(&mut self.file, id, page)?;
        self.header.free_head = id.0;
        self.header.free_count += 1;
        self.needs_sync = true;
        Ok(())
    }

    /// Number of pages in the file, including the header.
    pub fn page_count(&self) -> u32 {
        self.header.page_count
    }

    /// Persists dirty pages and the header, then fsyncs. A no-op when nothing
    /// changed since the last sync.
    pub fn sync(&mut self) -> Result<()> {
        if !self.needs_sync {
            return Ok(());
        }
        self.pool.flush(&mut self.file)?;
        write_page(&mut self.file, PageId(0), &self.header.encode()?)?;
        self.file
            .set_len(u64::from(self.header.page_count) * PAGE_SIZE as u64)?;
        self.file.sync_all()?;
        self.needs_sync = false;
        Ok(())
    }

    /// Syncs and closes the file, reporting any error `Drop` would swallow.
    pub fn close(mut self) -> Result<()> {
        self.sync()
    }

    /// Keeps `id` resident until a matching `unpin`. Pins nest.
    pub fn pin(&mut self, id: PageId) -> Result<()> {
        self.check_id(id)?;
        self.pool.pin(&mut self.file, id)
    }

    pub fn unpin(&mut self, id: PageId) -> Result<()> {
        self.check_id(id)?;
        self.pool.unpin(id)
    }

    /// The free list, head first. Fails with `Corrupt` if the list is longer
    /// than the header says, cycles, or contains a page that is not free.
    pub fn free_list(&mut self) -> Result<Vec<PageId>> {
        let expected = self.header.free_count as usize;
        let page_count = self.header.page_count;
        let mut list = Vec::with_capacity(expected);
        let mut seen = HashSet::with_capacity(expected);
        let mut next = self.header.free_head;
        while next != 0 {
            if list.len() >= expected || !seen.insert(next) {
                return Err(corrupt_header(
                    "free list is longer than its count or cycles",
                ));
            }
            let id = PageId(next);
            list.push(id);
            next = decode_free_page(self.pool.page(&mut self.file, id)?, id, page_count)?;
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
        let roots = &mut self.header.roots;
        let index = roots
            .iter()
            .position(|slot| slot.as_ref().is_some_and(|s| s.name == name))
            .or_else(|| roots.iter().position(Option::is_none))
            .ok_or(StorageError::RootTableFull)?;
        if let Some(slot) = roots.get_mut(index) {
            *slot = Some(RootSlot {
                name: name.to_owned(),
                root,
            });
        }
        self.needs_sync = true;
        Ok(())
    }

    pub fn root(&self, name: &str) -> Option<PageId> {
        self.header
            .roots
            .iter()
            .flatten()
            .find(|slot| slot.name == name)
            .map(|slot| slot.root)
    }

    /// Removes the root called `name`; returns whether it existed.
    pub fn remove_root(&mut self, name: &str) -> bool {
        let found = self
            .header
            .roots
            .iter_mut()
            .find(|slot| slot.as_ref().is_some_and(|s| s.name == name));
        let Some(slot) = found else { return false };
        *slot = None;
        self.needs_sync = true;
        true
    }

    /// All named roots in slot order.
    pub fn roots(&self) -> Vec<(String, PageId)> {
        self.header
            .roots
            .iter()
            .flatten()
            .map(|slot| (slot.name.clone(), slot.root))
            .collect()
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
        self.pool.stats()
    }

    fn pop_free(&mut self) -> Result<PageId> {
        let id = PageId(self.header.free_head);
        let page_count = self.header.page_count;
        let next = decode_free_page(self.pool.page(&mut self.file, id)?, id, page_count)?;
        let remaining = self
            .header
            .free_count
            .checked_sub(1)
            .ok_or_else(|| corrupt_header("free list count"))?;
        if (next == 0) != (remaining == 0) {
            return Err(corrupt_header("free list count"));
        }
        self.pool.install(&mut self.file, id, Page::zeroed())?;
        self.header.free_head = next;
        self.header.free_count = remaining;
        self.needs_sync = true;
        Ok(id)
    }

    fn check_id(&self, id: PageId) -> Result<()> {
        if id.0 == 0 {
            return Err(StorageError::InvalidPage {
                page: id,
                reason: "page 0 is the header",
            });
        }
        if id.0 >= self.header.page_count {
            return Err(StorageError::InvalidPage {
                page: id,
                reason: "beyond page count",
            });
        }
        Ok(())
    }
}

impl Drop for Pager {
    fn drop(&mut self) {
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
