//! Every corrupt-file error: malformed files are reported as typed
//! `StorageError`s and never panic (milestone AC1, AC6, AC7).

mod common;

use std::ops::Bound::Unbounded;

use cairn_storage::{BTree, PAGE_SIZE, PageId, Pager, StorageError};
use common::{TempPath, write_raw};

type TestResult = Result<(), StorageError>;

/// A valid file with pages 1 and 2 allocated.
fn valid_file(tag: &str) -> Result<TempPath, StorageError> {
    let tmp = TempPath::new(tag);
    let mut pager = Pager::create(tmp.path())?;
    let a = pager.allocate()?;
    pager.allocate()?;
    pager.set_root("t", a)?;
    pager.close()?;
    Ok(tmp)
}

fn open_error(tmp: &TempPath) -> StorageError {
    match Pager::open(tmp.path()) {
        Err(e) => e,
        Ok(_) => panic!("expected open of {} to fail", tmp.path().display()),
    }
}

#[test]
fn empty_file_is_too_short() {
    let tmp = TempPath::new("empty");
    std::fs::write(tmp.path(), b"").expect("write");
    assert!(matches!(
        open_error(&tmp),
        StorageError::FileTooShort { len: 0 }
    ));
}

#[test]
fn hundred_byte_file_is_too_short() {
    let tmp = TempPath::new("short");
    std::fs::write(tmp.path(), [7u8; 100]).expect("write");
    let error = open_error(&tmp);
    assert!(matches!(error, StorageError::FileTooShort { len: 100 }));
    assert!(error.to_string().contains("too short"));
}

#[test]
fn length_not_a_page_multiple() -> TestResult {
    let tmp = valid_file("multiple")?;
    let len = 3 * PAGE_SIZE as u64;
    write_raw(tmp.path(), len, &[0; 100]);
    let error = open_error(&tmp);
    assert!(matches!(error, StorageError::NotPageMultiple { len } if len == 3 * 4096 + 100));
    assert!(error.to_string().contains("not a multiple"));
    Ok(())
}

#[test]
fn wrong_magic() -> TestResult {
    let tmp = valid_file("magic")?;
    write_raw(tmp.path(), 0, b"SQLite");
    let error = open_error(&tmp);
    assert!(matches!(error, StorageError::BadMagic { found } if &found == b"SQLite"));
    assert!(error.to_string().contains("bad magic"));
    Ok(())
}

#[test]
fn unsupported_version() -> TestResult {
    let tmp = valid_file("version")?;
    write_raw(tmp.path(), 8, &2u32.to_le_bytes());
    let error = open_error(&tmp);
    assert!(matches!(
        error,
        StorageError::UnsupportedVersion { found: 2 }
    ));
    assert!(error.to_string().contains("unsupported format version 2"));
    Ok(())
}

#[test]
fn page_count_disagrees_with_length() -> TestResult {
    let tmp = valid_file("count")?;
    write_raw(tmp.path(), 16, &7u32.to_le_bytes());
    assert!(matches!(
        open_error(&tmp),
        StorageError::Corrupt {
            page: PageId(0),
            ..
        }
    ));
    Ok(())
}

#[test]
fn free_head_out_of_range() -> TestResult {
    let tmp = valid_file("freehead")?;
    write_raw(tmp.path(), 20, &99u32.to_le_bytes());
    write_raw(tmp.path(), 24, &1u32.to_le_bytes());
    assert!(matches!(
        open_error(&tmp),
        StorageError::Corrupt {
            page: PageId(0),
            reason: "free list head"
        }
    ));
    Ok(())
}

#[test]
fn malformed_root_slot() -> TestResult {
    let tmp = valid_file("rootslot")?;
    write_raw(tmp.path(), 64, &[40]);
    assert!(matches!(
        open_error(&tmp),
        StorageError::Corrupt {
            page: PageId(0),
            reason: "root slot"
        }
    ));
    Ok(())
}

#[test]
fn corrupt_free_page_is_detected_on_allocate() -> TestResult {
    let tmp = valid_file("freepage")?;
    let mut pager = Pager::open(tmp.path())?;
    pager.free(PageId(1))?;
    pager.close()?;
    write_raw(tmp.path(), PAGE_SIZE as u64 + 100, &[0xAA]);
    let mut pager = Pager::open(tmp.path())?;
    assert!(matches!(
        pager.allocate(),
        Err(StorageError::Corrupt {
            page: PageId(1),
            ..
        })
    ));
    assert_eq!(
        pager.page_count(),
        3,
        "failed allocate leaves the pager unchanged"
    );
    Ok(())
}

#[test]
fn free_list_cycle_is_detected() -> TestResult {
    let tmp = valid_file("cycle")?;
    let mut pager = Pager::open(tmp.path())?;
    pager.free(PageId(1))?;
    pager.free(PageId(2))?;
    pager.close()?;
    // List is 2 -> 1; point 1 back at 2.
    write_raw(tmp.path(), PAGE_SIZE as u64 + 4, &2u32.to_le_bytes());
    let mut pager = Pager::open(tmp.path())?;
    assert!(matches!(
        pager.free_list(),
        Err(StorageError::Corrupt {
            page: PageId(0),
            ..
        })
    ));
    Ok(())
}

#[test]
fn free_list_shorter_than_count_is_detected() -> TestResult {
    let tmp = valid_file("shortlist")?;
    let mut pager = Pager::open(tmp.path())?;
    pager.free(PageId(1))?;
    pager.free(PageId(2))?;
    pager.close()?;
    // List is 2 -> 1; cut it after page 2 while the header still says 2.
    write_raw(tmp.path(), 2 * PAGE_SIZE as u64 + 4, &0u32.to_le_bytes());
    let mut pager = Pager::open(tmp.path())?;
    assert!(matches!(
        pager.free_list(),
        Err(StorageError::Corrupt { .. })
    ));
    assert!(matches!(
        pager.allocate(),
        Err(StorageError::Corrupt { .. })
    ));
    Ok(())
}

#[test]
fn corrupt_node_page_is_reported_not_panicked() -> TestResult {
    let tmp = TempPath::new("node");
    let mut pager = Pager::create(tmp.path())?;
    let mut tree = BTree::open(BTree::create(&mut pager)?);
    for i in 0..500u32 {
        tree.insert(&mut pager, &i.to_be_bytes(), b"value")?;
    }
    let root = tree.root();
    pager.set_root("t", root)?;
    pager.close()?;
    write_raw(tmp.path(), u64::from(root.0) * PAGE_SIZE as u64, &[9]);

    let mut pager = Pager::open(tmp.path())?;
    let mut tree = BTree::open(pager.root("t").expect("root"));
    let corrupt_root = |r: Result<(), StorageError>| matches!(r, Err(StorageError::Corrupt { page, reason: "node tag" }) if page == root);
    assert!(corrupt_root(tree.get(&mut pager, b"k").map(|_| ())));
    assert!(corrupt_root(tree.insert(&mut pager, b"k", b"v")));
    assert!(corrupt_root(tree.delete(&mut pager, b"k").map(|_| ())));
    assert!(corrupt_root(
        tree.range(&mut pager, Unbounded, Unbounded).map(|_| ())
    ));
    let report = tree
        .check(&mut pager)
        .expect_err("check reports corruption");
    assert!(
        report.contains(&format!("page {root}")) && report.contains("node tag"),
        "{report}"
    );
    Ok(())
}
