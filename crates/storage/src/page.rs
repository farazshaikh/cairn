//! Fixed-size pages, page ids and bounds-checked little-endian field access.

use std::fmt;

use crate::error::{Result, StorageError};

/// Size in bytes of every page in a database file.
pub const PAGE_SIZE: usize = 4096;

/// Index of a page in the database file; page `n` starts at byte `n * 4096`.
/// Page 0 is the header, so 0 doubles as "no page" inside page links.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct PageId(pub u32);

impl fmt::Display for PageId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// An owned copy of one page.
#[derive(Clone, PartialEq, Eq)]
pub struct Page(Box<[u8; PAGE_SIZE]>);

impl Page {
    /// A page of all zero bytes.
    pub fn zeroed() -> Page {
        Page(Box::new([0; PAGE_SIZE]))
    }

    /// The page's bytes.
    pub fn bytes(&self) -> &[u8; PAGE_SIZE] {
        &self.0
    }

    /// The page's bytes, for writing.
    pub fn bytes_mut(&mut self) -> &mut [u8; PAGE_SIZE] {
        &mut self.0
    }
}

impl Default for Page {
    fn default() -> Page {
        Page::zeroed()
    }
}

impl fmt::Debug for Page {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Page")
            .field("head", &self.0.first_chunk::<16>())
            .finish_non_exhaustive()
    }
}

/// Reads little-endian fields from page bytes. Any access outside the page
/// becomes `StorageError::Corrupt` for the page being decoded.
pub(crate) struct Reader<'a> {
    bytes: &'a [u8],
    page: PageId,
}

impl<'a> Reader<'a> {
    pub(crate) fn new(bytes: &'a [u8], page: PageId) -> Reader<'a> {
        Reader { bytes, page }
    }

    pub(crate) fn bytes(&self, offset: usize, len: usize) -> Result<&'a [u8]> {
        offset
            .checked_add(len)
            .and_then(|end| self.bytes.get(offset..end))
            .ok_or(StorageError::Corrupt {
                page: self.page,
                reason: "field runs past end of page",
            })
    }

    pub(crate) fn u8(&self, offset: usize) -> Result<u8> {
        Ok(u8::from_le_bytes(self.array(offset)?))
    }

    pub(crate) fn u16(&self, offset: usize) -> Result<u16> {
        Ok(u16::from_le_bytes(self.array(offset)?))
    }

    pub(crate) fn u32(&self, offset: usize) -> Result<u32> {
        Ok(u32::from_le_bytes(self.array(offset)?))
    }

    /// Fails with `Corrupt { reason }` unless `len` bytes at `offset` are zero.
    pub(crate) fn zeros(&self, offset: usize, len: usize, reason: &'static str) -> Result<()> {
        if self.bytes(offset, len)?.iter().all(|&b| b == 0) {
            return Ok(());
        }
        Err(StorageError::Corrupt {
            page: self.page,
            reason,
        })
    }

    fn array<const N: usize>(&self, offset: usize) -> Result<[u8; N]> {
        let slice = self.bytes(offset, N)?;
        slice.try_into().map_err(|_| StorageError::Corrupt {
            page: self.page,
            reason: "field runs past end of page",
        })
    }
}

/// Writes little-endian fields into page bytes. Writes outside the page
/// return `StorageError::Corrupt` instead of panicking.
pub(crate) struct Writer<'a> {
    bytes: &'a mut [u8],
    page: PageId,
}

impl<'a> Writer<'a> {
    pub(crate) fn new(bytes: &'a mut [u8], page: PageId) -> Writer<'a> {
        Writer { bytes, page }
    }

    pub(crate) fn put_bytes(&mut self, offset: usize, value: &[u8]) -> Result<()> {
        let page = self.page;
        offset
            .checked_add(value.len())
            .and_then(|end| self.bytes.get_mut(offset..end))
            .ok_or(StorageError::Corrupt {
                page,
                reason: "write runs past end of page",
            })?
            .copy_from_slice(value);
        Ok(())
    }

    pub(crate) fn put_u8(&mut self, offset: usize, value: u8) -> Result<()> {
        self.put_bytes(offset, &value.to_le_bytes())
    }

    pub(crate) fn put_u16(&mut self, offset: usize, value: u16) -> Result<()> {
        self.put_bytes(offset, &value.to_le_bytes())
    }

    pub(crate) fn put_u32(&mut self, offset: usize, value: u32) -> Result<()> {
        self.put_bytes(offset, &value.to_le_bytes())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn is_corrupt(result: Result<impl fmt::Debug>) -> bool {
        matches!(
            result,
            Err(StorageError::Corrupt {
                page: PageId(7),
                ..
            })
        )
    }

    #[test]
    fn little_endian_round_trip_at_both_ends() -> Result<()> {
        let mut page = Page::zeroed();
        let mut w = Writer::new(page.bytes_mut(), PageId(7));
        w.put_u8(0, 0xAB)?;
        w.put_u16(1, 0x1234)?;
        w.put_u32(3, 0xDEAD_BEEF)?;
        w.put_bytes(PAGE_SIZE - 3, b"xyz")?;
        w.put_u32(PAGE_SIZE - 7, u32::MAX - 1)?;
        let r = Reader::new(page.bytes(), PageId(7));
        assert_eq!(r.u8(0)?, 0xAB);
        assert_eq!(r.u16(1)?, 0x1234);
        assert_eq!(r.u32(3)?, 0xDEAD_BEEF);
        assert_eq!(r.bytes(PAGE_SIZE - 3, 3)?, b"xyz");
        assert_eq!(r.u32(PAGE_SIZE - 7)?, u32::MAX - 1);
        assert_eq!(page.bytes().get(1..3), Some(&[0x34, 0x12][..]));
        Ok(())
    }

    #[test]
    fn out_of_range_access_is_corrupt_not_panic() {
        let mut page = Page::zeroed();
        let r = Reader::new(page.bytes(), PageId(7));
        assert!(is_corrupt(r.u16(PAGE_SIZE - 1)));
        assert!(is_corrupt(r.u8(PAGE_SIZE)));
        assert!(is_corrupt(r.u32(usize::MAX - 1)));
        assert!(is_corrupt(r.bytes(usize::MAX, 2)));
        assert!(is_corrupt(r.bytes(0, PAGE_SIZE + 1)));
        assert!(r.zeros(0, PAGE_SIZE, "zero").is_ok());

        let mut w = Writer::new(page.bytes_mut(), PageId(7));
        assert!(is_corrupt(w.put_u16(PAGE_SIZE - 1, 1)));
        assert!(is_corrupt(w.put_u8(PAGE_SIZE, 1)));
        assert!(is_corrupt(w.put_u32(usize::MAX - 3, 1)));
        assert!(is_corrupt(w.put_bytes(usize::MAX, b"ab")));
        assert_eq!(page, Page::zeroed());
    }

    #[test]
    fn zeros_reports_its_reason() {
        let mut page = Page::zeroed();
        page.bytes_mut()[100] = 1;
        let r = Reader::new(page.bytes(), PageId(3));
        assert!(matches!(
            r.zeros(90, 20, "padding"),
            Err(StorageError::Corrupt {
                page: PageId(3),
                reason: "padding"
            })
        ));
    }
}
