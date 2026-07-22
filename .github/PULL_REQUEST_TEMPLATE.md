<!--
Thanks for contributing to pagh! Please fill out the sections below.
See CONTRIBUTING.en.md (or CONTRIBUTING.md for Russian) for the full guidelines.
-->

## Summary

<!-- What does this PR change, and why? -->

## Related issue

<!-- e.g. Closes #123 -->

## How it was tested

<!-- Pick what applies and add details -->
- [ ] `host-tests` (`cd host-tests && cargo test`)
- [ ] In-QEMU `selftest` (P1–P27, no `FAIL`)
- [ ] Manual QEMU run

<!-- For behavior changes, paste before/after: serial output, framebuffer screenshot, etc. -->

## Checklist

- [ ] `run.cmd build` (debug) passes with no errors and no warnings
- [ ] `cargo fmt --all` and `cargo clippy` are clean
- [ ] `cd host-tests && cargo test` is green (if portable logic was touched)
- [ ] `selftest` in QEMU passes with no `FAIL` (if kernel code was touched)
- [ ] Build invariants are preserved (`#![no_std]`, `panic = abort`, custom target,
      `.requests` section, higher-half address, forced frame pointers)
- [ ] New `unsafe` is annotated with `// SAFETY:`
- [ ] Docs (`README.md`, comments) updated if behavior changed
- [ ] No stray/generated files in the diff (`target/`, `iso_root/`, `PAGH.elf`,
      `disk.img`, `OVMF.fd`, `limine-12.3.1/`, logs, IDE folders)
