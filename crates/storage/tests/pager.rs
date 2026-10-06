//! Pager behaviour: file creation, allocation, the on-disk free list, named
//! roots and persistence across close and reopen (milestone AC1, AC2, AC7).

mod common;

use std::io::ErrorKind;

use cairn_storage::{PAGE_SIZE, Page, PageId, Pager, StorageError};
use common::{TempPath, read_raw};

type TestResult = Result<(), StorageError>;

fn filled(byte: u8) -> Page {
    let mut page = Page::zeroed();
    page.bytes_mut().fill(byte);
    page
}

fn file_len(tmp: &TempPath) -> u64 {
    std::fs::metadata(tmp.path()).expect("metadata").len()
}

#[test]
fn temp_paths_are_unique_and_removed() {
    let a = TempPath::new("unique");
    let b = TempPath::new("unique");
    assert_ne!(a.path(), b.path());
    std::fs::write(a.path(), b"x").expect("write");
    let path = a.path().to_path_buf();
    drop(a);
    assert!(!path.exists());
}

#[test]
fn create_makes_one_page_file() -> TestResult {
    let tmp = TempPath::new("create");
    let pager = Pager::create(tmp.path())?;
    assert_eq!(pager.page_count(), 1);
    assert_eq!(pager.pool_capacity(), cairn_storage::DEFAULT_POOL_PAGES);
    pager.close()?;
    let bytes = read_raw(tmp.path());
    assert_eq!(bytes.len(), PAGE_SIZE);
    assert_eq!(&bytes[..6], b"cairn\0");
    assert_eq!(Pager::open(tmp.path())?.page_count(), 1);
    Ok(())
}

#[test]
fn create_existing_path_fails() -> TestResult {
    let tmp = TempPath::new("exists");
    Pager::create(tmp.path())?.close()?;
    match Pager::create(tmp.path()) {
        Err(StorageError::Io(e)) => assert_eq!(e.kind(), ErrorKind::AlreadyExists),
        other => panic!("expected AlreadyExists, got {:?}", other.map(|_| ())),
    }
    Ok(())
}

#[test]
fn write_then_read_returns_same_bytes() -> TestResult {
    let tmp = TempPath::new("rw");
    let mut pager = Pager::create(tmp.path())?;
    let id = pager.allocate()?;
    assert_eq!(pager.read(id)?, Page::zeroed());
    pager.write(id, &filled(0x5A))?;
    assert_eq!(pager.read(id)?, filled(0x5A));
    pager.close()
}

#[test]
fn allocate_grows_then_reuses_freed_lifo() -> TestResult {
    let tmp = TempPath::new("alloc");
    let mut pager = Pager::create(tmp.path())?;
    let ids: Vec<PageId> = (0..3).map(|_| pager.allocate()).collect::<Result<_, _>>()?;
    assert_eq!(ids, [PageId(1), PageId(2), PageId(3)]);
    assert_eq!(pager.page_count(), 4);
    pager.write(PageId(1), &filled(1))?;
    pager.free(PageId(1))?;
    pager.free(PageId(2))?;
    assert_eq!(pager.free_list()?, [PageId(2), PageId(1)]);
    assert_eq!(pager.allocate()?, PageId(2));
    assert_eq!(pager.allocate()?, PageId(1));
    assert_eq!(
        pager.read(PageId(1))?,
        Page::zeroed(),
        "reused page reads as zeroed"
    );
    assert_eq!(pager.page_count(), 4);
    assert!(pager.free_list()?.is_empty());
    assert_eq!(pager.allocate()?, PageId(4));
    pager.close()
}

#[test]
fn free_does_not_grow_file() -> TestResult {
    let tmp = TempPath::new("nogrow");
    let mut pager = Pager::create(tmp.path())?;
    for byte in 1..=3 {
        let id = pager.allocate()?;
        pager.write(id, &filled(byte))?;
    }
    pager.sync()?;
    assert_eq!(file_len(&tmp), 4 * PAGE_SIZE as u64);
    pager.free(PageId(2))?;
    pager.free(PageId(3))?;
    pager.allocate()?;
    pager.allocate()?;
    pager.sync()?;
    assert_eq!(pager.page_count(), 4);
    assert_eq!(file_len(&tmp), 4 * PAGE_SIZE as u64);
    pager.close()
}

#[test]
fn freed_page_stores_its_link_on_disk() -> TestResult {
    let tmp = TempPath::new("link");
    let mut pager = Pager::create(tmp.path())?;
    pager.allocate()?;
    pager.allocate()?;
    pager.free(PageId(1))?;
    pager.free(PageId(2))?;
    pager.close()?;
    let bytes = read_raw(tmp.path());
    let page2 = &bytes[2 * PAGE_SIZE..3 * PAGE_SIZE];
    assert_eq!(page2[0], 3, "free page tag");
    assert_eq!(
        page2[4..8],
        1u32.to_le_bytes(),
        "next link points at page 1"
    );
    assert_eq!(bytes[20..24], 2u32.to_le_bytes(), "header free head");
    assert_eq!(bytes[24..28], 2u32.to_le_bytes(), "header free count");
    Ok(())
}

