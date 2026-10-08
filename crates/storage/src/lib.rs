//! Page file, buffer pool and B-tree storage for cairn.
//!
//! A database is one file of fixed 4096-byte pages. Page 0 is the header;
//! every other page is a B-tree node or a free page. All on-disk integers are
//! little-endian and written field by field (no `unsafe`, no serialization
//! crates). Decoding never panics: malformed bytes surface as
//! [`StorageError`], with `Corrupt { page, .. }` naming the bad page.
//!
//! Each byte layout is documented as an offset table at the top of the module
//! that encodes it, and the repository README collects them:
//!
//! - `header.rs`: page 0 (magic `cairn\0`, version, page count, free list
//!   head and length, 16 named root slots);
//! - `pager.rs`: free pages, which form a LIFO list through their own bytes;
//! - `node.rs`: B-tree leaf and internal nodes, size limits and fill bounds.
//!
//! [`Pager`] owns a handle on the file: its transaction overlay and a
//! bounded LRU buffer pool of committed pages, over state shared by every
//! handle on the same file in the process. Every change goes through the
//! write-ahead log `<file>-wal` (format in `wal.rs`); see `pager.rs` for the
//! transaction, checkpoint and recovery rules. [`BTree`] is a handle to a
//! tree rooted at a page; its methods borrow the pager.
//!
//! All file access goes through the [`Vfs`] trait. [`OsVfs`] is the real
//! file system; [`fault::FaultVfs`] is an in-memory double that injects
//! crashes for tests.

#![warn(missing_docs)]
#![cfg_attr(
    not(test),
    deny(
        clippy::indexing_slicing,
        clippy::unwrap_used,
        clippy::expect_used,
        clippy::panic
    )
)]

mod btree;
mod crc;
mod error;
pub mod fault;
mod header;
mod node;
mod page;
mod pager;
mod pool;
mod shared;
mod vfs;
mod wal;

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_common;

pub use btree::{BTree, Range};
pub use error::{Result, SizeKind, StorageError};
pub use header::{FORMAT_VERSION, MAGIC, MAX_ROOT_NAME_LEN, MAX_ROOTS};
pub use node::{MAX_KEY_LEN, MAX_VALUE_LEN};
pub use page::{PAGE_SIZE, Page, PageId};
pub use pager::{DEFAULT_CHECKPOINT_FRAMES, Options, Pager, Savepoint};
pub use pool::{DEFAULT_POOL_PAGES, MIN_POOL_PAGES, PoolStats};
pub use vfs::{OsVfs, Vfs, VfsFile};
