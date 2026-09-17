//! Pure mixing for the `AT_RANDOM` fallback (issue #16).
//!
//! WHY THIS MODULE EXISTS. `arch::x86_64::linux::misc::random_bytes_16()` produces
//! the ELF `AT_RANDOM` block — what glibc turns into the stack-canary and
//! pointer-mangling keys — and prefers hardware entropy (RDSEED/RDRAND). When the
//! CPU exposes neither, it used to fall back to a 64-bit **xorshift** seeded from
//! the tick clock, the RTC wall clock, the pid and a fixed constant. That is a
//! linear generator over values an attacker observes or guesses (boot time, tick
//! rate, pid), so every process's canary was predictable. `sys_getrandom` stayed
//! fail-closed (`EAGAIN`) while only `AT_RANDOM` degraded silently, which is why
//! the weakness could sit in the tree unnoticed.
//!
//! WHAT REPLACED IT. Each block is derived as `SHA-256(pool ‖ counter ‖
//! observables)` — a one-way, fully diffusing function — over a **boot seed**
//! that absorbed several independent boot-time sources (see
//! [`super::entropy::mixed_fill`] for the effectful collection: TSC jitter, RTC
//! port-read races, kernel counters/heap state, address-space layout). The
//! counter makes two processes with *identical* observable inputs distinct, and
//! the seed makes the output independent of what the attacker can observe.
//!
//! HONEST SCOPE — this is best-effort, **not** a CSPRNG:
//!
//!   * it is a *fallback*: with RDSEED/RDRAND present nothing here runs;
//!   * on a hypothetical fully deterministic platform the collected boot seed is
//!     reproducible (and so are the canaries), which is exactly why the
//!     degradation is ANNOUNCED at boot instead of being silent, and why
//!     `sys_getrandom` keeps failing closed instead of serving these bytes;
//!   * it is not a substitute for hardware entropy, and `SECURITY.md` says so.
//!
//! The module is `core`-only plus `sha2` and deliberately effect-free, so the host
//! property tests (`host-tests/src/properties/at_random.rs`) exercise the exact source
//! the kernel compiles: avalanche, no output dependence on observables alone, and
//! no repeats under a fixed observable tuple.

use sha2::{Digest, Sha256};

/// Domain-separation tag. Mixed into the seed transcript and every derivation so
/// bytes produced here can never collide with another SHA-256 use in the kernel
/// (the TLS transcript, certificate fingerprints, …) even for identical inputs.
pub const DOMAIN: &[u8] = b"pagh/at-random/v1";

/// Derivation tag: distinguishes these bytes from any other SHA-256 use in the
/// kernel that happens to see the same seed/counter/observables.
const DERIVE_TAG: u8 = 0x41;

/// A transcript of independent boot-time sources, folded into one 32-byte seed.
///
/// `absorb`/`absorb_bytes` are length-prefixed and labelled, so
/// `absorb(b"tick", 0x1234)` and `absorb(b"tick1", 0x234)` cannot produce the same
/// transcript (no concatenation ambiguity).
#[derive(Clone)]
pub struct SeedPool {
    hasher: Sha256,
    sources: u32,
}

impl Default for SeedPool {
    fn default() -> Self {
        SeedPool::new()
    }
}

impl SeedPool {
    /// Start an empty transcript bound to [`DOMAIN`].
    pub fn new() -> Self {
        let mut hasher = Sha256::new();
        hasher.update((DOMAIN.len() as u64).to_le_bytes());
        hasher.update(DOMAIN);
        SeedPool { hasher, sources: 0 }
    }

    /// Absorb one labelled 64-bit source value.
    pub fn absorb(&mut self, label: &[u8], value: u64) -> &mut Self {
        self.absorb_bytes(label, &value.to_le_bytes())
    }

    /// Absorb one labelled byte string.
    pub fn absorb_bytes(&mut self, label: &[u8], bytes: &[u8]) -> &mut Self {
        self.hasher.update((label.len() as u64).to_le_bytes());
        self.hasher.update(label);
        self.hasher.update((bytes.len() as u64).to_le_bytes());
        self.hasher.update(bytes);
        self.sources = self.sources.saturating_add(1);
        self
    }

    /// How many source samples were absorbed (diagnostics / self-tests).
    pub fn sources(&self) -> u32 {
        self.sources
    }

    /// Finish the transcript: the 32-byte boot seed.
    pub fn finish(self) -> [u8; 32] {
        let out = self.hasher.finalize();
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&out);
        seed
    }
}

/// The observable inputs of one `AT_RANDOM` derivation.
///
/// These are mixed in — but they are **not** the security of the block: an
/// attacker who knows all three exactly still cannot compute the output without
/// the boot seed (property `observables_alone_do_not_determine_output`). They
/// exist so that a seed collision on a deterministic platform still cannot yield
/// the same canary for two processes with different observable inputs.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Observables {
    /// `scheduler::ticks()` at derivation time (monotonic tick clock).
    pub ticks: u64,
    /// CMOS RTC wall clock, Unix seconds (`linux::rtc::now_unix`).
    pub rtc_unix: u64,
    /// `scheduler::current_pid()`.
    pub pid: u64,
}

/// One SHA-256 derivation step: `DOMAIN ‖ tag ‖ seed ‖ counter ‖ observables`.
fn derive32(seed: &[u8; 32], counter: u64, obs: Observables, tag: u8) -> [u8; 32] {
    let mut hasher = Sha256::new();
    hasher.update((DOMAIN.len() as u64).to_le_bytes());
    hasher.update(DOMAIN);
    hasher.update([tag]);
    hasher.update(seed);
    hasher.update(counter.to_le_bytes());
    hasher.update(obs.ticks.to_le_bytes());
    hasher.update(obs.rtc_unix.to_le_bytes());
    hasher.update(obs.pid.to_le_bytes());
    let out = hasher.finalize();
    let mut digest = [0u8; 32];
    digest.copy_from_slice(&out);
    digest
}

/// Fill `dest` from `seed`, one 16-byte SHA-256 chunk at a time.
///
/// This is the single derivation used by the kernel (`entropy::mixed_fill` calls
/// it with a 16-byte `AT_RANDOM` block) and by the host property tests, so the
/// tests exercise the shipped path. The counter advances per chunk and must be
/// unique within a boot — the kernel feeds a monotonic sequence — which is what
/// makes two processes with *identical* observable inputs (ticks/RTC/pid) get
/// different blocks.
pub fn derive_bytes(seed: &[u8; 32], counter: u64, obs: Observables, dest: &mut [u8]) {
    let mut offset = 0usize;
    let mut block_counter = counter;
    while offset < dest.len() {
        let digest = derive32(seed, block_counter, obs, DERIVE_TAG);
        let n = core::cmp::min(16, dest.len() - offset);
        dest[offset..offset + n].copy_from_slice(&digest[..n]);
        offset += n;
        block_counter = block_counter.wrapping_add(1);
    }
}
