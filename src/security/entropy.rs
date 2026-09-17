//! Hardware-backed entropy for x86_64, plus the `AT_RANDOM` fallback mixer.
//!
//! The API fails closed: it never substitutes timestamps or a linear PRNG when
//! cryptographic randomness was requested. RDSEED is preferred; RDRAND is the
//! compatibility fallback. Callers must propagate `Unavailable`.
//!
//! ONE caller cannot do that: `AT_RANDOM` (see [`mixed_fill`]) is read
//! unconditionally by glibc at process start, so refusing to produce it means
//! refusing to start the process. Instead of a 64-bit xorshift over the tick
//! clock / RTC / pid — a *linear* stream over values an attacker observes or
//! guesses (issue #16) — that path now collects several independent boot-time
//! sources into one SHA-256 seed ([`super::seed`]), derives every block from it,
//! and ANNOUNCES the degradation at boot. The honest scope is in the
//! [`super::seed`] module docs: best-effort, not a CSPRNG.

use core::arch::x86_64::{__cpuid, _rdrand64_step, _rdseed64_step};
use core::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use crate::sync::spinlock::Spinlock;

use super::seed::{self, Observables};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropyError {
    Unavailable,
}

#[inline]
fn capabilities() -> (bool, bool) {
    // CPUID is an unprivileged architectural query and, on this toolchain,
    // `__cpuid` is a SAFE function: wrapping it in `unsafe` was redundant (the
    // compiler warns about exactly that). Hardware-entropy support is decoded from
    // leaf 1 ECX bit 30 (RDRAND) and leaf 7 EBX bit 18 (RDSEED).
    let leaf1 = __cpuid(1);
    let leaf7 = __cpuid(7);
    let rdrand = (leaf1.ecx & (1 << 30)) != 0;
    let rdseed = (leaf7.ebx & (1 << 18)) != 0;
    (rdseed, rdrand)
}

pub fn is_available() -> bool {
    let (rdseed, rdrand) = capabilities();
    rdseed || rdrand
}

/// Which hardware entropy instructions this CPU exposes, for diagnostics.
///
/// The TLS path refuses to run without one of them, and that refusal is easy
/// to misread as a network failure: under QEMU the default `qemu64` CPU model
/// exposes NEITHER instruction, so every live HTTPS check reports
/// `cause=entropy` until the guest is booted with `-cpu max` (or any other
/// model that carries RDSEED/RDRAND). `tools/build.py run` and the `e2e_*.ps1`
/// scripts pass `-cpu max` for exactly this reason.
pub fn capabilities_str() -> &'static str {
    let (rdseed, rdrand) = capabilities();
    match (rdseed, rdrand) {
        (true, true) => "rdseed+rdrand",
        (true, false) => "rdseed",
        (false, true) => "rdrand",
        (false, false) => "none (boot QEMU with -cpu max to expose RDSEED/RDRAND)",
    }
}

#[target_feature(enable = "rdseed")]
unsafe fn rdseed_word() -> Option<u64> {
    let mut value = 0u64;
    for _ in 0..128 {
        // `_rdseed64_step` is a SAFE intrinsics wrapper on this toolchain; the
        // CPUID.RDSEED precondition is carried by this function's
        // `#[target_feature(enable = "rdseed")]` and by `secure_u64` checking
        // CPUID before calling it.
        if _rdseed64_step(&mut value) == 1 {
            return Some(value);
        }
        core::hint::spin_loop();
    }
    None
}

#[target_feature(enable = "rdrand")]
unsafe fn rdrand_word() -> Option<u64> {
    let mut value = 0u64;
    for _ in 0..128 {
        // `_rdrand64_step` is a SAFE intrinsics wrapper on this toolchain; see
        // `rdseed_word` for where the CPUID precondition is enforced.
        if _rdrand64_step(&mut value) == 1 {
            return Some(value);
        }
        core::hint::spin_loop();
    }
    None
}

pub fn secure_u64() -> Result<u64, EntropyError> {
    let (rdseed, rdrand) = capabilities();
    if rdseed {
        // SAFETY: CPUID reported RDSEED support.
        if let Some(v) = unsafe { rdseed_word() } {
            return Ok(v);
        }
    }
    if rdrand {
        // SAFETY: CPUID reported RDRAND support.
        if let Some(v) = unsafe { rdrand_word() } {
            return Ok(v);
        }
    }
    Err(EntropyError::Unavailable)
}

