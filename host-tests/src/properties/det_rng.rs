// Shared deterministic test RNG for the TLS property tests (P46+).
//
// A XorShift128+ style RNG implementing rand_core's traits, seeded from
// proptest-provided values: every run replays the same keys/signatures — no
// OsRng, no getrandom, replayable failures. Extracted from P46 so later
// properties of the issue #14 series (P47 chain building, …) reuse ONE
// implementation instead of drifting copies.

use rand_core::{CryptoRng, RngCore};

/// Deterministic XorShift128+ style RNG (seeded from proptest values).
pub struct DetRng(u64, u64);

impl RngCore for DetRng {
    fn next_u32(&mut self) -> u32 {
        (self.next_u64() >> 32) as u32
    }
    fn next_u64(&mut self) -> u64 {
        // xorshift128+ (linear, deterministic, fine for test key generation).
        let mut a = self.0;
        let b = self.1;
        self.0 = b;
        a ^= a << 23;
        a ^= a >> 17;
        a ^= b ^ (b >> 26);
        self.1 = a;
        a.wrapping_add(b)
    }
    fn fill_bytes(&mut self, dest: &mut [u8]) {
        for chunk in dest.chunks_mut(8) {
            let v = self.next_u64().to_le_bytes();
            chunk.copy_from_slice(&v[..chunk.len()]);
        }
    }
    fn try_fill_bytes(&mut self, dest: &mut [u8]) -> Result<(), rand_core::Error> {
        self.fill_bytes(dest);
        Ok(())
    }
}

impl CryptoRng for DetRng {}

/// Seed the RNG from a proptest value (mixed so related seeds diverge early).
pub fn rng_from(seed: u64) -> DetRng {
    DetRng(seed ^ 0x9E3779B97F4A7C15, seed.rotate_left(32) | 1)
}
