//! Buffer pool behaviour: bounded capacity, LRU eviction, dirty write-back,
//! pinning and `PoolExhausted` (milestone AC3, AC7).

mod common;

use cairn_storage::{PAGE_SIZE, Page, PageId, Pager, StorageError};
use common::{SEED, TempPath, XorShift64, read_raw};

type TestResult = Result<(), StorageError>;

const CAP: usize = 8;

fn filled(byte: u8) -> Page {
    let mut page = Page::zeroed();
    page.bytes_mut().fill(byte);
    page
}

/// Creates a file whose pages 1..=n are filled with their own id and returns
/// it reopened with an empty pool of capacity 8.
fn setup(tmp: &TempPath, n: u8) -> Result<Pager, StorageError> {
    let mut pager = Pager::create_with_capacity(tmp.path(), CAP)?;
    for byte in 1..=n {
        let id = pager.allocate()?;
        pager.write(id, &filled(byte))?;
    }
    pager.close()?;
    Pager::open_with_capacity(tmp.path(), CAP)
}

fn read_ids(pager: &mut Pager, ids: impl IntoIterator<Item = u32>) -> TestResult {
    for id in ids {
        assert_eq!(
            pager.read(PageId(id))?,
            filled(id as u8),
            "page {id} contents"
        );
    }
    Ok(())
}

#[test]
fn capacity_below_minimum_rejected() -> TestResult {
    let tmp = TempPath::new("cap");
    assert!(matches!(
        Pager::create_with_capacity(tmp.path(), 7),
        Err(StorageError::PoolTooSmall {
            requested: 7,
            min: 8
        })
    ));
    assert!(
        !tmp.path().exists(),
        "no file is created for a rejected capacity"
    );
    Pager::create_with_capacity(tmp.path(), 8)?.close()?;
    assert!(matches!(
        Pager::open_with_capacity(tmp.path(), 0),
        Err(StorageError::PoolTooSmall { requested: 0, .. })
    ));
    assert_eq!(Pager::open_with_capacity(tmp.path(), 8)?.pool_capacity(), 8);
    Ok(())
}

#[test]
fn lru_evicts_least_recently_used() -> TestResult {
    let tmp = TempPath::new("lru");
    let mut pager = setup(&tmp, 9)?;
    read_ids(&mut pager, 1..=8)?;
    read_ids(&mut pager, [1])?;
    read_ids(&mut pager, [9])?;
    assert!(
        !pager.is_cached(PageId(2)),
        "page 2 was least recently used"
    );
    assert!(pager.is_cached(PageId(1)), "page 1 was re-touched");
    assert!(pager.is_cached(PageId(9)));
    let stats = pager.stats();
    assert_eq!(stats.evictions, 1);
    assert_eq!(stats.hits, 1);
    assert_eq!(stats.misses, 9);
    pager.close()
}

#[test]
fn dirty_page_written_back_on_eviction() -> TestResult {
    let tmp = TempPath::new("writeback");
    let mut pager = setup(&tmp, 9)?;
    pager.write(PageId(1), &filled(0xEE))?;
    read_ids(&mut pager, 2..=9)?;
    assert!(!pager.is_cached(PageId(1)));
    assert_eq!(pager.stats().page_writes, 1);
    let raw = read_raw(tmp.path());
    assert!(
        raw[PAGE_SIZE..2 * PAGE_SIZE].iter().all(|&b| b == 0xEE),
        "evicted dirty page reached the file before any sync"
    );
    assert_eq!(pager.read(PageId(1))?, filled(0xEE));
    pager.close()
}

