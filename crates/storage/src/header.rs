//! The header page (page 0).
//!
//! All integers are little-endian. Byte ranges are `[start, end)`.
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 6    | magic `cairn\0` (`63 61 69 72 6E 00`) |
//! | 6      | 2    | reserved, zero |
//! | 8      | 4    | format version, `u32` = 1 |
//! | 12     | 4    | page size, `u32` = 4096 |
//! | 16     | 4    | page count, `u32`; equals file length / 4096 |
//! | 20     | 4    | free-list head page id, `u32` (0 = empty) |
//! | 24     | 4    | free-list length, `u32` |
//! | 28     | 36   | reserved, zero |
//! | 64     | 640  | 16 root slots of 40 bytes; slot `i` starts at `64 + 40 * i` |
//! | 704    | 3392 | reserved, zero |
//!
//! Root slot:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 1    | name length `u8`, 1..=32; 0 marks an empty, all-zero slot |
//! | 1      | 3    | reserved, zero |
//! | 4      | 32   | UTF-8 name, zero-padded |
//! | 36     | 4    | root page id, `u32` |
//!
//! Decoding checks fields in table order and reports the first problem.

use crate::error::{Result, StorageError};
use crate::page::{PAGE_SIZE, Page, PageId, Reader, Writer};

/// Magic bytes at the start of every cairn database file.
pub const MAGIC: [u8; 6] = *b"cairn\0";
/// The only on-disk format version this build reads and writes.
pub const FORMAT_VERSION: u32 = 1;
/// Number of named root slots in the header.
pub const MAX_ROOTS: usize = 16;
/// Maximum byte length of a root name.
pub const MAX_ROOT_NAME_LEN: usize = 32;

const OFF_RESERVED_A: usize = 6;
const OFF_VERSION: usize = 8;
const OFF_PAGE_SIZE: usize = 12;
const OFF_PAGE_COUNT: usize = 16;
const OFF_FREE_HEAD: usize = 20;
const OFF_FREE_COUNT: usize = 24;
const OFF_RESERVED_B: usize = 28;
const OFF_ROOTS: usize = 64;
const ROOT_SLOT_SIZE: usize = 40;
const OFF_RESERVED_C: usize = OFF_ROOTS + MAX_ROOTS * ROOT_SLOT_SIZE;

const SLOT_NAME: usize = 4;
const SLOT_ROOT: usize = 36;

const HEADER: PageId = PageId(0);

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct RootSlot {
    pub(crate) name: String,
    pub(crate) root: PageId,
}

/// In-memory copy of page 0.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Header {
    pub(crate) page_count: u32,
    pub(crate) free_head: u32,
    pub(crate) free_count: u32,
    pub(crate) roots: [Option<RootSlot>; MAX_ROOTS],
}

impl Header {
    /// Header of a freshly created file: one page, no free pages, no roots.
    pub(crate) fn new() -> Header {
        Header {
            page_count: 1,
            free_head: 0,
            free_count: 0,
            roots: Default::default(),
        }
    }

    pub(crate) fn encode(&self) -> Result<Page> {
        let mut page = Page::zeroed();
        let mut w = Writer::new(page.bytes_mut(), HEADER);
        w.put_bytes(0, &MAGIC)?;
        w.put_u32(OFF_VERSION, FORMAT_VERSION)?;
        w.put_u32(OFF_PAGE_SIZE, PAGE_SIZE as u32)?;
        w.put_u32(OFF_PAGE_COUNT, self.page_count)?;
        w.put_u32(OFF_FREE_HEAD, self.free_head)?;
        w.put_u32(OFF_FREE_COUNT, self.free_count)?;
        for (i, slot) in self.roots.iter().enumerate() {
            let Some(slot) = slot else { continue };
            let offset = OFF_ROOTS + i * ROOT_SLOT_SIZE;
            let name_len =
                u8::try_from(slot.name.len()).map_err(|_| StorageError::InvalidRootName)?;
            w.put_u8(offset, name_len)?;
            w.put_bytes(offset + SLOT_NAME, slot.name.as_bytes())?;
            w.put_u32(offset + SLOT_ROOT, slot.root.0)?;
        }
        Ok(page)
    }

