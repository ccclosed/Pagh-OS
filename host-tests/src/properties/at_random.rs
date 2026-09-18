// Feature: `AT_RANDOM` entropy fallback (issue #16), Property 51: the fallback
// block must be a well-mixed ONE-WAY function of a secret boot seed — not a
// predictable function of the observable inputs (tick clock, RTC wall clock, pid).
//
// THE BUG THIS PINS. Without RDSEED/RDRAND, `misc::random_bytes_16()` (the ELF
// `AT_RANDOM` block that glibc turns into the stack-canary / pointer-mangling
// keys) used to emit a 64-bit xorshift over exactly the observable values:
//
//     x = FIXED ^ ticks*K ^ rtc.rotate_left(32) ^ (pid << 48) ^ 0   // 0 = no hw entropy
//     x ^= x << 13; x ^= x >> 7; x ^= x << 17;  second = x * K2
//
// Every term is public (boot time, tick rate, pid) or a constant, the map is
// linear, and the global state was merely *continued* from one block to the next —
// so one observation of a canary was enough to reproduce the whole sequence. The
// old construction is reproduced verbatim below (`old_xorshift`) as a NEGATIVE
// CONTROL: every property in this file that claims to detect "predictable from
// observables" must REJECT it, and the shipped mixer must pass it.
//
// WHAT THE PROPERTIES VARY, AND WHAT WOULD MAKE THEM FAIL
// (the axes the independent verifier asked to see, per property):
//
//   property                          | varied on input            | fails if the mixer…
//   ----------------------------------|----------------------------|------------------------------------------
//   `is_deterministic`                | nothing (same inputs twice)| is not a function (e.g. true RNG/noise)
//   `avalanche_ok`                    | 1 bit of secret/counter/obs| is linear (old xorshift: 1 flipped bit
//                                     |                            | flips ~1 output bit) or constant
//   `secret_separation_ok`            | the SECRET, public inputs  | ignores the secret (old xorshift,
//                                     | fixed                      | constant): all outputs collapse
//   `single_input_variation_ok`       | ONLY ticks, then ONLY pid  | drops an input (`ticks_ignored`,
//                                     |                            | `pid_ignored`, constant)
//   `attack_model_ok`                 | attacker's guesses of the  | is invertible/predictable from the public
//                                     | secret (public inputs all | inputs (old xorshift: guess #1 == real),
//                                     | known exactly)             | or is a constant
//   `no_repeats_ok`                   | the counter (processes)    | repeats blocks for identical observables
//
// Every one of those is an INPUT→OUTPUT relation, not a shape check: none of them
// looks at the length of the block, and "is not all zero" is deliberately NOT a
// property here (it is an accident of the arithmetic, not a guarantee — zero was
// reachable in the old scheme, and reachable in principle in any hash-based one).
// The negative controls below are executable evidence that the properties bite.

use crate::seed::{derive_bytes, Observables, SeedPool};
use proptest::prelude::*;

/// A mixer under test: `(secret boot seed, per-boot counter, observables, out)`.
type Mixer = fn(&[u8; 32], u64, Observables, &mut [u8; 16]);

/// The shipped mixer — exactly what `security::entropy::mixed_fill` calls for the
/// 16-byte `AT_RANDOM` block.
fn shipped(seed: &[u8; 32], counter: u64, obs: Observables, out: &mut [u8; 16]) {
    derive_bytes(seed, counter, obs, &mut out[..]);
}

/// NEGATIVE CONTROL 1 — degenerate mixer: constant output.
fn constant(_seed: &[u8; 32], _counter: u64, _obs: Observables, out: &mut [u8; 16]) {
    out.fill(0xA5);
}

/// NEGATIVE CONTROL 2 — the mixer this change REMOVED (issue #16), verbatim: the
/// first block after boot, when the hardware-entropy term is necessarily empty
/// (`secure_u64().unwrap_or(0)`), and the global state is still the constant.
fn old_xorshift(_seed: &[u8; 32], _counter: u64, obs: Observables, out: &mut [u8; 16]) {
    let mut x = 0x9E37_79B9_7F4A_7C15u64
        ^ obs.ticks.wrapping_mul(0x9E37_79B9_7F4A_7C15)
        ^ obs.rtc_unix.rotate_left(32)
        ^ (obs.pid << 48);
    x ^= x << 13;
    x ^= x >> 7;
    x ^= x << 17;
    let second = x.wrapping_mul(0xBF58_476D_1CE4_E5B9);
    out[..8].copy_from_slice(&x.to_le_bytes());
    out[8..].copy_from_slice(&second.to_le_bytes());
}

