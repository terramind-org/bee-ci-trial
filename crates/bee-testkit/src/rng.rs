//! A seeded, portable random number generator for tests (SplitMix64).
//!
//! Every random choice in a test flows from one seed, so a failing case is reproduced by its
//! seed alone. The sequence is fixed forever: changing it would change every recorded seed.

use bee_core::{ChangeId, Ulid};

/// A SplitMix64 generator.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TestRng {
    state: u64,
}

impl TestRng {
    /// A generator from a seed.
    pub fn seed(seed: u64) -> TestRng {
        TestRng { state: seed }
    }

    /// The next 64 random bits.
    pub fn next_u64(&mut self) -> u64 {
        self.state = self.state.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.state;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    /// Uniform in `0..n` (`n > 0`).
    pub fn below(&mut self, n: u64) -> u64 {
        assert!(n > 0, "below(0)");
        // Rejection sampling keeps the distribution exactly uniform.
        let zone = u64::MAX - (u64::MAX % n);
        loop {
            let v = self.next_u64();
            if v < zone {
                return v % n;
            }
        }
    }

    /// Uniform index into a non-empty slice length.
    pub fn index(&mut self, len: usize) -> usize {
        self.below(len as u64) as usize
    }

    /// True with probability `num / den`.
    pub fn chance(&mut self, num: u64, den: u64) -> bool {
        self.below(den) < num
    }

    /// A random element of a non-empty slice.
    pub fn pick<'a, T>(&mut self, items: &'a [T]) -> &'a T {
        &items[self.index(items.len())]
    }

    /// Shuffles in place (Fisher–Yates).
    pub fn shuffle<T>(&mut self, items: &mut [T]) {
        for i in (1..items.len()).rev() {
            let j = self.below(i as u64 + 1) as usize;
            items.swap(i, j);
        }
    }

    /// `n` random bytes.
    pub fn bytes(&mut self, n: usize) -> Vec<u8> {
        let mut out = Vec::with_capacity(n);
        while out.len() < n {
            out.extend_from_slice(&self.next_u64().to_le_bytes());
        }
        out.truncate(n);
        out
    }

    /// A random non-zero ChangeId.
    pub fn change_id(&mut self) -> ChangeId {
        loop {
            let mut b = [0u8; 16];
            b.copy_from_slice(&self.bytes(16));
            if b != [0; 16] {
                return ChangeId::new(b);
            }
        }
    }

    /// A ULID with the given timestamp and random low bits.
    pub fn ulid(&mut self, timestamp_ms: u64) -> Ulid {
        let random = (u128::from(self.next_u64()) << 16 | u128::from(self.next_u64() & 0xffff))
            & ((1u128 << 80) - 1);
        let ts = timestamp_ms & ((1 << 48) - 1);
        Ulid::from_u128((u128::from(ts) << 80) | random)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequence_is_frozen() {
        let mut r = TestRng::seed(0);
        // SplitMix64 reference outputs for seed 0.
        assert_eq!(r.next_u64(), 0xe220_a839_7b1d_cdaf);
        assert_eq!(r.next_u64(), 0x6e78_9e6a_a1b9_65f4);
    }

    #[test]
    fn helpers_are_deterministic_and_in_range() {
        let mut a = TestRng::seed(9);
        let mut b = TestRng::seed(9);
        for _ in 0..100 {
            let n = a.below(7);
            assert_eq!(n, b.below(7));
            assert!(n < 7);
        }
        assert_eq!(a.bytes(13).len(), 13);
        assert_ne!(a.change_id(), ChangeId::ZERO);
        let u = a.ulid(1234);
        assert_eq!(u.timestamp_ms(), 1234);
        let mut v = vec![1, 2, 3, 4, 5];
        a.shuffle(&mut v);
        v.sort();
        assert_eq!(v, vec![1, 2, 3, 4, 5]);
        assert!([1, 2].contains(a.pick(&[1, 2])));
        let hits = (0..1000).filter(|_| a.chance(1, 4)).count();
        assert!((150..350).contains(&hits));
    }
}