    /// Decodes page 0 of a file that is `file_len` bytes long.
    pub(crate) fn decode(page: &Page, file_len: u64) -> Result<Header> {
        let r = Reader::new(page.bytes(), HEADER);
        let magic = r.bytes(0, MAGIC.len())?;
        if magic != MAGIC {
            let mut found = [0; 6];
            found.copy_from_slice(magic);
            return Err(StorageError::BadMagic { found });
        }
        r.zeros(OFF_RESERVED_A, 2, "header reserved bytes")?;
        let version = r.u32(OFF_VERSION)?;
        if version != FORMAT_VERSION {
            return Err(StorageError::UnsupportedVersion { found: version });
        }
        if r.u32(OFF_PAGE_SIZE)? as usize != PAGE_SIZE {
            return Err(corrupt("page size"));
        }
        let page_count = r.u32(OFF_PAGE_COUNT)?;
        if page_count == 0 || u64::from(page_count) * PAGE_SIZE as u64 != file_len {
            return Err(corrupt("page count does not match file length"));
        }
        let free_head = r.u32(OFF_FREE_HEAD)?;
        if free_head >= page_count {
            return Err(corrupt("free list head"));
        }
        let free_count = r.u32(OFF_FREE_COUNT)?;
        if free_count > page_count - 1 || (free_count == 0) != (free_head == 0) {
            return Err(corrupt("free list count"));
        }
        r.zeros(
            OFF_RESERVED_B,
            OFF_ROOTS - OFF_RESERVED_B,
            "header reserved bytes",
        )?;
        let mut roots: [Option<RootSlot>; MAX_ROOTS] = Default::default();
        for (i, slot) in roots.iter_mut().enumerate() {
            *slot = decode_slot(&r, OFF_ROOTS + i * ROOT_SLOT_SIZE, page_count)?;
        }
        if has_duplicate_names(&roots) {
            return Err(corrupt("duplicate root name"));
        }
        r.zeros(
            OFF_RESERVED_C,
            PAGE_SIZE - OFF_RESERVED_C,
            "header reserved bytes",
        )?;
        Ok(Header {
            page_count,
            free_head,
            free_count,
            roots,
        })
    }
}

fn decode_slot(r: &Reader<'_>, offset: usize, page_count: u32) -> Result<Option<RootSlot>> {
    let name_len = usize::from(r.u8(offset)?);
    if name_len == 0 {
        r.zeros(offset, ROOT_SLOT_SIZE, "root slot")?;
        return Ok(None);
    }
    if name_len > MAX_ROOT_NAME_LEN {
        return Err(corrupt("root slot"));
    }
    r.zeros(offset + 1, SLOT_NAME - 1, "root slot")?;
    let name = r.bytes(offset + SLOT_NAME, name_len)?;
    r.zeros(
        offset + SLOT_NAME + name_len,
        MAX_ROOT_NAME_LEN - name_len,
        "root slot",
    )?;
    let name = String::from_utf8(name.to_vec()).map_err(|_| corrupt("root slot"))?;
    let root = r.u32(offset + SLOT_ROOT)?;
    if root == 0 || root >= page_count {
        return Err(corrupt("root slot"));
    }
    Ok(Some(RootSlot {
        name,
        root: PageId(root),
    }))
}

fn has_duplicate_names(roots: &[Option<RootSlot>]) -> bool {
    let names: Vec<&str> = roots.iter().flatten().map(|s| s.name.as_str()).collect();
    names
        .iter()
        .enumerate()
        .any(|(i, name)| names.iter().skip(i + 1).any(|other| other == name))
}