/// NEGATIVE CONTROL 3 — the shipped mixer with the tick-clock input dropped.
fn ticks_ignored(seed: &[u8; 32], counter: u64, mut obs: Observables, out: &mut [u8; 16]) {
    obs.ticks = 0;
    shipped(seed, counter, obs, out);
}

/// NEGATIVE CONTROL 4 — the shipped mixer with the pid input dropped.
fn pid_ignored(seed: &[u8; 32], counter: u64, mut obs: Observables, out: &mut [u8; 16]) {
    obs.pid = 0;
    shipped(seed, counter, obs, out);
}

/// Hamming distance between two 128-bit blocks (0..=128).
fn hamming(a: &[u8; 16], b: &[u8; 16]) -> u32 {
    a.iter()
        .zip(b.iter())
        .map(|(x, y)| (x ^ y).count_ones())
        .sum()
}

fn derive(mixer: Mixer, seed: &[u8; 32], counter: u64, obs: Observables) -> [u8; 16] {
    let mut out = [0u8; 16];
    mixer(seed, counter, obs, &mut out);
    out
}

fn obs_strategy() -> impl Strategy<Value = Observables> {
    (any::<u64>(), any::<u64>(), any::<u64>()).prop_map(|(ticks, rtc_unix, pid)| Observables {
        ticks,
        rtc_unix,
        pid,
    })
}

/// Inputs for one block, as `(seed, counter, observables)`.
fn inputs_strategy() -> impl Strategy<Value = ([u8; 32], u64, Observables)> {
    (any::<[u8; 32]>(), any::<u64>(), obs_strategy())
}

// ───────────────────────────── the properties ─────────────────────────────

/// Same inputs twice → same block. (A real RNG would fail this; the fallback is a
/// pure function of its inputs, which is what makes the *inputs* the whole story.)
fn is_deterministic(mixer: Mixer, seed: &[u8; 32], counter: u64, obs: Observables) -> bool {
    derive(mixer, seed, counter, obs) == derive(mixer, seed, counter, obs)
}

/// Avalanche with thresholds: flipping ANY single input bit (secret, counter or
/// any observable) must change roughly half of the 128 output bits.
///
/// Bounds are binomial: distance ~ B(128, 1/2) (mean 64, σ ≈ 5.66). Per-sample the
/// band is a generous [24, 104] (≈ 7σ); the MEAN over the 512 single-bit flips is
/// required in [58, 70] (≈ 24σ of the mean), so a linear mixer — where a flip of a
/// low tick bit moves 1–2 output bits, dragging the mean far below 58 — and a
/// constant mixer (distance 0 everywhere) both fail.
fn avalanche_ok(mixer: Mixer, seed: &[u8; 32], counter: u64, obs: Observables) -> bool {
    let base = derive(mixer, seed, counter, obs);
    let mut distances = alloc::vec::Vec::new();

    for byte in 0..32 {
        for bit in 0..8 {
            let mut flipped = *seed;
            flipped[byte] ^= 1 << bit;
            distances.push(hamming(&base, &derive(mixer, &flipped, counter, obs)));
        }
    }
    for bit in 0..64 {
        distances.push(hamming(
            &base,
            &derive(mixer, seed, counter ^ (1 << bit), obs),
        ));
    }
    for bit in 0..64 {
        let mut o = obs;
        o.ticks ^= 1 << bit;
        distances.push(hamming(&base, &derive(mixer, seed, counter, o)));
        let mut o = obs;
        o.rtc_unix ^= 1 << bit;
        distances.push(hamming(&base, &derive(mixer, seed, counter, o)));
        let mut o = obs;
        o.pid ^= 1 << bit;
        distances.push(hamming(&base, &derive(mixer, seed, counter, o)));
    }

    if distances.iter().any(|&d| !(24..=104).contains(&d)) {
        return false;
    }
    let total: u32 = distances.iter().sum();
    let mean = total / distances.len() as u32;
    (58..=70).contains(&mean)
}

