//! Pager transactions (milestone M4 AC1, AC2): commit through the log,
//! rollback, savepoints, and no uncommitted page in the main file.

mod common;

use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use cairn_storage::fault::{Event, FaultVfs, Op};
use cairn_storage::{BTree, Options, PAGE_SIZE, Page, PageId, Pager, StorageError};
use common::{SEED, XorShift64};

type TestResult = Result<(), StorageError>;

const DB: &str = "/txn.db";
const WAL: &str = "/txn.db-wal";

fn filled(byte: u8) -> Page {
    let mut page = Page::zeroed();
    page.bytes_mut().fill(byte);
    page
}

fn create(vfs: &FaultVfs, pool: usize) -> Result<Pager, StorageError> {
    Pager::create_with(
        Arc::new(vfs.clone()),
        Path::new(DB),
        Options::new(pool, 1000),
    )
}

fn main_bytes(vfs: &FaultVfs) -> Vec<u8> {
    vfs.read_file(Path::new(DB)).expect("main file")
}

/// AC1's ordering rule on a recorded run: every write to the main file comes
/// after a log sync with no log write in between, so the commit record of
/// every page copied is durable. Writes before the log first appears are
/// `create` writing the initial header.
pub fn assert_main_writes_follow_log_syncs(events: &[Event]) {
    let mut log_seen = false;
    let mut log_dirty = false;
    for (i, event) in events.iter().enumerate() {
        let is_log = event.path == Path::new(WAL);
        match (is_log, event.op) {
            (true, Op::Sync) => log_dirty = false,
            (true, _) => {
                log_seen = true;
                log_dirty = true;
            }
            (false, Op::Write { .. } | Op::SetLen(_)) if log_seen => {
                assert!(
                    !log_dirty,
                    "event {i} writes the main file before the log is synced: {event:?}"
                );
            }
            _ => {}
        }
    }
    assert!(log_seen, "the run used the log");
}

#[test]
fn main_file_writes_happen_only_after_the_log_is_synced() -> TestResult {
    let vfs = FaultVfs::new();
    let mut pager = Pager::create_with(Arc::new(vfs.clone()), Path::new(DB), Options::new(8, 20))?;
    let mut tree = BTree::open(BTree::create(&mut pager)?);
    pager.set_root("t", tree.root())?;
    pager.commit()?;
    let mut rng = XorShift64::new(SEED);
    for round in 0..60 {
        for _ in 0..1 + rng.below(30) {
            let key = (rng.below(2000) as u32).to_be_bytes();
            if rng.below(4) == 0 {
                tree.delete(&mut pager, &key)?;
            } else {
                tree.insert(&mut pager, &key, &vec![round as u8; 1 + rng.below(300)])?;
            }
        }
        pager.set_root("t", tree.root())?;
        pager.commit()?;
    }
    pager.close()?;
    assert!(
        vfs.events()
            .iter()
            .filter(|e| e.op == Op::SetLen(0))
            .count()
            >= 3,
        "several checkpoints ran"
    );
    assert_main_writes_follow_log_syncs(&vfs.events());
    Ok(())
}

#[test]
fn a_large_transaction_leaves_the_main_file_untouched_until_checkpoint() -> TestResult {
    let vfs = FaultVfs::new();
    let mut pager = create(&vfs, 8)?;
    let before = main_bytes(&vfs);
    let ids: Vec<PageId> = (0..50)
        .map(|_| pager.allocate())
        .collect::<Result<_, _>>()?;
    for &id in &ids {
        pager.write(id, &filled(id.0 as u8))?;
    }
    for &id in &ids {
        assert_eq!(pager.read(id)?, filled(id.0 as u8));
    }
    assert_eq!(
        main_bytes(&vfs),
        before,
        "uncommitted pages reached the main file"
    );
    pager.commit()?;
    assert_eq!(main_bytes(&vfs), before, "commit alone writes only the log");
    assert_eq!(pager.log_frames(), 51, "50 pages and the header");
    assert_eq!(
        pager.read(PageId(7))?,
        filled(7),
        "committed pages are read from the log"
    );
    assert_eq!(pager.checkpoint()?, 51);
    let after = main_bytes(&vfs);
    assert_eq!(after.len(), 51 * PAGE_SIZE);
    assert!(after[7 * PAGE_SIZE..8 * PAGE_SIZE].iter().all(|&b| b == 7));
    assert_eq!(vfs.read_file(Path::new(WAL)).map(|b| b.len()), Some(0));
    pager.close()
}

#[test]
fn rollback_restores_pages_header_free_list_and_roots() -> TestResult {
    let vfs = FaultVfs::new();
    let mut pager = create(&vfs, 8)?;
    for _ in 0..6 {
        let id = pager.allocate()?;
        pager.write(id, &filled(id.0 as u8))?;
    }
    pager.free(PageId(2))?;
    pager.set_root("keep", PageId(3))?;
    pager.commit()?;
    let main_before = main_bytes(&vfs);
    let log_before = vfs.read_file(Path::new(WAL));

    pager.begin()?;
    pager.write(PageId(1), &filled(0xAA))?;
    let reused = pager.allocate()?;
    assert_eq!(reused, PageId(2));
    pager.allocate()?;
    pager.free(PageId(4))?;
    pager.set_root("keep", PageId(5))?;
    pager.set_root("new", PageId(6))?;
    assert!(pager.remove_root("keep")?);
    pager.rollback()?;

    assert_eq!(pager.read(PageId(1))?, filled(1));
    assert_eq!(pager.page_count(), 7);
    assert_eq!(pager.free_list()?, vec![PageId(2)]);
    assert_eq!(pager.roots(), vec![("keep".to_string(), PageId(3))]);
    assert_eq!(main_bytes(&vfs), main_before);
    assert_eq!(
        vfs.read_file(Path::new(WAL)),
        log_before,
        "rollback writes nothing"
    );
    assert!(matches!(pager.rollback(), Err(StorageError::NoTransaction)));
    assert!(matches!(pager.commit(), Err(StorageError::NoTransaction)));
    pager.close()
}