fn corrupt(reason: &'static str) -> StorageError {
    StorageError::Corrupt {
        page: HEADER,
        reason,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const LEN: u64 = 10 * PAGE_SIZE as u64;

    fn sample() -> Header {
        let mut header = Header::new();
        header.page_count = 10;
        header.free_head = 7;
        header.free_count = 3;
        header
    }

    fn patched(offset: usize, bytes: &[u8]) -> Page {
        let mut page = sample().encode().expect("encode");
        page.bytes_mut()[offset..offset + bytes.len()].copy_from_slice(bytes);
        page
    }

    fn corrupt_reason(page: &Page) -> &'static str {
        match Header::decode(page, LEN) {
            Err(StorageError::Corrupt {
                page: PageId(0),
                reason,
            }) => reason,
            other => panic!("expected Corrupt on page 0, got {other:?}"),
        }
    }

    #[test]
    fn round_trips_without_roots() -> Result<()> {
        let header = sample();
        assert_eq!(Header::decode(&header.encode()?, LEN)?, header);
        Ok(())
    }

    #[test]
    fn round_trips_all_sixteen_roots() -> Result<()> {
        let mut header = sample();
        for (i, slot) in header.roots.iter_mut().enumerate() {
            let name = if i == 0 {
                "x".repeat(32)
            } else {
                format!("root-{i}")
            };
            *slot = Some(RootSlot {
                name,
                root: PageId(1 + i as u32 % 9),
            });
        }
        let page = header.encode()?;
        assert_eq!(&page.bytes()[..6], b"cairn\0");
        assert_eq!(Header::decode(&page, LEN)?, header);
        Ok(())
    }

    #[test]
    fn field_offsets_match_documented_layout() -> Result<()> {
        let mut header = sample();
        header.roots[1] = Some(RootSlot {
            name: "ab".into(),
            root: PageId(5),
        });
        let page = header.encode()?;
        let b = page.bytes();
        assert_eq!(b[8..12], 1u32.to_le_bytes());
        assert_eq!(b[12..16], 4096u32.to_le_bytes());
        assert_eq!(b[16..20], 10u32.to_le_bytes());
        assert_eq!(b[20..24], 7u32.to_le_bytes());
        assert_eq!(b[24..28], 3u32.to_le_bytes());
        assert_eq!(b[104], 2);
        assert_eq!(&b[108..110], b"ab");
        assert_eq!(b[140..144], 5u32.to_le_bytes());
        Ok(())
    }

    #[test]
    fn rejects_wrong_magic_and_version() {
        let page = patched(0, b"cairX\0");
        assert!(matches!(
            Header::decode(&page, LEN),
            Err(StorageError::BadMagic { found }) if &found == b"cairX\0"
        ));
        let page = patched(OFF_VERSION, &2u32.to_le_bytes());
        assert!(matches!(
            Header::decode(&page, LEN),
            Err(StorageError::UnsupportedVersion { found: 2 })
        ));
    }

    #[test]
    fn rejects_inconsistent_fields_as_corrupt_page_zero() {
        assert_eq!(corrupt_reason(&patched(6, &[1])), "header reserved bytes");
        assert_eq!(
            corrupt_reason(&patched(OFF_PAGE_SIZE, &512u32.to_le_bytes())),
            "page size"
        );
        assert_eq!(
            corrupt_reason(&patched(OFF_PAGE_COUNT, &0u32.to_le_bytes())),
            "page count does not match file length"
        );
        assert_eq!(
            corrupt_reason(&patched(OFF_PAGE_COUNT, &11u32.to_le_bytes())),
            "page count does not match file length"
        );
        assert_eq!(
            corrupt_reason(&patched(OFF_FREE_HEAD, &10u32.to_le_bytes())),
            "free list head"
        );
        assert_eq!(
            corrupt_reason(&patched(OFF_FREE_COUNT, &10u32.to_le_bytes())),
            "free list count"
        );
        assert_eq!(
            corrupt_reason(&patched(OFF_FREE_COUNT, &0u32.to_le_bytes())),
            "free list count"
        );
        assert_eq!(corrupt_reason(&patched(40, &[9])), "header reserved bytes");
        assert_eq!(
            corrupt_reason(&patched(4095, &[9])),
            "header reserved bytes"
        );
    }

    #[test]
    fn rejects_malformed_root_slots() {
        let slot = OFF_ROOTS + ROOT_SLOT_SIZE;
        assert_eq!(corrupt_reason(&patched(slot, &[33])), "root slot");
        assert_eq!(
            corrupt_reason(&patched(slot + SLOT_ROOT, &[1])),
            "root slot"
        );
        let mut good = sample();
        good.roots[0] = Some(RootSlot {
            name: "a".into(),
            root: PageId(2),
        });
        let mut page = good.encode().expect("encode");
        page.bytes_mut()[OFF_ROOTS + SLOT_NAME] = 0xFF;
        assert_eq!(corrupt_reason(&page), "root slot");
        let mut page = good.encode().expect("encode");
        page.bytes_mut()[OFF_ROOTS + SLOT_ROOT..OFF_ROOTS + SLOT_ROOT + 4]
            .copy_from_slice(&10u32.to_le_bytes());
        assert_eq!(corrupt_reason(&page), "root slot");
        let mut page = good.encode().expect("encode");
        page.bytes_mut()[OFF_ROOTS + SLOT_NAME + 5] = b'z';
        assert_eq!(corrupt_reason(&page), "root slot");
        good.roots[3] = Some(RootSlot {
            name: "a".into(),
            root: PageId(4),
        });
        assert_eq!(
            corrupt_reason(&good.encode().expect("encode")),
            "duplicate root name"
        );
    }

    #[test]
    fn every_single_byte_flip_decodes_or_errors() {
        let mut header = sample();
        header.roots[0] = Some(RootSlot {
            name: "table".into(),
            root: PageId(3),
        });
        let original = header.encode().expect("encode");
        for offset in 0..PAGE_SIZE {
            let mut page = original.clone();
            page.bytes_mut()[offset] ^= 0xFF;
            let _ = Header::decode(&page, LEN);
        }
        assert!(Header::decode(&Page::zeroed(), LEN).is_err());
        let mut ones = Page::zeroed();
        ones.bytes_mut().fill(0xFF);
        assert!(Header::decode(&ones, LEN).is_err());
    }
}