/// With the PUBLIC inputs fixed exactly, different secrets must give different
/// blocks — the property the old scheme cannot have, because it had no secret.
fn secret_separation_ok(mixer: Mixer, counter: u64, obs: Observables) -> bool {
    let mut seen: alloc::vec::Vec<[u8; 16]> = alloc::vec::Vec::new();
    for i in 0..64u64 {
        let mut seed = [0u8; 32];
        // A deterministic family of distinct seeds (no host RNG needed here).
        for (j, byte) in seed.iter_mut().enumerate() {
            *byte = (i as u8).wrapping_mul(31).wrapping_add(j as u8);
        }
        let block = derive(mixer, &seed, counter, obs);
        if seen.contains(&block) {
            return false;
        }
        seen.push(block);
    }
    true
}

/// With everything else fixed, varying ONLY the tick clock — and separately ONLY
/// the pid — must still produce distinct blocks. This is the "one input dropped"
/// detector: a mixer that ignores `ticks` returns the same block 256 times.
fn single_input_variation_ok(mixer: Mixer, seed: &[u8; 32]) -> bool {
    let base = Observables {
        ticks: 1_000,
        rtc_unix: 1_770_000_000,
        pid: 7,
    };

    let mut by_ticks: alloc::vec::Vec<[u8; 16]> = alloc::vec::Vec::new();
    for i in 0..256u64 {
        let mut obs = base;
        obs.ticks = obs.ticks.wrapping_add(i);
        let block = derive(mixer, seed, 0, obs);
        if by_ticks.contains(&block) {
            return false;
        }
        by_ticks.push(block);
    }

    let mut by_pid: alloc::vec::Vec<[u8; 16]> = alloc::vec::Vec::new();
    for i in 0..256u64 {
        let mut obs = base;
        obs.pid = obs.pid.wrapping_add(i);
        let block = derive(mixer, seed, 0, obs);
        if by_pid.contains(&block) {
            return false;
        }
        by_pid.push(block);
    }
    true
}

/// ATTACK MODEL: the adversary knows every public input exactly (ticks, RTC, pid,
/// the per-boot counter) and may try any secret it likes. None of its guesses may
/// reproduce the real block.
///
/// The old scheme fails this on the FIRST guess: its output does not depend on a
/// secret at all, so any guess (or the constant used by the guesser) reproduces
/// the canary byte-for-byte. A constant mixer fails it too, for the opposite
/// reason.
fn attack_model_ok(mixer: Mixer, seed: &[u8; 32], counter: u64, obs: Observables) -> bool {
    let real = derive(mixer, seed, counter, obs);
    let guesses: [[u8; 32]; 4] = [[0u8; 32], [0xFFu8; 32], [0x5Au8; 32], [1u8; 32]];
    guesses
        .iter()
        .all(|guess| derive(mixer, guess, counter, obs) != real)
}

/// Two processes with IDENTICAL observable inputs must not share a block: the
/// per-boot counter is the only difference, and it must be enough.
fn no_repeats_ok(mixer: Mixer, seed: &[u8; 32], obs: Observables) -> bool {
    let mut seen: alloc::vec::Vec<[u8; 16]> = alloc::vec::Vec::new();
    for counter in 0..1024u64 {
        let block = derive(mixer, seed, counter, obs);
        if seen.contains(&block) {
            return false;
        }
        seen.push(block);
    }
    true
}

// ─────────────────────── shipped mixer: all properties ───────────────────────

proptest! {
    #![proptest_config(ProptestConfig::with_cases(48))]

    #[test]
    fn p51_shipped_is_deterministic((seed, counter, obs) in inputs_strategy()) {
        prop_assert!(is_deterministic(shipped, &seed, counter, obs));
    }

    #[test]
    fn p51_shipped_avalanches((seed, counter, obs) in inputs_strategy()) {
        prop_assert!(avalanche_ok(shipped, &seed, counter, obs));
    }

    #[test]
    fn p51_shipped_separates_secrets((counter, obs) in (any::<u64>(), obs_strategy())) {
        prop_assert!(secret_separation_ok(shipped, counter, obs));
    }

    #[test]
    fn p51_shipped_depends_on_every_single_input(seed in any::<[u8; 32]>()) {
        prop_assert!(single_input_variation_ok(shipped, &seed));
    }

    #[test]
    fn p51_shipped_resists_the_public_input_attack((seed, counter, obs) in inputs_strategy()) {
        prop_assert!(attack_model_ok(shipped, &seed, counter, obs));
    }

    #[test]
    fn p51_shipped_never_repeats_a_block((seed, obs) in (any::<[u8; 32]>(), obs_strategy())) {
        prop_assert!(no_repeats_ok(shipped, &seed, obs));
    }
}