#[test]
fn savepoints_undo_and_release_nested_changes() -> TestResult {
    let vfs = FaultVfs::new();
    let mut pager = create(&vfs, 8)?;
    for _ in 0..4 {
        pager.allocate()?;
    }
    pager.commit()?;
    assert!(matches!(
        pager.savepoint(),
        Err(StorageError::NoTransaction)
    ));

    pager.begin()?;
    pager.write(PageId(1), &filled(1))?;
    let outer = pager.savepoint()?;
    pager.write(PageId(1), &filled(2))?;
    pager.write(PageId(2), &filled(2))?;
    let inner = pager.savepoint()?;
    pager.write(PageId(3), &filled(3))?;
    let grown = pager.allocate()?;
    pager.release(inner)?;
    assert!(matches!(
        pager.rollback_to(inner),
        Err(StorageError::InvalidSavepoint)
    ));
    pager.rollback_to(outer)?;
    assert_eq!(
        pager.read(PageId(1))?,
        filled(1),
        "change before the savepoint stays"
    );
    assert_eq!(pager.read(PageId(2))?, Page::zeroed());
    assert_eq!(
        pager.read(PageId(3))?,
        Page::zeroed(),
        "released level undone by its parent"
    );
    assert_eq!(pager.page_count(), grown.0, "allocation undone");

    let again = pager.savepoint()?;
    pager.write(PageId(4), &filled(4))?;
    pager.release(again)?;
    pager.commit()?;
    let reopened = vfs.crash(cairn_storage::fault::CrashMode::SyncedOnly);
    drop(pager);
    let mut pager = Pager::open_with(Arc::new(reopened), Path::new(DB), Options::new(8, 1000))?;
    assert_eq!(pager.read(PageId(1))?, filled(1));
    assert_eq!(pager.read(PageId(4))?, filled(4));
    assert_eq!(pager.read(PageId(3))?, Page::zeroed());
    assert_eq!(pager.page_count(), 5);
    pager.close()
}

#[test]
fn empty_commits_do_no_io_and_begin_rules_hold() -> TestResult {
    let vfs = FaultVfs::new();
    let mut pager = create(&vfs, 8)?;
    let events = vfs.events().len();
    pager.begin()?;
    assert!(matches!(pager.begin(), Err(StorageError::TransactionOpen)));
    assert!(matches!(
        pager.checkpoint(),
        Err(StorageError::TransactionOpen)
    ));
    pager.commit()?;
    assert!(!pager.in_transaction());
    assert_eq!(vfs.events().len(), events, "an empty commit writes nothing");
    pager.sync()?;
    assert_eq!(
        vfs.events().len(),
        events,
        "sync with nothing to do writes nothing"
    );
    pager.close()
}

#[test]
fn transactional_btree_matches_a_model_with_random_rollbacks() -> TestResult {
    let vfs = FaultVfs::new();
    let mut pager = Pager::create_with(Arc::new(vfs.clone()), Path::new(DB), Options::new(8, 64))?;
    let root = BTree::create(&mut pager)?;
    pager.set_root("t", root)?;
    pager.commit()?;
    let mut model: BTreeMap<Vec<u8>, Vec<u8>> = BTreeMap::new();
    let mut rng = XorShift64::new(SEED ^ 0xABCD);
    for round in 0..150 {
        let mut scratch = model.clone();
        pager.begin()?;
        let mut tree = BTree::open(pager.root("t").expect("root"));
        for _ in 0..1 + rng.below(50) {
            let key = format!("k{:05}", rng.below(3000)).into_bytes();
            if rng.below(3) == 0 {
                tree.delete(&mut pager, &key)?;
                scratch.remove(&key);
            } else {
                let value = vec![round as u8; rng.below(400)];
                tree.insert(&mut pager, &key, &value)?;
                scratch.insert(key, value);
            }
        }
        pager.set_root("t", tree.root())?;
        if rng.below(4) == 0 {
            pager.rollback()?;
        } else {
            pager.commit()?;
            model = scratch;
        }
        let tree = BTree::open(pager.root("t").expect("root"));
        if let Err(problem) = tree.check(&mut pager) {
            panic!("round {round}: {problem}");
        }
        if round % 25 == 0 {
            let stored: Vec<(Vec<u8>, Vec<u8>)> = tree
                .range(
                    &mut pager,
                    std::ops::Bound::Unbounded,
                    std::ops::Bound::Unbounded,
                )?
                .collect::<Result<_, _>>()?;
            let expected: Vec<(Vec<u8>, Vec<u8>)> = model.clone().into_iter().collect();
            assert_eq!(stored, expected, "round {round}");
        }
    }
    pager.close()?;
    assert_main_writes_follow_log_syncs(&vfs.events());
    Ok(())
}
