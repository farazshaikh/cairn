//! The write-ahead log format (`<database>-wal`).
//!
//! All integers are little-endian. Byte ranges are `[start, end)`.
//!
//! Log header, 32 bytes at offset 0:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 8    | magic `cairnwal` |
//! | 8      | 4    | log format version, `u32` = 1 |
//! | 12     | 4    | page size, `u32` = 4096 |
//! | 16     | 8    | salt, `u64`; new for every log generation |
//! | 24     | 4    | reserved, zero |
//! | 28     | 4    | `crc32(0, bytes[0..28])` |
//!
//! Page frame, 32 + 4096 bytes:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 4    | record type, `u32` = 1 |
//! | 4      | 4    | page id, `u32` |
//! | 8      | 8    | transaction id, `u64` |
//! | 16     | 8    | salt, `u64` (equals the header's) |
//! | 24     | 4    | reserved, zero |
//! | 28     | 4    | `crc32(crc32(prev, bytes[0..28]), image)` |
//! | 32     | 4096 | full page image |
//!
//! Commit record, 32 bytes:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 4    | record type, `u32` = 2 |
//! | 4      | 4    | database page count after the commit, `u32` |
//! | 8      | 8    | transaction id, `u64` |
//! | 16     | 8    | salt, `u64` |
//! | 24     | 4    | number of frames in this transaction, `u32` |
//! | 28     | 4    | `crc32(prev, bytes[0..28])` |
//!
//! `prev` is the checksum field of the previous record (the header's for the
//! first record), so each checksum covers everything before it. Transaction
//! ids start at 1 in each generation and increase by exactly 1. A checkpoint
//! truncates the log; the next commit writes a new header with a new salt,
//! so bytes left over from an older generation can never validate.

use std::collections::BTreeMap;

use crate::crc::crc32;
use crate::error::Result;
use crate::page::{PAGE_SIZE, Page, PageId};
use crate::vfs::VfsFile;

pub(crate) const LOG_MAGIC: [u8; 8] = *b"cairnwal";
pub(crate) const LOG_VERSION: u32 = 1;
pub(crate) const LOG_HEADER_SIZE: u64 = 32;
pub(crate) const RECORD_HEADER_SIZE: usize = 32;
pub(crate) const FRAME_SIZE: u64 = (RECORD_HEADER_SIZE + PAGE_SIZE) as u64;
pub(crate) const COMMIT_SIZE: u64 = RECORD_HEADER_SIZE as u64;

const TYPE_FRAME: u32 = 1;
const TYPE_COMMIT: u32 = 2;
const CRC_AT: usize = 28;

/// One committed transaction found by [`scan`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct TxnRecord {
    pub(crate) id: u64,
    pub(crate) page_count: u32,
    /// Page ids and the log offsets of their frames, in log order.
    pub(crate) frames: Vec<(PageId, u64)>,
}

#[derive(Debug, Default)]
pub(crate) struct Scan {
    /// The salt of a valid header, even when no transaction follows it.
    pub(crate) salt: Option<u64>,
    pub(crate) txns: Vec<TxnRecord>,
    /// Offset just past the last valid commit record.
    pub(crate) valid_end: u64,
}

pub(crate) fn encode_header(salt: u64) -> ([u8; 32], u32) {
    let mut bytes = [0u8; 32];
    put(&mut bytes, 0, &LOG_MAGIC);
    put(&mut bytes, 8, &LOG_VERSION.to_le_bytes());
    put(&mut bytes, 12, &(PAGE_SIZE as u32).to_le_bytes());
    put(&mut bytes, 16, &salt.to_le_bytes());
    let crc = crc32(0, bytes.get(..CRC_AT).unwrap_or(&[]));
    put(&mut bytes, CRC_AT, &crc.to_le_bytes());
    (bytes, crc)
}

