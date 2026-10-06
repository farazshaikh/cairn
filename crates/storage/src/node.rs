//! B-tree node pages (B+ tree: all key/value pairs live in leaves).
//!
//! All integers are little-endian. Cells are packed in ascending key order
//! directly after an 8-byte node header, and every byte after the last cell
//! is zero. Keys compare as unsigned bytes.
//!
//! Leaf node:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 1    | tag = 1 |
//! | 1      | 1    | reserved, zero |
//! | 2      | 2    | cell count, `u16` |
//! | 4      | 4    | next leaf page id, `u32` (0 = last leaf) |
//! | 8      | ...  | cells: key length `u16`, value length `u16`, key bytes, value bytes |
//!
//! Internal node:
//!
//! | Offset | Size | Field |
//! |--------|------|-------|
//! | 0      | 1    | tag = 2 |
//! | 1      | 1    | reserved, zero |
//! | 2      | 2    | key count, `u16` (at least 1) |
//! | 4      | 4    | leftmost child page id, `u32` |
//! | 8      | ...  | cells: key length `u16`, key bytes, right child page id `u32` |
//!
//! Child `0` is the leftmost child and child `i >= 1` is the child of cell
//! `i - 1`. Every key `k` under child `i` satisfies
//! `cell[i-1].key <= k < cell[i].key`.
//!
//! # Fill bounds
//!
//! A node's payload is the byte size of its cells (at most 4088). A non-root
//! node is rebalanced with a sibling when its payload drops below half
//! (2044 bytes). Because cells are variable-sized (a leaf cell can take 1284
//! bytes), a literal half-full invariant cannot always be restored. The bound
//! that splits, merges and redistributions always guarantee, and that
//! `BTree::check` enforces, is 1402 bytes for leaves and 1782 bytes for
//! internal nodes.

use crate::error::{Result, StorageError};
use crate::page::{PAGE_SIZE, Page, PageId, Reader, Writer};

/// Longest key a B-tree accepts.
pub const MAX_KEY_LEN: usize = 256;
/// Longest value a B-tree accepts.
pub const MAX_VALUE_LEN: usize = 1024;

pub(crate) const TAG_LEAF: u8 = 1;
pub(crate) const TAG_INTERNAL: u8 = 2;
pub(crate) const NODE_HEADER: usize = 8;
pub(crate) const PAYLOAD: usize = PAGE_SIZE - NODE_HEADER;
pub(crate) const MAX_LEAF_CELL: usize = 4 + MAX_KEY_LEN + MAX_VALUE_LEN;
pub(crate) const MAX_INTERNAL_CELL: usize = 2 + MAX_KEY_LEN + 4;
pub(crate) const REBALANCE_BELOW: usize = PAYLOAD / 2;
pub(crate) const LEAF_MIN_FILL: usize = (PAYLOAD - MAX_LEAF_CELL) / 2;
pub(crate) const INTERNAL_MIN_FILL: usize = PAYLOAD / 2 - MAX_INTERNAL_CELL;

// Splits must always produce two non-empty nodes.
const _: () = assert!(PAYLOAD / MAX_LEAF_CELL >= 3);
const _: () = assert!(PAYLOAD / MAX_INTERNAL_CELL >= 3);

const OFF_COUNT: usize = 2;
const OFF_LINK: usize = 4;

