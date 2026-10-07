//! The error type shared by every storage operation.

use std::fmt;
use std::io;

use crate::header::FORMAT_VERSION;
use crate::page::{PAGE_SIZE, PageId};

/// Which half of a key/value pair exceeded its size limit.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SizeKind {
    Key,
    Value,
}

impl fmt::Display for SizeKind {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            SizeKind::Key => f.write_str("key"),
            SizeKind::Value => f.write_str("value"),
        }
    }
}

/// Every failure the storage layer reports. Malformed input never panics; it
/// surfaces as one of these variants.
#[derive(Debug)]
pub enum StorageError {
    /// The operating system reported an I/O failure.
    Io(io::Error),
    /// The file is shorter than one page, so it cannot hold a header.
    FileTooShort { len: u64 },
    /// The file length is not a whole number of pages.
    NotPageMultiple { len: u64 },
    /// Page 0 does not start with `cairn\0`.
    BadMagic { found: [u8; 6] },
    /// The header names a format version this build cannot read.
    UnsupportedVersion { found: u32 },
    /// A page failed structural validation while being decoded.
    Corrupt { page: PageId, reason: &'static str },
    /// Every buffer pool frame is pinned, so no page can be brought in.
    PoolExhausted,
    /// The requested buffer pool capacity is below the minimum.
    PoolTooSmall { requested: usize, min: usize },
    /// A key or value exceeds its size limit.
    TooLarge {
        kind: SizeKind,
        len: usize,
        max: usize,
    },
    /// A page id is not valid for the requested operation.
    InvalidPage { page: PageId, reason: &'static str },
    /// The 32-bit page id space is exhausted.
    DatabaseFull,
    /// A root name is empty or longer than 32 bytes.
    InvalidRootName,
    /// All 16 named root slots are in use.
    RootTableFull,
    /// Another handle holds the write lock, or readers block a checkpoint.
    Busy { reason: &'static str },
    /// `commit`, `rollback` or `savepoint` was called with no transaction.
    NoTransaction,
    /// `begin` or `checkpoint` was called while this handle has a
    /// transaction open.
    TransactionOpen,
    /// The savepoint was already released or rolled back.
    InvalidSavepoint,
    /// A sync of the log failed, so what is durable is unknown. The shared
    /// file refuses every operation until all handles close and it is
    /// reopened, which replays the log.
    Unusable,
}

/// Result alias used throughout the crate.
pub type Result<T> = std::result::Result<T, StorageError>;

impl fmt::Display for StorageError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            StorageError::Io(e) => write!(f, "I/O error: {e}"),
            StorageError::FileTooShort { len } => {
                write!(f, "file too short: {len} bytes, need at least {PAGE_SIZE}")
            }
            StorageError::NotPageMultiple { len } => write!(
                f,
                "file length {len} is not a multiple of the {PAGE_SIZE}-byte page size"
            ),
            StorageError::BadMagic { found } => {
                write!(f, "bad magic {found:02x?}: not a cairn database")
            }
            StorageError::UnsupportedVersion { found } => write!(
                f,
                "unsupported format version {found} (supported: {FORMAT_VERSION})"
            ),
            StorageError::Corrupt { page, reason } => write!(f, "corrupt page {page}: {reason}"),
            StorageError::PoolExhausted => {
                f.write_str("buffer pool exhausted: every frame is pinned")
            }
            StorageError::PoolTooSmall { requested, min } => write!(
                f,
                "buffer pool capacity {requested} is below the minimum {min}"
            ),
            StorageError::TooLarge { kind, len, max } => {
                write!(f, "{kind} too large: {len} bytes, maximum {max}")
            }
            StorageError::InvalidPage { page, reason } => {
                write!(f, "invalid page {page}: {reason}")
            }
            StorageError::DatabaseFull => f.write_str("database full: page id space exhausted"),
            StorageError::InvalidRootName => f.write_str("root name must be 1 to 32 bytes"),
            StorageError::RootTableFull => f.write_str("root table full: at most 16 named roots"),
            StorageError::Busy { reason } => write!(f, "database is busy: {reason}"),
            StorageError::NoTransaction => f.write_str("no transaction is open"),
            StorageError::TransactionOpen => {
                f.write_str("a transaction is already open on this handle")
            }
            StorageError::InvalidSavepoint => f.write_str("savepoint does not exist"),
            StorageError::Unusable => f.write_str(
                "database file is unusable after a failed sync; close every handle and reopen",
            ),
        }
    }
}

impl std::error::Error for StorageError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            StorageError::Io(e) => Some(e),
            _ => None,
        }
    }
}

impl From<io::Error> for StorageError {
    fn from(e: io::Error) -> StorageError {
        StorageError::Io(e)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn display_names_each_problem() {
        let cases: Vec<(StorageError, &str)> = vec![
            (
                StorageError::Io(io::Error::other("disk on fire")),
                "I/O error: disk on fire",
            ),
            (
                StorageError::FileTooShort { len: 10 },
                "file too short: 10 bytes",
            ),
            (
                StorageError::NotPageMultiple { len: 4097 },
                "4097 is not a multiple",
            ),
            (StorageError::BadMagic { found: *b"abcdef" }, "bad magic"),
            (
                StorageError::UnsupportedVersion { found: 7 },
                "unsupported format version 7",
            ),
            (
                StorageError::Corrupt {
                    page: PageId(9),
                    reason: "node tag",
                },
                "corrupt page 9: node tag",
            ),
            (StorageError::PoolExhausted, "buffer pool exhausted"),
            (
                StorageError::PoolTooSmall {
                    requested: 3,
                    min: 8,
                },
                "capacity 3 is below the minimum 8",
            ),
            (
                StorageError::TooLarge {
                    kind: SizeKind::Key,
                    len: 257,
                    max: 256,
                },
                "key too large: 257 bytes, maximum 256",
            ),
            (
                StorageError::TooLarge {
                    kind: SizeKind::Value,
                    len: 1025,
                    max: 1024,
                },
                "value too large: 1025 bytes",
            ),
            (
                StorageError::InvalidPage {
                    page: PageId(0),
                    reason: "page 0 is the header",
                },
                "invalid page 0: page 0 is the header",
            ),
            (StorageError::DatabaseFull, "database full"),
            (
                StorageError::InvalidRootName,
                "root name must be 1 to 32 bytes",
            ),
            (StorageError::RootTableFull, "root table full"),
            (
                StorageError::Busy {
                    reason: "another handle has an open write transaction",
                },
                "database is busy: another handle has an open write transaction",
            ),
            (StorageError::NoTransaction, "no transaction is open"),
            (StorageError::TransactionOpen, "already open on this handle"),
            (StorageError::InvalidSavepoint, "savepoint does not exist"),
            (StorageError::Unusable, "unusable after a failed sync"),
        ];
        for (error, expected) in cases {
            let text = error.to_string();
            assert!(
                text.contains(expected),
                "{text:?} does not contain {expected:?}"
            );
        }
    }

    #[test]
    fn io_errors_convert_and_expose_source() {
        let error = StorageError::from(io::Error::other("boom"));
        assert!(matches!(error, StorageError::Io(_)));
        assert!(std::error::Error::source(&error).is_some());
        assert!(std::error::Error::source(&StorageError::PoolExhausted).is_none());
    }
}