/// Encodes a page frame chained to `prev`; returns the bytes and checksum.
pub(crate) fn encode_frame(
    id: PageId,
    txn: u64,
    salt: u64,
    image: &Page,
    prev: u32,
) -> (Vec<u8>, u32) {
    let mut head = [0u8; RECORD_HEADER_SIZE];
    put(&mut head, 0, &TYPE_FRAME.to_le_bytes());
    put(&mut head, 4, &id.0.to_le_bytes());
    put(&mut head, 8, &txn.to_le_bytes());
    put(&mut head, 16, &salt.to_le_bytes());
    let crc = crc32(
        crc32(prev, head.get(..CRC_AT).unwrap_or(&[])),
        image.bytes(),
    );
    put(&mut head, CRC_AT, &crc.to_le_bytes());
    let mut bytes = Vec::with_capacity(FRAME_SIZE as usize);
    bytes.extend_from_slice(&head);
    bytes.extend_from_slice(image.bytes());
    (bytes, crc)
}

pub(crate) fn encode_commit(
    page_count: u32,
    txn: u64,
    salt: u64,
    frames: u32,
    prev: u32,
) -> ([u8; 32], u32) {
    let mut bytes = [0u8; RECORD_HEADER_SIZE];
    put(&mut bytes, 0, &TYPE_COMMIT.to_le_bytes());
    put(&mut bytes, 4, &page_count.to_le_bytes());
    put(&mut bytes, 8, &txn.to_le_bytes());
    put(&mut bytes, 16, &salt.to_le_bytes());
    put(&mut bytes, 24, &frames.to_le_bytes());
    let crc = crc32(prev, bytes.get(..CRC_AT).unwrap_or(&[]));
    put(&mut bytes, CRC_AT, &crc.to_le_bytes());
    (bytes, crc)
}

/// Reads the whole log and returns its committed transactions in order. The
/// scan stops at the first record that is short, has the wrong type, salt,
/// transaction id, frame count or checksum, or names a page beyond its
/// commit's page count; everything from there on is ignored. Only an I/O
/// error is an `Err`.
pub(crate) fn scan(log: &mut dyn VfsFile) -> Result<Scan> {
    let len = log.size()? as usize;
    let mut bytes = vec![0u8; len];
    let read = log.read_at(0, &mut bytes)?;
    bytes.truncate(read);
    Ok(scan_bytes(&bytes))
}

pub(crate) fn scan_bytes(bytes: &[u8]) -> Scan {
    let mut scan = Scan::default();
    let Some((salt, header_crc)) = decode_header(bytes) else {
        return scan;
    };
    scan.salt = Some(salt);
    let mut off = LOG_HEADER_SIZE as usize;
    let mut chain = header_crc;
    let mut expect = 1u64;
    let mut pending: Vec<(PageId, u64)> = Vec::new();
    while let Some(head) = bytes.get(off..off + RECORD_HEADER_SIZE) {
        let fields = Fields::read(head);
        if fields.salt != salt || fields.txn != expect {
            break;
        }
        match fields.kind {
            TYPE_FRAME => {
                let Some(image) = bytes.get(off + RECORD_HEADER_SIZE..off + FRAME_SIZE as usize)
                else {
                    break;
                };
                let crc = crc32(crc32(chain, head.get(..CRC_AT).unwrap_or(&[])), image);
                if fields.reserved != 0 || crc != fields.crc {
                    break;
                }
                pending.push((PageId(fields.a), off as u64));
                chain = crc;
                off += FRAME_SIZE as usize;
            }
            TYPE_COMMIT => {
                let crc = crc32(chain, head.get(..CRC_AT).unwrap_or(&[]));
                let page_count = fields.a;
                let consistent = crc == fields.crc
                    && page_count > 0
                    && !pending.is_empty()
                    && fields.reserved as usize == pending.len()
                    && pending.iter().all(|(id, _)| id.0 < page_count);
                if !consistent {
                    break;
                }
                scan.txns.push(TxnRecord {
                    id: expect,
                    page_count,
                    frames: std::mem::take(&mut pending),
                });
                chain = crc;
                expect += 1;
                off += COMMIT_SIZE as usize;
                scan.valid_end = off as u64;
            }
            _ => break,
        }
    }
    scan
}