pub(crate) type LeafCell = (Vec<u8>, Vec<u8>);
pub(crate) type InternalCell = (Vec<u8>, PageId);

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Leaf {
    pub(crate) cells: Vec<LeafCell>,
    pub(crate) next: Option<PageId>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) struct Internal {
    pub(crate) leftmost: PageId,
    pub(crate) cells: Vec<InternalCell>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Node {
    Leaf(Leaf),
    Internal(Internal),
}

pub(crate) fn leaf_cell_size(cell: &LeafCell) -> usize {
    4 + cell.0.len() + cell.1.len()
}

pub(crate) fn internal_cell_size(cell: &InternalCell) -> usize {
    2 + cell.0.len() + 4
}

impl Leaf {
    pub(crate) fn payload(&self) -> usize {
        self.cells.iter().map(leaf_cell_size).sum()
    }
}

impl Internal {
    pub(crate) fn payload(&self) -> usize {
        internal_payload(&self.cells)
    }

    /// Index of the child whose key range contains `key`.
    pub(crate) fn child_index(&self, key: &[u8]) -> usize {
        self.cells.partition_point(|(sep, _)| sep.as_slice() <= key)
    }

    pub(crate) fn child(&self, index: usize, id: PageId) -> Result<PageId> {
        if index == 0 {
            return Ok(self.leftmost);
        }
        self.cells
            .get(index - 1)
            .map(|cell| cell.1)
            .ok_or(StorageError::Corrupt {
                page: id,
                reason: "child index out of range",
            })
    }
}

pub(crate) fn internal_payload(cells: &[InternalCell]) -> usize {
    cells.iter().map(internal_cell_size).sum()
}

impl Node {
    /// Encodes the node as page `id`. Fails with `Corrupt` if the cells do
    /// not fit, which indicates a caller bug rather than bad input.
    pub(crate) fn encode(&self, id: PageId) -> Result<Page> {
        let mut page = Page::zeroed();
        let mut w = Writer::new(page.bytes_mut(), id);
        let too_many = || StorageError::Corrupt {
            page: id,
            reason: "too many cells for one page",
        };
        let mut offset = NODE_HEADER;
        match self {
            Node::Leaf(leaf) => {
                w.put_u8(0, TAG_LEAF)?;
                w.put_u16(
                    OFF_COUNT,
                    u16::try_from(leaf.cells.len()).map_err(|_| too_many())?,
                )?;
                w.put_u32(OFF_LINK, leaf.next.map_or(0, |p| p.0))?;
                for (key, value) in &leaf.cells {
                    w.put_u16(offset, len_u16(key, id)?)?;
                    w.put_u16(offset + 2, len_u16(value, id)?)?;
                    w.put_bytes(offset + 4, key)?;
                    w.put_bytes(offset + 4 + key.len(), value)?;
                    offset += 4 + key.len() + value.len();
                }
            }
            Node::Internal(node) => {
                w.put_u8(0, TAG_INTERNAL)?;
                w.put_u16(
                    OFF_COUNT,
                    u16::try_from(node.cells.len()).map_err(|_| too_many())?,
                )?;
                w.put_u32(OFF_LINK, node.leftmost.0)?;
                for (key, child) in &node.cells {
                    w.put_u16(offset, len_u16(key, id)?)?;
                    w.put_bytes(offset + 2, key)?;
                    w.put_u32(offset + 2 + key.len(), child.0)?;
                    offset += 6 + key.len();
                }
            }
        }
        Ok(page)
    }

    /// Decodes page `id` of a file with `page_count` pages, validating every
    /// length, pointer and the key order.
    pub(crate) fn decode(page: &Page, id: PageId, page_count: u32) -> Result<Node> {
        let r = Reader::new(page.bytes(), id);
        let corrupt = |reason| StorageError::Corrupt { page: id, reason };
        let tag = r.u8(0)?;
        if tag != TAG_LEAF && tag != TAG_INTERNAL {
            return Err(corrupt("node tag"));
        }
        if r.u8(1)? != 0 {
            return Err(corrupt("node reserved byte"));
        }
        let count = usize::from(r.u16(OFF_COUNT)?);
        let link = r.u32(OFF_LINK)?;
        let mut offset = NODE_HEADER;
        let node = if tag == TAG_LEAF {
            if link == id.0 || link >= page_count {
                return Err(corrupt("next leaf pointer"));
            }
            let mut cells: Vec<LeafCell> = Vec::with_capacity(count);
            for _ in 0..count {
                let key_len = usize::from(r.u16(offset)?);
                let value_len = usize::from(r.u16(offset + 2)?);
                if key_len > MAX_KEY_LEN {
                    return Err(corrupt("key length"));
                }
                if value_len > MAX_VALUE_LEN {
                    return Err(corrupt("value length"));
                }
                let key = r.bytes(offset + 4, key_len)?;
                let value = r.bytes(offset + 4 + key_len, value_len)?;
                if cells.last().is_some_and(|(prev, _)| prev.as_slice() >= key) {
                    return Err(corrupt("keys not in ascending order"));
                }
                cells.push((key.to_vec(), value.to_vec()));
                offset += 4 + key_len + value_len;
            }
            let next = (link != 0).then_some(PageId(link));
            Node::Leaf(Leaf { cells, next })
        } else {
            if count == 0 {
                return Err(corrupt("internal node without keys"));
            }
            let valid_child = |child: u32| child != 0 && child != id.0 && child < page_count;
            if !valid_child(link) {
                return Err(corrupt("child pointer"));
            }
            let mut cells: Vec<InternalCell> = Vec::with_capacity(count);
            for _ in 0..count {
                let key_len = usize::from(r.u16(offset)?);
                if key_len > MAX_KEY_LEN {
                    return Err(corrupt("key length"));
                }
                let key = r.bytes(offset + 2, key_len)?;
                let child = r.u32(offset + 2 + key_len)?;
                if !valid_child(child) {
                    return Err(corrupt("child pointer"));
                }
                if cells.last().is_some_and(|(prev, _)| prev.as_slice() >= key) {
                    return Err(corrupt("keys not in ascending order"));
                }
                cells.push((key.to_vec(), PageId(child)));
                offset += 6 + key_len;
            }
            Node::Internal(Internal {
                leftmost: PageId(link),
                cells,
            })
        };
        let tail = PAGE_SIZE
            .checked_sub(offset)
            .ok_or(corrupt("cells overflow the page"))?;
        r.zeros(offset, tail, "node padding")?;
        Ok(node)
    }
}

fn len_u16(bytes: &[u8], id: PageId) -> Result<u16> {
    u16::try_from(bytes.len()).map_err(|_| StorageError::Corrupt {
        page: id,
        reason: "field longer than u16",
    })
}

/// Splits leaf cells at the boundary closest to the byte midpoint, keeping
/// both halves non-empty. Callers pass more than one page of cells, so each
/// half holds at least `LEAF_MIN_FILL` bytes.
pub(crate) fn split_leaf_cells(cells: Vec<LeafCell>) -> (Vec<LeafCell>, Vec<LeafCell>) {
    let total: usize = cells.iter().map(leaf_cell_size).sum();
    let mut best = 1;
    let mut best_diff = usize::MAX;
    let mut prefix = 0;
    for (k, cell) in cells.iter().enumerate().take(cells.len().saturating_sub(1)) {
        prefix += leaf_cell_size(cell);
        let diff = (2 * prefix).abs_diff(total);
        if diff < best_diff {
            best = k + 1;
            best_diff = diff;
        }
    }
    let mut left = cells;
    let right = left.split_off(best.min(left.len()));
    (left, right)
}

/// Splits internal cells around the cell containing the byte midpoint, which
/// is promoted to the parent. Returns `None` for fewer than three cells.
pub(crate) fn split_internal_cells(
    cells: Vec<InternalCell>,
) -> Option<(Vec<InternalCell>, InternalCell, Vec<InternalCell>)> {
    if cells.len() < 3 {
        return None;
    }
    let half = internal_payload(&cells) / 2;
    let mut prefix = 0;
    let mut mid = 0;
    for (i, cell) in cells.iter().enumerate() {
        prefix += internal_cell_size(cell);
        if prefix > half {
            mid = i;
            break;
        }
    }
    let mid = mid.clamp(1, cells.len() - 2);
    let mut left = cells;
    let mut rest = left.split_off(mid);
    let right = rest.split_off(1);
    let promoted = rest.pop()?;
    Some((left, promoted, right))
}

#[cfg(test)]
mod tests {
    use super::*;

    const ID: PageId = PageId(5);
    const PAGES: u32 = 100;

    fn leaf_cell(i: usize, key_len: usize, value_len: usize) -> LeafCell {
        let mut key = format!("{i:08}").into_bytes();
        key.resize(key_len.max(8), b'k');
        key.truncate(key_len);
        (key, vec![i as u8; value_len])
    }

    fn full_leaf() -> Leaf {
        let cells: Vec<LeafCell> = (0..40).map(|i| leaf_cell(i, 20, 76)).collect();
        Leaf {
            cells,
            next: Some(PageId(9)),
        }
    }

    fn full_internal() -> Internal {
        let cells = (0..15)
            .map(|i| (leaf_cell(i, MAX_KEY_LEN, 0).0, PageId(10 + i as u32)))
            .collect();
        Internal {
            leftmost: PageId(7),
            cells,
        }
    }

    fn round_trip(node: Node) -> Result<()> {
        let page = node.encode(ID)?;
        assert_eq!(Node::decode(&page, ID, PAGES)?, node);
        Ok(())
    }

    fn reason(page: &Page) -> &'static str {
        match Node::decode(page, ID, PAGES) {
            Err(StorageError::Corrupt { page: ID, reason }) => reason,
            other => panic!("expected Corrupt on page 5, got {other:?}"),
        }
    }

    #[test]
    fn constants_match_the_documented_numbers() {
        assert_eq!(PAYLOAD, 4088);
        assert_eq!(MAX_LEAF_CELL, 1284);
        assert_eq!(MAX_INTERNAL_CELL, 262);
        assert_eq!(REBALANCE_BELOW, 2044);
        assert_eq!(LEAF_MIN_FILL, 1402);
        assert_eq!(INTERNAL_MIN_FILL, 1782);
    }

    #[test]
    fn leaves_round_trip() -> Result<()> {
        round_trip(Node::Leaf(Leaf::default()))?;
        round_trip(Node::Leaf(Leaf {
            cells: vec![(vec![], vec![])],
            next: None,
        }))?;
        round_trip(Node::Leaf(full_leaf()))?;
        let max: Vec<LeafCell> = (0..3)
            .map(|i| leaf_cell(i, MAX_KEY_LEN, MAX_VALUE_LEN))
            .collect();
        let leaf = Leaf {
            cells: max,
            next: None,
        };
        assert!(leaf.payload() <= PAYLOAD, "three maximum-size cells fit");
        round_trip(Node::Leaf(leaf))
    }

    #[test]
    fn internals_round_trip() -> Result<()> {
        round_trip(Node::Internal(Internal {
            leftmost: PageId(1),
            cells: vec![(vec![], PageId(2))],
        }))?;
        let node = full_internal();
        assert!(
            node.payload() <= PAYLOAD,
            "fifteen maximum-size separators fit"
        );
        round_trip(Node::Internal(node))
    }

    #[test]
    fn leaf_layout_matches_documentation() -> Result<()> {
        let leaf = Leaf {
            cells: vec![(b"ab".to_vec(), b"xyz".to_vec())],
            next: Some(PageId(9)),
        };
        let page = Node::Leaf(leaf).encode(ID)?;
        let b = page.bytes();
        assert_eq!(b[..8], [1, 0, 1, 0, 9, 0, 0, 0]);
        assert_eq!(b[8..17], [2, 0, 3, 0, b'a', b'b', b'x', b'y', b'z']);
        assert!(b[17..].iter().all(|&x| x == 0));
        let node = Internal {
            leftmost: PageId(3),
            cells: vec![(b"m".to_vec(), PageId(4))],
        };
        let page = Node::Internal(node).encode(ID)?;
        assert_eq!(
            page.bytes()[..15],
            [2, 0, 1, 0, 3, 0, 0, 0, 1, 0, b'm', 4, 0, 0, 0]
        );
        Ok(())
    }

    #[test]
    fn child_lookup_follows_separators() {
        let node = Internal {
            leftmost: PageId(1),
            cells: vec![(b"b".to_vec(), PageId(2)), (b"d".to_vec(), PageId(3))],
        };
        assert_eq!(node.child_index(b"a"), 0);
        assert_eq!(node.child_index(b"b"), 1);
        assert_eq!(node.child_index(b"c"), 1);
        assert_eq!(node.child_index(b"d"), 2);
        assert_eq!(node.child_index(&[0xFF]), 2);
        assert_eq!(node.child(2, ID).ok(), Some(PageId(3)));
        assert!(node.child(3, ID).is_err());
    }

    /// Four maximum-size cells claimed on one page: the last runs past 4096.
    fn overrunning_leaf() -> Page {
        let mut page = Page::zeroed();
        let mut w = Writer::new(page.bytes_mut(), ID);
        w.put_u8(0, TAG_LEAF).expect("tag");
        w.put_u16(OFF_COUNT, 4).expect("count");
        for i in 0..3u8 {
            let offset = NODE_HEADER + usize::from(i) * MAX_LEAF_CELL;
            w.put_u16(offset, MAX_KEY_LEN as u16).expect("key len");
            w.put_u16(offset + 2, MAX_VALUE_LEN as u16)
                .expect("value len");
            w.put_u8(offset + 4, i + 1).expect("key");
        }
        let last = NODE_HEADER + 3 * MAX_LEAF_CELL;
        w.put_u16(last, MAX_KEY_LEN as u16).expect("key len");
        w.put_u8(last + 4, 9).expect("key");
        page
    }

    #[test]
    fn each_corruption_kind_is_reported() -> Result<()> {
        let leaf = Node::Leaf(full_leaf()).encode(ID)?;
        let internal = Node::Internal(full_internal()).encode(ID)?;
        let patch = |page: &Page, offset: usize, bytes: &[u8]| {
            let mut page = page.clone();
            page.bytes_mut()[offset..offset + bytes.len()].copy_from_slice(bytes);
            page
        };
        assert_eq!(reason(&patch(&leaf, 0, &[9])), "node tag");
        assert_eq!(reason(&patch(&leaf, 0, &[3])), "node tag");
        assert_eq!(reason(&patch(&leaf, 1, &[1])), "node reserved byte");
        assert_eq!(reason(&overrunning_leaf()), "field runs past end of page");
        assert_eq!(
            reason(&patch(&leaf, 4, &5u32.to_le_bytes())),
            "next leaf pointer"
        );
        assert_eq!(
            reason(&patch(&leaf, 4, &100u32.to_le_bytes())),
            "next leaf pointer"
        );
        assert_eq!(
            reason(&patch(&leaf, 8, &257u16.to_le_bytes())),
            "key length"
        );
        assert_eq!(
            reason(&patch(&leaf, 10, &1025u16.to_le_bytes())),
            "value length"
        );
        assert_eq!(
            reason(&patch(&leaf, 12, b"9")),
            "keys not in ascending order"
        );
        assert_eq!(
            reason(&patch(&leaf, 2, &39u16.to_le_bytes())),
            "node padding"
        );
        assert_eq!(
            reason(&patch(&internal, 2, &0u16.to_le_bytes())),
            "internal node without keys"
        );
        assert_eq!(
            reason(&patch(&internal, 4, &0u32.to_le_bytes())),
            "child pointer"
        );
        assert_eq!(
            reason(&patch(&internal, 4, &100u32.to_le_bytes())),
            "child pointer"
        );
        assert_eq!(
            reason(&patch(&internal, 266, &0u32.to_le_bytes())),
            "child pointer"
        );
        let swapped = Internal {
            leftmost: PageId(7),
            cells: vec![(b"b".to_vec(), PageId(8)), (b"a".to_vec(), PageId(9))],
        };
        assert_eq!(
            reason(&Node::Internal(swapped).encode(ID)?),
            "keys not in ascending order"
        );
        Ok(())
    }

    #[test]
    fn every_single_byte_flip_decodes_or_is_corrupt() -> Result<()> {
        for node in [Node::Leaf(full_leaf()), Node::Internal(full_internal())] {
            let original = node.encode(ID)?;
            for offset in 0..PAGE_SIZE {
                for mask in [0x01, 0x80, 0xFF] {
                    let mut page = original.clone();
                    page.bytes_mut()[offset] ^= mask;
                    match Node::decode(&page, ID, PAGES) {
                        Ok(_) | Err(StorageError::Corrupt { page: ID, .. }) => {}
                        Err(other) => panic!("unexpected error {other:?}"),
                    }
                }
            }
        }
        let mut ones = Page::zeroed();
        ones.bytes_mut().fill(0xFF);
        assert!(Node::decode(&ones, ID, PAGES).is_err());
        assert!(Node::decode(&Page::zeroed(), ID, PAGES).is_err());
        Ok(())
    }

    #[test]
    fn oversized_node_fails_to_encode() {
        let cells: Vec<LeafCell> = (0..4)
            .map(|i| leaf_cell(i, MAX_KEY_LEN, MAX_VALUE_LEN))
            .collect();
        let leaf = Leaf { cells, next: None };
        assert!(leaf.payload() > PAYLOAD);
        assert!(matches!(
            Node::Leaf(leaf).encode(ID),
            Err(StorageError::Corrupt { .. })
        ));
    }

    #[test]
    fn leaf_split_halves_respect_minimum_fill() {
        let mut rng = crate::test_common::XorShift64::new(crate::test_common::SEED);
        for _ in 0..2000 {
            let mut cells = Vec::new();
            let mut total = 0;
            let mut i = 0;
            while total <= PAYLOAD {
                let cell = leaf_cell(
                    i,
                    8 + rng.below(MAX_KEY_LEN - 7),
                    rng.below(MAX_VALUE_LEN + 1),
                );
                total += leaf_cell_size(&cell);
                cells.push(cell);
                i += 1;
            }
            let (left, right) = split_leaf_cells(cells);
            for half in [&left, &right] {
                let size: usize = half.iter().map(leaf_cell_size).sum();
                assert!(
                    (LEAF_MIN_FILL..=PAYLOAD).contains(&size),
                    "half of {size} bytes"
                );
            }
        }
    }

    #[test]
    fn internal_split_halves_respect_minimum_fill() {
        let mut rng = crate::test_common::XorShift64::new(crate::test_common::SEED);
        for _ in 0..2000 {
            let mut cells = Vec::new();
            let mut i = 0;
            while internal_payload(&cells) <= PAYLOAD {
                cells.push((leaf_cell(i, 8 + rng.below(MAX_KEY_LEN - 7), 0).0, PageId(2)));
                i += 1;
            }
            let (left, _, right) = split_internal_cells(cells).expect("enough cells");
            for half in [&left, &right] {
                let size = internal_payload(half);
                assert!(
                    (INTERNAL_MIN_FILL..=PAYLOAD).contains(&size),
                    "half of {size} bytes"
                );
            }
        }
        assert!(split_internal_cells(vec![(vec![], PageId(2)); 2]).is_none());
    }
}