// ──────────────── negative controls: the properties must BITE ────────────────

/// The removed xorshift is reproducible from public inputs alone: the properties
/// that exist to detect exactly that must reject it (avalanche is linear, there is
/// no secret, and the attacker's first guess reproduces the block byte-for-byte).
#[test]
fn p51_negative_control_old_xorshift_is_rejected() {
    let seed = [0x11u8; 32];
    let counter = 0;
    let obs = Observables {
        ticks: 9_539_086_926_640_840_705, // the tick value pasha-vfs's model derived
        rtc_unix: 1_770_000_000,
        pid: 3,
    };
    assert!(
        !avalanche_ok(old_xorshift, &seed, counter, obs),
        "the removed xorshift must fail the avalanche criterion"
    );
    assert!(
        !secret_separation_ok(old_xorshift, counter, obs),
        "the removed xorshift has no secret: outputs collapse across seeds"
    );
    assert!(
        !attack_model_ok(old_xorshift, &seed, counter, obs),
        "the removed xorshift is predicted by a public-input model"
    );
    // …and as documentation of the vulnerability: a "model" that knows only the
    // public inputs reproduces the block exactly.
    let mut real = [0u8; 16];
    old_xorshift(&seed, counter, obs, &mut real);
    let mut model = [0u8; 16];
    old_xorshift(&[0u8; 32], counter, obs, &mut model);
    assert_eq!(
        real, model,
        "old fallback is a function of public inputs only"
    );
}

/// A mixer that degenerates to a constant must fail every property that claims to
/// detect "not a function of the observables".
#[test]
fn p51_negative_control_constant_mixer_is_rejected() {
    let seed = [0x22u8; 32];
    let counter = 0;
    let obs = Observables {
        ticks: 42,
        rtc_unix: 1_770_000_000,
        pid: 1,
    };
    assert!(!avalanche_ok(constant, &seed, counter, obs));
    assert!(!secret_separation_ok(constant, counter, obs));
    assert!(!single_input_variation_ok(constant, &seed));
    assert!(!attack_model_ok(constant, &seed, counter, obs));
    assert!(!no_repeats_ok(constant, &seed, obs));
    // Determinism holds for a constant, which is why it is not a security property
    // on its own — the file's header lists it as the "is a function" check only.
    assert!(is_deterministic(constant, &seed, counter, obs));
}

/// Dropping a single input (here: ticks, and separately pid) must be caught by the
/// one-input-dependence property.
#[test]
fn p51_negative_control_dropped_input_is_rejected() {
    let seed = [0x33u8; 32];
    assert!(
        !single_input_variation_ok(ticks_ignored, &seed),
        "a mixer that ignores the tick clock must fail the ticks-only variation"
    );
    assert!(
        !single_input_variation_ok(pid_ignored, &seed),
        "a mixer that ignores the pid must fail the pid-only variation"
    );
}

// ───────────────────────────── seed-pool transcript ─────────────────────────

/// The transcript contract: order and labels are part of the seed, so two boots
/// that absorb the same *values* in a different order/labelling cannot collide,
/// while the same transcript is stable (that is what makes the boot seed usable).
#[test]
fn p51_seed_pool_is_label_and_order_sensitive() {
    let transcript = |order_swapped: bool, relabel: bool| {
        let mut pool = SeedPool::new();
        if order_swapped {
            pool.absorb(b"rtc", 0x2222);
            pool.absorb(if relabel { b"tscx" } else { b"tsc" }, 0x1111);
        } else {
            pool.absorb(if relabel { b"tscx" } else { b"tsc" }, 0x1111);
            pool.absorb(b"rtc", 0x2222);
        }
        pool
    };
    assert_eq!(
        transcript(false, false).finish(),
        transcript(false, false).finish(),
        "same transcript → same seed"
    );
    assert_ne!(
        transcript(false, false).finish(),
        transcript(true, false).finish(),
        "different order → different seed"
    );
    assert_ne!(
        transcript(false, false).finish(),
        transcript(false, true).finish(),
        "different labels → different seed"
    );
    assert_eq!(transcript(false, false).sources(), 2);
}
