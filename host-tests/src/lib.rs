//! Host-side property-test harness for the `pagh` kernel.
//!
//! This crate hosts `proptest`-based property tests that run on the HOST (see the
//! crate-level `Cargo.toml` for why this lives outside the kernel workspace). The
//! kernel itself is `#![no_std]` and built for a bare-metal target, so its property
//! tests live here against pure logic extracted from the bug fixes.
//!
//! Later tasks fill this in:
//!   * Property 1/2 (DMA share/unshare + cross-page round trip)  — task 11.2/11.3
//!   * Property 3   (context-frame interchangeability)            — task 13.2
//!   * Property 4/5 (PMM reserved + alloc/free conservation)      — task 10.2/10.3
//!   * Property 6/7/8 (ext2 capacity sizing / round trip / reject)— task 14.3-14.5
//!   * Property 9   (/dev/serial byte fidelity)                   — task 9.2
//!
//! For now this module only proves the proptest harness is wired up and discovered
//! by `cargo test`.

#[cfg(test)]
mod scaffolding {
    use proptest::prelude::*;

    proptest! {
        /// Smoke test: confirms the `proptest!` harness compiles, links `std`, and
        /// runs on the host. `u8::wrapping_add` is associative-with-zero, a trivially
        /// true property that exercises generator + shrinking machinery end to end.
        #[test]
        fn proptest_harness_is_wired(x in any::<u8>()) {
            prop_assert_eq!(x.wrapping_add(0), x);
        }
    }
}