pub fn fill(dest: &mut [u8]) -> Result<(), EntropyError> {
    let mut offset = 0usize;
    while offset < dest.len() {
        let word = secure_u64()?.to_le_bytes();
        let n = core::cmp::min(8, dest.len() - offset);
        dest[offset..offset + n].copy_from_slice(&word[..n]);
        offset += n;
    }
    Ok(())
}

// ─────────────────────────── AT_RANDOM fallback (issue #16) ───────────────────

/// Upper bound on TSC-jitter samples taken while building the boot seed.
const JITTER_MAX_ITERATIONS: u32 = 4096;
/// Cycle budget for the same loop (~tens of microseconds), so the collection is
/// bounded even on a machine where the iteration cap is never reached.
const JITTER_CYCLES: u64 = 200_000;
/// CMOS RTC port-read samples absorbed into the seed.
const RTC_SAMPLES: usize = 4;

/// Emitted once, the first time the `AT_RANDOM` path discovers it has no
/// hardware entropy (or at the boot probe, whichever comes first).
static DEGRADED_WARNED: AtomicBool = AtomicBool::new(false);
/// Monotonic per-boot derivation counter: makes every block unique even when the
/// observable inputs are identical (two processes with the same pid-shaped inputs).
static DERIVE_SEQ: AtomicU64 = AtomicU64::new(0);
/// The mixed boot seed, built once from [`collect_boot_seed`].
static BOOT_SEED: Spinlock<Option<[u8; 32]>> = Spinlock::new(None);

/// Read the time-stamp counter.
#[inline]
fn rdtsc() -> u64 {
    // SAFETY: RDTSC is an unprivileged x86_64 instruction and this wrapper only
    // reads the counter — no memory, no flags, no side effects.
    unsafe { core::arch::x86_64::_rdtsc() }
}

/// Announce (once) that `AT_RANDOM` is running without hardware entropy.
///
/// Called by [`mixed_fill`] and by the boot probe in `boot::kernel_main`, so the
/// operator sees the degradation even before the first process starts. The
/// message names the CPU capability string, because "no RDSEED/RDRAND" is
/// usually a `-cpu`/firmware choice (QEMU's default `qemu64` model exposes
/// neither).
fn warn_degraded_once() {
    if DEGRADED_WARNED.swap(true, Ordering::Relaxed) {
        return;
    }
    crate::warn!(
        "entropy: stage=degraded cause=NoHardwareEntropy cpu={} \
         (AT_RANDOM derives from the mixed boot seed: best-effort, NOT a CSPRNG; \
         sys_getrandom stays fail-closed with EAGAIN)",
        capabilities_str()
    );
}

/// Boot-time capability probe: warns when the `AT_RANDOM` fallback will be used.
///
/// Deliberately side-effect-free when hardware entropy IS available (no log
/// line), so a healthy boot log is unchanged.
pub fn report_capabilities() {
    if !is_available() {
        warn_degraded_once();
    }
}

