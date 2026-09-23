//! SplitMix64: a small, deterministic RNG with no dependencies, shared by crash plans and
//! bench generators. Not cryptographic; only reproducibility matters here.

/// A SplitMix64 generator, seeded once and then advanced by `next_u64`.
pub struct SplitMix64(u64);

impl SplitMix64 {
    pub fn new(seed: u64) -> Self {
        SplitMix64(seed)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e3779b97f4a7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58476d1ce4e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d049bb133111eb);
        z ^ (z >> 31)
    }

    /// Uniform in `lo..hi` (`hi` exclusive, `lo < hi`).
    pub fn range(&mut self, lo: u64, hi: u64) -> u64 {
        lo + self.next_u64() % (hi - lo)
    }

    /// Uniform in `[0, 1)`, from the top 53 bits (a `f64` mantissa's worth of entropy).
    pub fn f64(&mut self) -> f64 {
        const SCALE: f64 = 1.0 / (1u64 << 53) as f64;
        (self.next_u64() >> 11) as f64 * SCALE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn same_seed_same_sequence() {
        let mut a = SplitMix64::new(42);
        let mut b = SplitMix64::new(42);
        for _ in 0..100 {
            assert_eq!(a.next_u64(), b.next_u64());
        }
    }

    #[test]
    fn range_stays_in_bounds() {
        let mut rng = SplitMix64::new(7);
        for _ in 0..10_000 {
            let n = rng.range(5, 9);
            assert!((5..9).contains(&n));
        }
    }

    #[test]
    fn f64_stays_in_unit_interval() {
        let mut rng = SplitMix64::new(1234);
        for _ in 0..10_000 {
            let f = rng.f64();
            assert!((0.0..1.0).contains(&f));
        }
    }
}
