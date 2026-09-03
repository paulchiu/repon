//! A tiny deterministic generator so a corpus built from one seed compares to a corpus
//! built from the same seed later, on any machine, with no external `rand` dependency.
//! splitmix64: not cryptographic, only reproducible.
pub struct Rng(u64);

impl Rng {
    pub fn new(seed: u64) -> Self {
        Rng(seed)
    }

    fn next_u64(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9E3779B97F4A7C15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58476D1CE4E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D049BB133111EB);
        z ^ (z >> 31)
    }

    /// A value in `[lo, hi)`. Panics if `hi <= lo`.
    pub fn range(&mut self, lo: usize, hi: usize) -> usize {
        assert!(hi > lo, "empty range [{lo}, {hi})");
        lo + (self.next_u64() % (hi - lo) as u64) as usize
    }

    /// True with probability `p` (`0.0..=1.0`).
    pub fn chance(&mut self, p: f64) -> bool {
        (self.next_u64() as f64 / u64::MAX as f64) < p
    }
}