/// The latest committed log offset of every page in `txns`.
pub(crate) fn latest_frames(txns: &[TxnRecord]) -> BTreeMap<PageId, u64> {
    txns.iter().flat_map(|t| t.frames.iter().copied()).collect()
}

/// The 28 bytes before the checksum, split into their fields. For a frame
/// `a` is the page id and `reserved` must be zero; for a commit `a` is the
/// page count and `reserved` is the frame count.
struct Fields {
    kind: u32,
    a: u32,
    txn: u64,
    salt: u64,
    reserved: u32,
    crc: u32,
}

impl Fields {
    fn read(head: &[u8]) -> Fields {
        Fields {
            kind: le_u32(head, 0),
            a: le_u32(head, 4),
            txn: le_u64(head, 8),
            salt: le_u64(head, 16),
            reserved: le_u32(head, 24),
            crc: le_u32(head, CRC_AT),
        }
    }
}

fn decode_header(bytes: &[u8]) -> Option<(u64, u32)> {
    let head = bytes.get(..LOG_HEADER_SIZE as usize)?;
    let valid = head.get(..8) == Some(&LOG_MAGIC[..])
        && le_u32(head, 8) == LOG_VERSION
        && le_u32(head, 12) as usize == PAGE_SIZE
        && le_u32(head, 24) == 0;
    let crc = crc32(0, head.get(..CRC_AT)?);
    if !valid || crc != le_u32(head, CRC_AT) {
        return None;
    }
    Some((le_u64(head, 16), crc))
}

fn put(bytes: &mut [u8], at: usize, value: &[u8]) {
    if let Some(target) = bytes.get_mut(at..at + value.len()) {
        target.copy_from_slice(value);
    }
}

/// Reads a little-endian `u32`; out-of-range bytes read as zero (callers
/// pass slices whose length they already checked).
pub(crate) fn le_u32(bytes: &[u8], at: usize) -> u32 {
    let mut raw = [0u8; 4];
    if let Some(src) = bytes.get(at..at + 4) {
        raw.copy_from_slice(src);
    }
    u32::from_le_bytes(raw)
}

