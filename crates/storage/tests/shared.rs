//! Several handles on one file (milestone M4 AC3): committed-only reads,
//! one writer with `Busy`, reader snapshots that hold back checkpoints, and
//! one shared state per file.

mod common;

use std::path::Path;
use std::sync::Arc;

use cairn_storage::fault::FaultVfs;
use cairn_storage::{Options, Page, PageId, Pager, StorageError};
use common::TempPath;

type TestResult = Result<(), StorageError>;

const DB: &str = "/shared.db";

fn filled(byte: u8) -> Page {
    let mut page = Page::zeroed();
    page.bytes_mut().fill(byte);
    page
}

fn pair() -> Result<(FaultVfs, Pager, Pager), StorageError> {
    let vfs = FaultVfs::new();
    let options = Options::new(8, 1000);
    let mut a = Pager::create_with(Arc::new(vfs.clone()), Path::new(DB), options)?;
    for byte in 1..=4 {
        let id = a.allocate()?;
        a.write(id, &filled(byte))?;
    }
    a.set_root("r", PageId(1))?;
    a.commit()?;
    let b = Pager::open_with(Arc::new(vfs.clone()), Path::new(DB), options)?;
    Ok((vfs, a, b))
}

fn busy<T>(result: Result<T, StorageError>) -> bool {
    matches!(result, Err(StorageError::Busy { .. }))
}

#[test]
fn uncommitted_changes_are_invisible_to_other_handles() -> TestResult {
    let (_vfs, mut a, mut b) = pair()?;
    assert_eq!(b.read(PageId(2))?, filled(2), "b sees a's earlier commit");
    a.begin()?;
    a.write(PageId(2), &filled(0xA2))?;
    let grown = a.allocate()?;
    a.set_root("r", PageId(3))?;
    a.set_root("new", grown)?;
    assert_eq!(b.read(PageId(2))?, filled(2));
    assert_eq!(b.page_count(), 5);
    assert_eq!(b.roots(), vec![("r".to_string(), PageId(1))]);
    a.rollback()?;
    assert_eq!(b.read(PageId(2))?, filled(2));

    a.begin()?;
    a.write(PageId(2), &filled(0xB2))?;
    a.allocate()?;
    a.commit()?;
    assert_eq!(
        b.read(PageId(2))?,
        filled(0xB2),
        "the commit is visible without reopening"
    );
    assert_eq!(b.page_count(), 6);
    a.close()?;
    b.close()
}

#[test]
fn one_writer_at_a_time() -> TestResult {
    let (_vfs, mut a, mut b) = pair()?;
    a.begin()?;
    assert!(busy(b.begin()));
    assert!(
        busy(b.write(PageId(1), &filled(9))),
        "implicit transactions are writers too"
    );
    assert!(busy(b.allocate()));
    a.commit()?;
    b.begin()?;
    assert!(busy(a.begin()));
    b.rollback()?;
    a.begin()?;
    a.write(PageId(1), &filled(7))?;
    drop(a);
    b.begin()?;
    assert_eq!(
        b.read(PageId(1))?,
        filled(1),
        "dropping an explicit transaction discards it"
    );
    b.commit()?;
    b.close()
}

#[test]
fn registered_readers_keep_their_snapshot_and_hold_back_checkpoints() -> TestResult {
    let (_vfs, mut a, mut b) = pair()?;
    b.begin_read()?;
    a.write(PageId(1), &filled(0x11))?;
    a.commit()?;
    assert_eq!(
        b.read(PageId(1))?,
        filled(1),
        "a registered reader keeps its snapshot"
    );
    assert!(busy(a.checkpoint()));
    a.sync()?;
    assert!(
        a.log_frames() > 0,
        "sync skips the checkpoint while a reader is active"
    );
    b.end_read();
    assert_eq!(b.read(PageId(1))?, filled(0x11));
    assert!(a.checkpoint()? > 0);
    assert_eq!(
        b.read(PageId(1))?,
        filled(0x11),
        "a checkpoint changes no contents"
    );
    a.close()?;
    b.close()
}

#[test]
fn a_checkpoint_runs_under_another_handles_open_transaction() -> TestResult {
    let (_vfs, mut a, mut b) = pair()?;
    a.begin()?;
    a.write(PageId(3), &filled(0x33))?;
    assert!(b.checkpoint()? > 0);
    assert_eq!(a.read(PageId(4))?, filled(4));
    a.commit()?;
    assert_eq!(b.read(PageId(3))?, filled(0x33));
    a.close()?;
    b.close()
}

#[test]
fn spellings_of_one_path_share_state_and_handles_cross_threads() -> TestResult {
    let tmp = TempPath::new("shared");
    let mut a = Pager::create(tmp.path())?;
    a.allocate()?;
    a.commit()?;
    let dir = tmp.path().parent().expect("parent").to_path_buf();
    let dotted = dir.join(".").join(tmp.path().file_name().expect("name"));
    let mut b = Pager::open(&dotted)?;
    a.begin()?;
    a.write(PageId(1), &filled(5))?;
    let worker = std::thread::spawn(move || -> TestResult {
        assert!(busy(b.begin()), "same file through another spelling");
        assert_eq!(b.read(PageId(1))?, Page::zeroed());
        Ok(())
    });
    worker.join().expect("thread")?;
    a.commit()?;
    let mut c = Pager::open(&dotted)?;
    assert_eq!(c.read(PageId(1))?, filled(5));
    c.close()?;
    a.close()
}
