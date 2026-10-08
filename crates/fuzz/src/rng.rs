//! The xorshift64 generator (Marsaglia, shifts 13, 7, 17) and per-case
//! seeds.

/// A xorshift64 generator. Its state is never zero.
#[derive(Debug, Clone)]
pub struct Rng(u64);

impl Rng {
    /// A generator seeded with `seed`.
    ///
    /// # Panics
    ///
    /// When `seed` is zero, which xorshift never leaves.
    pub fn new(seed: u64) -> Rng {
        assert_ne!(seed, 0, "seed must be non-zero");
        Rng(seed)
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        self.0 = x;
        x
    }

    /// A value in `0..n`; `n` must be positive. The slight modulo bias is
    /// irrelevant for fuzzing.
    pub fn below(&mut self, n: u64) -> u64 {
        self.next_u64() % n.max(1)
    }

    /// An index into a collection of `len` items.
    pub fn index(&mut self, len: usize) -> usize {
        usize::try_from(self.below(len as u64)).unwrap_or(0)
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    /// A uniformly chosen element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        let i = self.index(items.len());
        &items[i]
    }

    /// A value in `low..=high`.
    pub fn range(&mut self, low: i64, high: i64) -> i64 {
        let span = high.wrapping_sub(low) as u64;
        low.wrapping_add(self.below(span.wrapping_add(1).max(1)) as i64)
    }
}

/// The seed of case `case` in a run seeded with `seed`: splitmix64 of the
/// two, so every case can be replayed on its own. Never zero.
pub fn case_seed(seed: u64, case: u32) -> u64 {
    let mut z = seed ^ u64::from(case).wrapping_mul(0x9E37_79B9_7F4A_7C15);
    z = z.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^= z >> 31;
    if z == 0 { 1 } else { z }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_sequence_is_the_published_xorshift64() {
        let mut rng = Rng::new(1);
        assert_eq!(rng.next_u64(), 1_082_269_761);
        assert_eq!(rng.next_u64(), 1_152_992_998_833_853_505);
    }

    #[test]
    fn helpers_stay_in_range() {
        let mut rng = Rng::new(42);
        for _ in 0..10_000 {
            assert!(rng.below(7) < 7);
            let v = rng.range(-3, 3);
            assert!((-3..=3).contains(&v));
            assert!([1, 2, 3].contains(rng.pick(&[1, 2, 3])));
        }
        let full = rng.range(i64::MIN, i64::MAX);
        let _ = full;
    }

    #[test]
    #[should_panic(expected = "seed must be non-zero")]
    fn a_zero_seed_is_rejected() {
        let _ = Rng::new(0);
    }
}
