//! Seeded xorshift64* generator, shared by the benches and the soak. No `rand`
//! dependency: a given seed always gives the same sequence.

/// Seeded xorshift64* — no `rand` dependency, identical for a given seed.
#[derive(Debug, Clone)]
pub(crate) struct Rng(u64);

/// SplitMix64 finaliser: a well-mixed 64-bit value from any input.
pub(crate) fn mix(x: u64) -> u64 {
    let mut z = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
    z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
    z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
    z ^ (z >> 31)
}

impl Rng {
    pub(crate) fn new(seed: u64) -> Self {
        Self(seed)
    }

    /// An independent stream for `(a, b)`: `Rng::new(mix(a ^ mix(b)) | 1)`.
    /// The `| 1` keeps xorshift away from the all-zero state.
    pub(crate) fn stream(a: u64, b: u64) -> Self {
        Self::new(mix(a ^ mix(b)) | 1)
    }

    pub(crate) fn next(&mut self) -> u64 {
        let mut x = self.0;
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        self.0 = x;
        x.wrapping_mul(0x2545_F491_4F6C_DD1D)
    }

    /// Uniform `f64` in `[0, 1)`.
    pub(crate) fn unit(&mut self) -> f64 {
        (self.next() >> 11) as f64 / (1_u64 << 53) as f64
    }

    pub(crate) fn below(&mut self, bound: usize) -> usize {
        (self.next() % bound as u64) as usize
    }

    /// Standard normal via Box–Muller.
    pub(crate) fn normal(&mut self) -> f64 {
        let u1 = self.unit().max(f64::MIN_POSITIVE);
        let u2 = self.unit();
        (-2.0 * u1.ln()).sqrt() * (std::f64::consts::TAU * u2).cos()
    }

    /// Log-normal with a given median and p99.
    pub(crate) fn lognormal(&mut self, median: f64, p99: f64) -> f64 {
        let sigma = (p99 / median).ln() / 2.326_347_874_040_840_8;
        (median.ln() + sigma * self.normal()).exp()
    }
}