#[test]
fn sync_writes_only_dirty_pages() -> TestResult {
    let tmp = TempPath::new("syncdirty");
    let mut pager = setup(&tmp, 8)?;
    read_ids(&mut pager, 1..=8)?;
    pager.write(PageId(3), &filled(0x33))?;
    pager.write(PageId(5), &filled(0x55))?;
    let before = pager.stats().page_writes;
    pager.sync()?;
    assert_eq!(pager.stats().page_writes - before, 2);
    pager.sync()?;
    assert_eq!(
        pager.stats().page_writes - before,
        2,
        "second sync writes nothing"
    );
    drop(pager);
    let mut pager = Pager::open_with_capacity(tmp.path(), CAP)?;
    assert_eq!(pager.read(PageId(3))?, filled(0x33));
    assert_eq!(pager.read(PageId(5))?, filled(0x55));
    read_ids(&mut pager, [1, 2, 4, 6, 7, 8])?;
    pager.close()
}

#[test]
fn pinned_page_survives_eviction_pressure() -> TestResult {
    let tmp = TempPath::new("pinned");
    let mut pager = setup(&tmp, 20)?;
    pager.pin(PageId(1))?;
    read_ids(&mut pager, 2..=20)?;
    assert!(
        pager.is_cached(PageId(1)),
        "pinned LRU page is never evicted"
    );
    pager.unpin(PageId(1))?;
    read_ids(&mut pager, 2..=9)?;
    assert!(
        !pager.is_cached(PageId(1)),
        "unpinned page becomes evictable again"
    );
    pager.close()
}

#[test]
fn all_pinned_returns_pool_exhausted() -> TestResult {
    let tmp = TempPath::new("exhausted");
    let mut pager = setup(&tmp, 9)?;
    for id in 1..=8 {
        pager.pin(PageId(id))?;
    }
    let page_count = pager.page_count();
    let exhausted = |r: Result<(), StorageError>| matches!(r, Err(StorageError::PoolExhausted));
    assert!(exhausted(pager.read(PageId(9)).map(|_| ())));
    assert!(exhausted(pager.write(PageId(9), &filled(1))));
    assert!(exhausted(pager.allocate().map(|_| ())));
    assert!(exhausted(pager.free(PageId(9))));
    assert_eq!(pager.resident(), 8);
    assert_eq!(pager.page_count(), page_count);
    read_ids(&mut pager, 1..=8)?;

    pager.unpin(PageId(4))?;
    read_ids(&mut pager, [9])?;
    assert!(!pager.is_cached(PageId(4)));
    assert_eq!(pager.resident(), 8);
    for id in [1, 2, 3, 5, 6, 7, 8] {
        pager.unpin(PageId(id))?;
    }
    pager.close()
}

#[test]
fn nested_pins_and_unpin_errors() -> TestResult {
    let tmp = TempPath::new("nested");
    let mut pager = setup(&tmp, 12)?;
    pager.pin(PageId(1))?;
    pager.pin(PageId(1))?;
    pager.unpin(PageId(1))?;
    read_ids(&mut pager, 2..=12)?;
    assert!(pager.is_cached(PageId(1)), "one pin is still held");
    pager.unpin(PageId(1))?;
    assert!(matches!(
        pager.unpin(PageId(1)),
        Err(StorageError::InvalidPage {
            reason: "not pinned",
            ..
        })
    ));
    read_ids(&mut pager, 2..=9)?;
    assert!(matches!(
        pager.unpin(PageId(1)),
        Err(StorageError::InvalidPage {
            reason: "not pinned",
            ..
        })
    ));
    pager.close()
}

#[test]
fn resident_never_exceeds_capacity() -> TestResult {
    let tmp = TempPath::new("bounded");
    let mut pager = setup(&tmp, 30)?;
    let mut rng = XorShift64::new(SEED);
    for step in 0..300 {
        let id = 1 + rng.below(30) as u32;
        if step % 3 == 0 {
            pager.write(PageId(id), &filled(id as u8))?;
        } else {
            read_ids(&mut pager, [id])?;
        }
        assert!(
            pager.resident() <= CAP,
            "resident {} at step {step}",
            pager.resident()
        );
    }
    pager.close()?;
    let mut pager = Pager::open_with_capacity(tmp.path(), CAP)?;
    read_ids(&mut pager, 1..=30)?;
    pager.close()
}
