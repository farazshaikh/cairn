//! The file layer every storage write goes through.
//!
//! [`Vfs`] opens and creates files; [`VfsFile`] reads and writes them at
//! byte offsets. [`OsVfs`] is the real implementation over `std::fs::File`;
//! [`crate::fault::FaultVfs`] is an in-memory test double that can stop or
//! tear writes to simulate crashes.
//!
//! `read_at` returns fewer bytes than requested only at the end of the file.
//! Errors are plain `io::Error`s; the pager converts them to `StorageError::Io`.

use std::fs::{File, TryLockError};
use std::io::{self, ErrorKind, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Opens files. Implementations must be shareable between threads because
/// the shared per-file state keeps a handle to the `Vfs` that opened it.
pub trait Vfs: Send + Sync {
    /// Distinguishes instances in the process-wide registry of open files:
    /// the same path opened through two instances is two different files.
    fn id(&self) -> u64;
    /// Creates a new, empty file; fails with `AlreadyExists` if it exists.
    fn create_new(&self, path: &Path) -> io::Result<Box<dyn VfsFile>>;
    /// Opens an existing file for reading and writing.
    fn open(&self, path: &Path) -> io::Result<Box<dyn VfsFile>>;
    /// Opens a file, creating it empty when missing.
    fn open_or_create(&self, path: &Path) -> io::Result<Box<dyn VfsFile>>;
    fn exists(&self, path: &Path) -> io::Result<bool>;
    /// A stable key for `path`, so different spellings of one file share
    /// state. Fails if the file does not exist.
    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf>;
}

/// An open file addressed by byte offset.
pub trait VfsFile: Send {
    /// Reads up to `buf.len()` bytes at `offset`; returns fewer only at the
    /// end of the file.
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize>;
    /// Writes all of `data` at `offset`, extending the file if needed.
    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()>;
    fn set_len(&mut self, len: u64) -> io::Result<()>;
    /// Current length in bytes.
    fn size(&mut self) -> io::Result<u64>;
    /// Makes every completed write durable (`fsync`).
    fn sync(&mut self) -> io::Result<()>;
    /// Takes an exclusive lock on the file without waiting. `Ok(false)` when
    /// another process, or another open handle to the same file, holds it.
    /// The lock lasts until this handle is dropped.
    fn try_lock(&mut self) -> io::Result<bool>;
}

/// The operating system's file system.
#[derive(Clone, Copy, Debug, Default)]
pub struct OsVfs;

struct OsFile(File);

impl Vfs for OsVfs {
    fn id(&self) -> u64 {
        0
    }

    fn create_new(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        let file = File::options()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)?;
        sync_parent(path);
        Ok(Box::new(OsFile(file)))
    }

    fn open(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        let file = File::options().read(true).write(true).open(path)?;
        Ok(Box::new(OsFile(file)))
    }

    fn open_or_create(&self, path: &Path) -> io::Result<Box<dyn VfsFile>> {
        match self.create_new(path) {
            Err(e) if e.kind() == ErrorKind::AlreadyExists => self.open(path),
            other => other,
        }
    }

    fn exists(&self, path: &Path) -> io::Result<bool> {
        path.try_exists()
    }

    fn canonicalize(&self, path: &Path) -> io::Result<PathBuf> {
        std::fs::canonicalize(path)
    }
}

/// Syncing a new file does not make its directory entry durable, so the
/// parent directory is synced too where the platform allows opening it.
fn sync_parent(path: &Path) {
    if !cfg!(unix) {
        return;
    }
    let parent = match path.parent() {
        Some(p) if !p.as_os_str().is_empty() => p,
        _ => Path::new("."),
    };
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
}

impl VfsFile for OsFile {
    fn read_at(&mut self, offset: u64, buf: &mut [u8]) -> io::Result<usize> {
        self.0.seek(SeekFrom::Start(offset))?;
        let mut filled = 0;
        while let Some(rest) = buf.get_mut(filled..) {
            if rest.is_empty() {
                break;
            }
            match self.0.read(rest) {
                Ok(0) => break,
                Ok(n) => filled += n,
                Err(e) if e.kind() == ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(filled)
    }

    fn write_at(&mut self, offset: u64, data: &[u8]) -> io::Result<()> {
        self.0.seek(SeekFrom::Start(offset))?;
        self.0.write_all(data)
    }

    fn set_len(&mut self, len: u64) -> io::Result<()> {
        self.0.set_len(len)
    }

    fn size(&mut self) -> io::Result<u64> {
        Ok(self.0.metadata()?.len())
    }

    fn sync(&mut self) -> io::Result<()> {
        self.0.sync_all()
    }

    fn try_lock(&mut self) -> io::Result<bool> {
        match self.0.try_lock() {
            Ok(()) => Ok(true),
            Err(TryLockError::WouldBlock) => Ok(false),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

/// The write-ahead log path for a database path: the same name plus `-wal`.
pub(crate) fn wal_path(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_common::TempPath;

    #[test]
    fn os_file_round_trips_and_reads_short_at_the_end() -> io::Result<()> {
        let tmp = TempPath::new("vfs");
        let mut file = OsVfs.create_new(tmp.path())?;
        file.write_at(3, b"abc")?;
        assert_eq!(file.size()?, 6);
        let mut buf = [9u8; 8];
        assert_eq!(file.read_at(0, &mut buf)?, 6);
        assert_eq!(&buf[..6], b"\0\0\0abc");
        file.set_len(4)?;
        file.sync()?;
        assert_eq!(file.size()?, 4);
        assert!(OsVfs.create_new(tmp.path()).is_err());
        assert!(OsVfs.exists(tmp.path())?);
        Ok(())
    }

    #[test]
    fn wal_path_appends_the_suffix() {
        assert_eq!(wal_path(Path::new("/d/x.db")), PathBuf::from("/d/x.db-wal"));
    }
}