/// Collect several INDEPENDENT boot-time sources into one 32-byte seed.
///
/// Sources, in the order absorbed (all labelled, all length-prefixed, see
/// [`seed::SeedPool`]):
///
///   1. **TSC jitter** — the deltas between back-to-back `RDTSC` reads at the
///      same call site. On bare metal these vary with interrupts, cache state and
///      SMT; under QEMU/TCG they come from the host clock, so they vary with host
///      scheduling. This is the source an attacker cannot reconstruct from the
///      tick clock, the wall clock or the pid.
///   2. **RTC port-read race/latency** — several CMOS reads with their cycle
///      costs; the read-twice guard inside `rtc` also exposes update-boundary
///      races.
///   3. **Kernel counters/state** — tick count, current/next pid, PMM frame
///      counts, heap accounting.
///   4. **Address-space layout** — CR3, plus the addresses of the sampled objects
///      (stack/heap placement).
///
/// NOT used: uninitialized memory (reading it is not sound Rust) and anything the
/// caller already has. The honest strength statement is in [`super::seed`]:
/// timing jitter is what makes this a seed rather than a formula, and on a
/// perfectly deterministic replay it is reproducible — which is why the
/// degradation is warned about and `sys_getrandom` never serves these bytes.
fn collect_boot_seed() -> [u8; 32] {
    let mut pool = seed::SeedPool::new();

    // 1. TSC jitter, bounded by iterations AND by cycles.
    let start = rdtsc();
    let mut last = start;
    let mut jitter = 0u64;
    let mut iterations: u32 = 0;
    loop {
        let now = rdtsc();
        jitter = jitter.rotate_left(5) ^ now.wrapping_sub(last);
        last = now;
        iterations += 1;
        if iterations >= JITTER_MAX_ITERATIONS || now.wrapping_sub(start) >= JITTER_CYCLES {
            break;
        }
    }
    pool.absorb(b"tsc.start", start);
    pool.absorb(b"tsc.jitter", jitter);
    pool.absorb(b"tsc.iters", iterations as u64);

    // 2. RTC wall-clock reads + the cycle cost of each (port I/O latency).
    for i in 0..RTC_SAMPLES {
        let before = rdtsc();
        let secs = crate::arch::x86_64::linux::rtc::now_unix();
        let after = rdtsc();
        pool.absorb(b"rtc.secs", secs);
        pool.absorb(b"rtc.latency", after.wrapping_sub(before));
        pool.absorb(b"rtc.index", i as u64);
    }

    // 3. Kernel counters and state.
    pool.absorb(b"tick", crate::task::scheduler::ticks());
    pool.absorb(b"pid.cur", crate::task::scheduler::current_pid());
    pool.absorb(b"pid.next", crate::task::scheduler::next_pid());
    pool.absorb(b"pmm.free", crate::memory::pmm::free_frames() as u64);
    pool.absorb(b"pmm.total", crate::memory::pmm::total_frames() as u64);
    let (heap_size, heap_used, heap_free) = crate::memory::heap::stats();
    pool.absorb(b"heap.size", heap_size as u64);
    pool.absorb(b"heap.used", heap_used as u64);
    pool.absorb(b"heap.free", heap_free as u64);

    // 4. Address-space layout: the page table in use and where these objects live.
    pool.absorb(b"cr3", crate::memory::vmm::current_pml4_phys());
    pool.absorb(b"addr.pool", core::ptr::addr_of!(pool) as u64);
    pool.absorb(b"addr.absorbed", pool.sources() as u64);

    pool.finish()
}

/// One-way fingerprint of the boot seed, for diagnostics and in-QEMU evidence.
///
/// Returns the first 8 bytes of `SHA-256(DOMAIN ‖ "fingerprint" ‖ seed)`. Two
/// properties make it safe to print: it is one-way over the 32-byte seed (the
/// seed cannot be recovered from 8 bytes of a hash), and it depends ONLY on the
/// seed — not on the counter and not on the observables — so two boots whose
/// fingerprints differ demonstrably collected DIFFERENT boot seeds, which is the
/// property that makes the fallback non-reproducible. Compare it with the
/// per-run `SELFTEST at_random: … digest=…` line, which mixes in the observables
/// and therefore varies even when the seed does not.
///
/// Builds the seed if it does not exist yet (first caller wins); never panics.
pub fn boot_seed_fingerprint() -> [u8; 8] {
    let boot_seed = {
        let mut guard = BOOT_SEED.lock();
        *guard.get_or_insert_with(collect_boot_seed)
    };
    let mut pool = seed::SeedPool::new();
    pool.absorb_bytes(b"fingerprint", &boot_seed);
    let digest = pool.finish();
    let mut out = [0u8; 8];
    out.copy_from_slice(&digest[..8]);
    out
}

/// Fill `dest` from the mixed boot seed — the `AT_RANDOM` fallback.
///
/// Deterministic per (seed, sequence, observables) and never panics. This is the
/// ONLY consumer of the boot seed: `secure_u64`/`fill` keep failing closed, so
/// nothing that asked for cryptographic randomness is ever served these bytes.
pub fn mixed_fill(dest: &mut [u8]) {
    warn_degraded_once();

    // Snapshot the observables BEFORE taking the lock, and derive AFTER releasing
    // it: neither port I/O nor hashing runs inside the critical section.
    let observables = Observables {
        ticks: crate::task::scheduler::ticks(),
        rtc_unix: crate::arch::x86_64::linux::rtc::now_unix(),
        pid: crate::task::scheduler::current_pid(),
    };
    let seq = DERIVE_SEQ.fetch_add(1, Ordering::Relaxed);

    let boot_seed = {
        let mut guard = BOOT_SEED.lock();
        *guard.get_or_insert_with(collect_boot_seed)
    };

    seed::derive_bytes(&boot_seed, seq, observables, dest);
}
