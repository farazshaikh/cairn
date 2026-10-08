//! Seeded corruption of database and log files for the file robustness
//! target. Field offsets follow the README "Header page" table.

use crate::rng::Rng;

/// One page of a database file.
pub const PAGE: usize = 4096;

/// A corruption to apply to a file image.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mutation {
    /// Flip 1 to 8 random bits.
    BitFlip,
    /// Overwrite 1 to 16 bytes with 0x00, 0xFF or random bytes.
    ByteSet,
    /// Write an edge value into a header field: page count, free-list head
    /// or length, a root slot's name length or root page.
    HeaderField,
    /// Swap two whole pages.
    PageSwap,
    /// Cut the file at a random length, not always a page multiple.
    Truncate,
    /// Append 1 to 5000 random bytes.
    Append,
}

/// Every mutation with its share of cases, in percent.
pub const MUTATIONS: [(Mutation, u64); 6] = [
    (Mutation::BitFlip, 30),
    (Mutation::ByteSet, 20),
    (Mutation::HeaderField, 20),
    (Mutation::PageSwap, 10),
    (Mutation::Truncate, 10),
    (Mutation::Append, 10),
];

/// Picks a mutation by the shares in [`MUTATIONS`].
pub fn pick(rng: &mut Rng) -> Mutation {
    let mut roll = rng.below(100);
    for (mutation, share) in MUTATIONS {
        if roll < share {
            return mutation;
        }
        roll -= share;
    }
    Mutation::BitFlip
}

/// Header fields as (offset, size): page count, free-list head and length,
/// version, page size, and each root slot's name length and root page id.
fn header_fields() -> Vec<(usize, usize)> {
    let mut fields = vec![(16, 4), (20, 4), (24, 4), (8, 4), (12, 4)];
    for slot in 0..16 {
        let base = 64 + slot * 40;
        fields.push((base, 1));
        fields.push((base + 36, 4));
    }
    fields
}

/// Applies `mutation` to `bytes`. An empty image only grows.
pub fn apply(mutation: Mutation, bytes: &mut Vec<u8>, rng: &mut Rng) {
    if bytes.is_empty() && mutation != Mutation::Append {
        return apply(Mutation::Append, bytes, rng);
    }
    match mutation {
        Mutation::BitFlip => {
            for _ in 0..rng.range(1, 8) {
                let i = rng.index(bytes.len());
                if let Some(b) = bytes.get_mut(i) {
                    *b ^= 1 << rng.below(8);
                }
            }
        }
        Mutation::ByteSet => {
            let start = rng.index(bytes.len());
            let end = (start + rng.index(16) + 1).min(bytes.len());
            let fill = rng.below(3);
            for i in start..end {
                let value = match fill {
                    0 => 0x00,
                    1 => 0xFF,
                    _ => rng.next_u64() as u8,
                };
                if let Some(b) = bytes.get_mut(i) {
                    *b = value;
                }
            }
        }
        Mutation::HeaderField => {
            let fields = header_fields();
            let (offset, size) = *rng.pick(&fields);
            let pages = (bytes.len() / PAGE) as u32;
            let random = rng.next_u64() as u32;
            let value: u32 = *rng.pick(&[
                0,
                1,
                2,
                pages.saturating_sub(1),
                pages,
                pages.saturating_add(1),
                u32::MAX,
                random,
            ]);
            for (i, byte) in value.to_le_bytes().iter().take(size).enumerate() {
                if let Some(b) = bytes.get_mut(offset + i) {
                    *b = *byte;
                }
            }
        }
        Mutation::PageSwap => {
            let pages = bytes.len() / PAGE;
            if pages < 2 {
                return apply(Mutation::BitFlip, bytes, rng);
            }
            let a = rng.index(pages);
            let b = (a + 1 + rng.index(pages - 1)) % pages;
            let (lo, hi) = (a.min(b) * PAGE, a.max(b) * PAGE);
            let (left, right) = bytes.split_at_mut(hi);
            if let (Some(x), Some(y)) = (left.get_mut(lo..lo + PAGE), right.get_mut(..PAGE)) {
                x.swap_with_slice(y);
            }
        }
        Mutation::Truncate => {
            let len = if rng.chance(1, 2) {
                rng.index(bytes.len().div_ceil(PAGE)) * PAGE
            } else {
                rng.index(bytes.len())
            };
            bytes.truncate(len);
        }
        Mutation::Append => {
            let extra = rng.range(1, 5000) as usize;
            bytes.extend((0..extra).map(|_| rng.next_u64() as u8));
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn image() -> Vec<u8> {
        (0..3 * PAGE).map(|i| (i / PAGE) as u8 + 1).collect()
    }

    #[test]
    fn in_place_mutations_keep_the_length() {
        let mut rng = Rng::new(7);
        for mutation in [Mutation::BitFlip, Mutation::ByteSet, Mutation::HeaderField] {
            let mut changed = 0;
            for _ in 0..50 {
                let mut bytes = image();
                apply(mutation, &mut bytes, &mut rng);
                assert_eq!(bytes.len(), 3 * PAGE, "{mutation:?}");
                if bytes != image() {
                    changed += 1;
                }
                if mutation == Mutation::HeaderField {
                    assert_eq!(
                        bytes[PAGE..],
                        image()[PAGE..],
                        "header writes stay in page 0"
                    );
                }
            }
            assert!(
                changed > 25,
                "{mutation:?} changed only {changed} of 50 images"
            );
        }
    }

    #[test]
    fn page_swaps_exchange_whole_pages() {
        let mut rng = Rng::new(3);
        let mut bytes = image();
        apply(Mutation::PageSwap, &mut bytes, &mut rng);
        assert!(bytes.chunks(PAGE).all(|p| p.iter().all(|b| *b == p[0])));
        let mut firsts: Vec<u8> = bytes.chunks(PAGE).map(|p| p[0]).collect();
        assert_ne!(firsts, vec![1, 2, 3]);
        firsts.sort_unstable();
        assert_eq!(firsts, vec![1, 2, 3]);
    }

    #[test]
    fn truncation_shortens_and_appending_grows() {
        let mut rng = Rng::new(11);
        for _ in 0..50 {
            let mut bytes = image();
            apply(Mutation::Truncate, &mut bytes, &mut rng);
            assert!(bytes.len() < 3 * PAGE);
            let mut grown = image();
            apply(Mutation::Append, &mut grown, &mut rng);
            assert!(grown.len() > 3 * PAGE && grown.len() <= 3 * PAGE + 5000);
            assert_eq!(grown[..3 * PAGE], image()[..]);
        }
        let mut empty = Vec::new();
        apply(Mutation::Truncate, &mut empty, &mut rng);
        assert!(!empty.is_empty());
    }

    #[test]
    fn picks_follow_the_shares() {
        let mut rng = Rng::new(5);
        let mut counts = [0u32; 6];
        for _ in 0..10_000 {
            let m = pick(&mut rng);
            let i = MUTATIONS.iter().position(|(x, _)| *x == m).unwrap_or(0);
            counts[i] += 1;
        }
        for ((_, share), count) in MUTATIONS.iter().zip(counts) {
            let expected = *share as u32 * 100;
            assert!(count.abs_diff(expected) < 400, "{count} vs {expected}");
        }
    }
}