#[test]
fn invalid_page_ids_are_rejected() -> TestResult {
    let tmp = TempPath::new("invalid");
    let mut pager = Pager::create(tmp.path())?;
    pager.allocate()?;
    let invalid = |r: Result<(), StorageError>| matches!(r, Err(StorageError::InvalidPage { .. }));
    assert!(invalid(pager.free(PageId(0))));
    assert!(invalid(pager.free(PageId(2))));
    assert!(invalid(pager.read(PageId(0)).map(|_| ())));
    assert!(invalid(pager.read(PageId(9)).map(|_| ())));
    assert!(invalid(pager.write(PageId(0), &Page::zeroed())));
    assert!(invalid(pager.write(PageId(9), &Page::zeroed())));
    assert!(invalid(pager.pin(PageId(0))));
    assert_eq!(pager.page_count(), 2);
    pager.close()
}

#[test]
fn double_free_rejected() -> TestResult {
    let tmp = TempPath::new("double");
    let mut pager = Pager::create(tmp.path())?;
    let id = pager.allocate()?;
    pager.free(id)?;
    assert!(matches!(
        pager.free(id),
        Err(StorageError::InvalidPage {
            reason: "already free",
            ..
        })
    ));
    assert_eq!(pager.free_list()?, [id]);
    pager.close()
}

#[test]
fn reopen_persists_pages_free_list_and_roots() -> TestResult {
    let tmp = TempPath::new("reopen");
    let mut pager = Pager::create(tmp.path())?;
    for byte in 1..=5 {
        let id = pager.allocate()?;
        pager.write(id, &filled(byte))?;
    }
    pager.free(PageId(2))?;
    pager.free(PageId(4))?;
    pager.set_root("users", PageId(1))?;
    pager.set_root("users_by_name", PageId(3))?;
    pager.close()?;

    let mut pager = Pager::open(tmp.path())?;
    assert_eq!(pager.page_count(), 6);
    for byte in [1u8, 3, 5] {
        assert_eq!(pager.read(PageId(u32::from(byte)))?, filled(byte));
    }
    assert_eq!(pager.free_list()?, [PageId(4), PageId(2)]);
    assert_eq!(
        pager.roots(),
        [
            ("users".to_owned(), PageId(1)),
            ("users_by_name".to_owned(), PageId(3))
        ]
    );
    assert_eq!(pager.root("users_by_name"), Some(PageId(3)));
    assert_eq!(pager.allocate()?, PageId(4));
    assert_eq!(pager.allocate()?, PageId(2));
    assert_eq!(pager.allocate()?, PageId(6));
    pager.close()
}

#[test]
fn drop_without_close_persists() -> TestResult {
    let tmp = TempPath::new("drop");
    {
        let mut pager = Pager::create(tmp.path())?;
        let id = pager.allocate()?;
        pager.write(id, &filled(9))?;
        pager.set_root("t", id)?;
    }
    let mut pager = Pager::open(tmp.path())?;
    assert_eq!(pager.root("t"), Some(PageId(1)));
    assert_eq!(pager.read(PageId(1))?, filled(9));
    pager.close()
}

#[test]
fn roots_limit_seventeenth_is_root_table_full() -> TestResult {
    let tmp = TempPath::new("roots");
    let mut pager = Pager::create(tmp.path())?;
    let id = pager.allocate()?;
    for i in 0..16 {
        pager.set_root(&format!("r{i}"), id)?;
    }
    assert!(matches!(
        pager.set_root("r16", id),
        Err(StorageError::RootTableFull)
    ));
    pager.set_root("r3", id)?;
    assert_eq!(pager.roots().len(), 16);
    assert!(pager.remove_root("r3"));
    assert!(!pager.remove_root("r3"));
    assert_eq!(pager.root("r3"), None);
    pager.set_root("r16", id)?;
    assert_eq!(pager.roots().len(), 16);
    pager.close()
}

#[test]
fn root_name_and_page_validation() -> TestResult {
    let tmp = TempPath::new("rootname");
    let mut pager = Pager::create(tmp.path())?;
    let id = pager.allocate()?;
    assert!(matches!(
        pager.set_root("", id),
        Err(StorageError::InvalidRootName)
    ));
    assert!(matches!(
        pager.set_root(&"n".repeat(33), id),
        Err(StorageError::InvalidRootName)
    ));
    assert!(matches!(
        pager.set_root("ok", PageId(0)),
        Err(StorageError::InvalidPage { .. })
    ));
    pager.set_root(&"n".repeat(32), id)?;
    pager.set_root("ünïcode", id)?;
    pager.close()?;
    let pager = Pager::open(tmp.path())?;
    assert_eq!(pager.root(&"n".repeat(32)), Some(id));
    assert_eq!(pager.root("ünïcode"), Some(id));
    pager.close()
}

#[test]
fn sync_without_changes_is_a_no_op() -> TestResult {
    let tmp = TempPath::new("noop");
    let mut pager = Pager::create(tmp.path())?;
    pager.sync()?;
    assert_eq!(pager.stats().page_writes, 0);
    pager.close()
}
