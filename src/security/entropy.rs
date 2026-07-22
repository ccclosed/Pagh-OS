//! Hardware-backed entropy for x86_64.
//!
//! The API fails closed: it never substitutes timestamps or a linear PRNG when
//! cryptographic randomness was requested. RDSEED is preferred; RDRAND is the
//! compatibility fallback. Callers must propagate `Unavailable`.

use core::arch::x86_64::{__cpuid, _rdrand64_step, _rdseed64_step};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum EntropyError {
    Unavailable,
}

#[inline]
fn capabilities() -> (bool, bool) {
    // SAFETY: CPUID is an unprivileged architectural query on x86_64.
    let leaf1 = unsafe { __cpuid(1) };
    // SAFETY: CPUID leaf 7 is an unprivileged architectural query.
    let leaf7 = unsafe { __cpuid(7) };
    let rdrand = (leaf1.ecx & (1 << 30)) != 0;
    let rdseed = (leaf7.ebx & (1 << 18)) != 0;
    (rdseed, rdrand)
}

pub fn is_available() -> bool {
    let (rdseed, rdrand) = capabilities();
    rdseed || rdrand
}

#[target_feature(enable = "rdseed")]
unsafe fn rdseed_word() -> Option<u64> {
    let mut value = 0u64;
    for _ in 0..128 {
        // SAFETY: caller verified CPUID.RDSEED before entering this function.
        if unsafe { _rdseed64_step(&mut value) } == 1 {
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
        // SAFETY: caller verified CPUID.RDRAND before entering this function.
        if unsafe { _rdrand64_step(&mut value) } == 1 {
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