fn le_u64(bytes: &[u8], at: usize) -> u64 {
    let mut raw = [0u8; 8];
    if let Some(src) = bytes.get(at..at + 8) {
        raw.copy_from_slice(src);
    }
    u64::from_le_bytes(raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    const SALT: u64 = 0x1234_5678_9ABC_DEF0;

    fn page(fill: u8) -> Page {
        let mut page = Page::zeroed();
        page.bytes_mut().fill(fill);
        page
    }

    /// A log of transactions given as lists of page ids, with page count 50.
    fn build(txns: &[&[u32]]) -> (Vec<u8>, Vec<u64>) {
        let (header, mut chain) = encode_header(SALT);
        let mut log = header.to_vec();
        let mut ends = Vec::new();
        for (i, pages) in txns.iter().enumerate() {
            let txn = i as u64 + 1;
            for &id in *pages {
                let (frame, crc) = encode_frame(PageId(id), txn, SALT, &page(id as u8), chain);
                log.extend_from_slice(&frame);
                chain = crc;
            }
            let (commit, crc) = encode_commit(50, txn, SALT, pages.len() as u32, chain);
            log.extend_from_slice(&commit);
            chain = crc;
            ends.push(log.len() as u64);
        }
        (log, ends)
    }

    #[test]
    fn round_trip_of_several_transactions() {
        let (log, ends) = build(&[&[1, 2], &[3], &[2, 4, 5], &[0], &[7, 1]]);
        let scan = scan_bytes(&log);
        assert_eq!(scan.salt, Some(SALT));
        assert_eq!(scan.txns.len(), 5);
        assert_eq!(scan.valid_end, *ends.last().expect("end"));
        let ids: Vec<Vec<u32>> = scan
            .txns
            .iter()
            .map(|t| t.frames.iter().map(|(p, _)| p.0).collect())
            .collect();
        assert_eq!(
            ids,
            vec![vec![1, 2], vec![3], vec![2, 4, 5], vec![0], vec![7, 1]]
        );
        let (_, offset) = scan.txns[2].frames[0];
        let start = offset as usize + RECORD_HEADER_SIZE;
        assert!(log[start..start + PAGE_SIZE].iter().all(|&b| b == 2));
        assert_eq!(latest_frames(&scan.txns).len(), 7);
    }

    #[test]
    fn incomplete_tail_is_ignored() {
        let (mut log, ends) = build(&[&[1], &[2]]);
        let (frame, _) = encode_frame(PageId(3), 3, SALT, &page(3), 0);
        log.extend_from_slice(&frame);
        let scan = scan_bytes(&log);
        assert_eq!(scan.txns.len(), 2);
        assert_eq!(scan.valid_end, ends[1]);
    }

    #[test]
    fn every_byte_flip_in_the_last_transaction_drops_only_it() {
        let (log, ends) = build(&[&[1, 2], &[3, 4]]);
        for at in ends[0] as usize..log.len() {
            let mut bad = log.clone();
            bad[at] ^= 0x01;
            let scan = scan_bytes(&bad);
            assert_eq!(scan.txns.len(), 1, "flip at {at}");
            assert_eq!(scan.valid_end, ends[0]);
        }
    }

    #[test]
    fn truncation_at_every_offset_keeps_earlier_transactions() {
        let (log, ends) = build(&[&[1], &[2, 3]]);
        for cut in ends[0] as usize..log.len() {
            assert_eq!(scan_bytes(&log[..cut]).txns.len(), 1, "cut at {cut}");
        }
        assert_eq!(scan_bytes(&log).txns.len(), 2);
    }

    #[test]
    fn stale_generation_and_bad_sequences_are_rejected() {
        let (log, ends) = build(&[&[1], &[2]]);
        // Same records under a header with another salt: nothing validates.
        let (other, _) = encode_header(SALT + 1);
        let mut stale = other.to_vec();
        stale.extend_from_slice(&log[32..]);
        assert!(scan_bytes(&stale).txns.is_empty());
        // A repeated transaction (records of txn 1 appended again) stops.
        let mut repeated = log.clone();
        repeated.extend_from_slice(&log[32..ends[0] as usize]);
        assert_eq!(scan_bytes(&repeated).txns.len(), 2);
    }

    #[test]
    fn inconsistent_commit_records_are_rejected() {
        let (header, chain) = encode_header(SALT);
        let (frame, crc) = encode_frame(PageId(9), 1, SALT, &page(9), chain);
        let with = |count: u32, frames: u32| {
            let mut log = header.to_vec();
            log.extend_from_slice(&frame);
            log.extend_from_slice(&encode_commit(count, 1, SALT, frames, crc).0);
            scan_bytes(&log).txns.len()
        };
        assert_eq!(with(10, 1), 1);
        assert_eq!(with(9, 1), 0, "page id beyond the page count");
        assert_eq!(with(10, 2), 0, "frame count mismatch");
        assert_eq!(with(0, 1), 0, "zero page count");
        let mut empty = header.to_vec();
        empty.extend_from_slice(&encode_commit(10, 1, SALT, 0, chain).0);
        assert!(scan_bytes(&empty).txns.is_empty(), "commit without frames");
    }

    #[test]
    fn bad_headers_give_no_transactions() {
        let (log, _) = build(&[&[1]]);
        for at in 0..32 {
            let mut bad = log.clone();
            bad[at] ^= 0x80;
            let scan = scan_bytes(&bad);
            assert!(scan.txns.is_empty() && scan.salt.is_none(), "flip at {at}");
        }
        assert!(scan_bytes(&log[..31]).salt.is_none());
        assert!(scan_bytes(&[]).txns.is_empty());
    }
}
