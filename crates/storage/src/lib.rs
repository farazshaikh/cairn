//! Page file, buffer pool and B-tree storage for cairn.
//!
//! A database is one file of fixed 4096-byte pages. Page 0 is the header
//! (see [`header`](crate#header-page)); every other page is a B-tree node or
//! a free page. All on-disk integers are little-endian, and every layout is
//! documented next to the code that encodes it. Decoding never panics:
//! malformed bytes surface as [`StorageError`].
//!
//! # Header page
//!
//! The byte layout of page 0 is documented in the `header` module source and
//! summarised in the repository README.

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
mod error;
mod header;
mod node;
mod page;
mod pager;
mod pool;

#[cfg(test)]
#[path = "../tests/common/mod.rs"]
mod test_common;

pub use btree::{BTree, Range};
pub use error::{Result, SizeKind, StorageError};
pub use header::{FORMAT_VERSION, MAGIC, MAX_ROOT_NAME_LEN, MAX_ROOTS};
pub use node::{MAX_KEY_LEN, MAX_VALUE_LEN};
pub use page::{PAGE_SIZE, Page, PageId};
pub use pager::Pager;
pub use pool::{DEFAULT_POOL_PAGES, MIN_POOL_PAGES, PoolStats};
