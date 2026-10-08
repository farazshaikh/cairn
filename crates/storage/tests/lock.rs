//! The exclusive per-process operating-system lock (milestone M5 AC7).
//!
//! Another process is simulated by a second, independent `std::fs::File`
//! on the same path: `flock` and `LockFileEx` locks belong to an open file
//! description, so two separate opens conflict even within one process.

mod common;

use std::fs::{self, File, TryLockError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

use cairn_storage::fault::FaultVfs;
use cairn_storage::{Options, OsVfs, Page, PageId, Pager, StorageError};
use common::TempPath;

const LOCKED: &str = "the file is locked by another process";

fn wal(path: &Path) -> PathBuf {
    let mut name = path.as_os_str().to_owned();
    name.push("-wal");
    PathBuf::from(name)
}

/// Holds the lock the way another process would.
fn foreign_lock(path: &Path) -> File {
    let file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .expect("open raw");
    file.try_lock().expect("foreign lock");
    file
}

fn is_locked(path: &Path) -> bool {
    let file = File::options()
        .read(true)
        .write(true)
        .open(path)
        .expect("open raw");
    matches!(file.try_lock(), Err(TryLockError::WouldBlock))
}

fn assert_locked_error<T>(result: Result<T, StorageError>) {
    match result {
        Err(StorageError::Busy { reason }) => assert_eq!(reason, LOCKED),
        Err(other) => panic!("expected Busy, got {other}"),
        Ok(_) => panic!("expected Busy, the open succeeded"),
    }
}

#[test]
fn open_fails_with_busy_while_another_holder_has_the_lock() {
    let tmp = TempPath::new("lock-open");
    Pager::create(tmp.path())
        .expect("create")
        .close()
        .expect("close");
    let holder = foreign_lock(tmp.path());
    assert_locked_error(Pager::open(tmp.path()));
    drop(holder);
    Pager::open(tmp.path())
        .expect("open after release")
        .close()
        .expect("close");
}

#[test]
fn a_locked_open_touches_neither_the_file_nor_its_log() {
    let src = TempPath::new("lock-src");
    let copy = TempPath::new("lock-copy");
    let options = Options::new(8, 1_000_000);
    let mut pager =
        Pager::create_with(Arc::new(OsVfs), src.path(), options).expect("create source");
    let id = pager.allocate().expect("allocate");
    let mut page = Page::zeroed();
    page.bytes_mut().fill(7);
    pager.write(id, &page).expect("write");
    pager.set_root("r", PageId(1)).expect("root");
    pager.commit().expect("commit");
    fs::copy(src.path(), copy.path()).expect("copy main");
    fs::copy(wal(src.path()), wal(copy.path())).expect("copy log");
    let main_before = fs::read(copy.path()).expect("read main");
    let log_before = fs::read(wal(copy.path())).expect("read log");
    assert!(!log_before.is_empty(), "the copy needs a log to replay");

    let holder = foreign_lock(copy.path());
    assert_locked_error(Pager::open(copy.path()));
    drop(holder);
    assert_eq!(fs::read(copy.path()).expect("main"), main_before);
    assert_eq!(fs::read(wal(copy.path())).expect("log"), log_before);

    Pager::open(copy.path())
        .expect("open after release")
        .close()
        .expect("close");
    assert!(
        fs::read(wal(copy.path())).expect("log").is_empty(),
        "the unlocked open recovered, so the locked one did not"
    );
    drop(pager);
}

#[test]
fn handles_in_one_process_share_the_lock_until_the_last_closes() {
    let tmp = TempPath::new("lock-share");
    let first = Pager::create(tmp.path()).expect("create");
    let second = Pager::open(tmp.path()).expect("second handle in this process");
    assert!(is_locked(tmp.path()));
    first.close().expect("close first");
    assert!(is_locked(tmp.path()), "the second handle still holds it");
    second.close().expect("close second");
    assert!(!is_locked(tmp.path()), "released with the last handle");
}

#[test]
fn create_takes_the_lock() {
    let tmp = TempPath::new("lock-create");
    let pager = Pager::create(tmp.path()).expect("create");
    assert!(is_locked(tmp.path()));
    drop(pager);
    assert!(!is_locked(tmp.path()));
}

#[test]
fn the_fault_layer_always_grants_the_lock() {
    let vfs = Arc::new(FaultVfs::new());
    let path = Path::new("/lock.db");
    let first = Pager::create_with(vfs.clone(), path, Options::default()).expect("create");
    let second = Pager::open_with(vfs, path, Options::default()).expect("open");
    drop(second);
    drop(first);
}
