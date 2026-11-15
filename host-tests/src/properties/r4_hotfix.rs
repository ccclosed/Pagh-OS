// Round-4 security hotfix — the two pure halves the kernel cannot exercise on
// the host directly.
//
//   * F-r4-arch-3: `syscall` masks RFLAGS with `IA32_FMASK`. pagh used IF|TF,
//     so AC/DF/NT/IOPL survived into ring 0; AC with CR0.AM=1 turns an
//     unaligned kernel access into #AC at CPL0 -> halt. QEMU (`-cpu max`) does
//     not model #AC, so the audit could only verify the spec. The bit set is
//     therefore pinned here against Linux's `X86_EFLAGS_TF|DF|IF|IOPL|AC|NT`
//     (0x47700) and the kernel's `syscall::init` writes exactly this constant.
//   * F-r4-vfs-1: the installer may follow a pre-existing symlink only when it
//     is the documented provisioning pair (`lib64 -> usr/lib64`). The allowlist
//     is an exact (path, target) match: a path-only rule would accept
//     `lib64 -> etc` and reproduce the cross-package `/etc/autorun` split.

use crate::abi::{
    SYSCALL_SFMASK, RFLAGS_AC, RFLAGS_DF, RFLAGS_IF, RFLAGS_IOPL, RFLAGS_NT, RFLAGS_TF,
};
use crate::install::{is_provisioned_in_root_link, PROVISIONED_INSTALL_LINKS};
use proptest::prelude::*;

/// Linux x86_64 `X86_EFLAGS_TF|DF|IF|IOPL|AC|NT` (arch/x86/include/asm/processor-flags.h).
const LINUX_SFMASK: u64 = 0x0004_7700;

#[test]
fn syscall_sfmask_is_the_linux_set() {
    assert_eq!(SYSCALL_SFMASK, LINUX_SFMASK);
    for (name, bit) in [
        ("TF", RFLAGS_TF),
        ("IF", RFLAGS_IF),
        ("DF", RFLAGS_DF),
        ("IOPL", RFLAGS_IOPL),
        ("NT", RFLAGS_NT),
        ("AC", RFLAGS_AC),
    ] {
        assert_eq!(SYSCALL_SFMASK & bit, bit, "{name} must be masked on entry");
    }
    // The pre-fix mask (IF|TF) must be a strict subset: a regression that drops
    // AC/DF/NT/IOPL has to fail this test loudly.
    assert_eq!(SYSCALL_SFMASK & (RFLAGS_IF | RFLAGS_TF), RFLAGS_IF | RFLAGS_TF);
    assert_ne!(SYSCALL_SFMASK, RFLAGS_IF | RFLAGS_TF);
    // Arithmetic/carry/zero flags are not part of the mask: syscall must not
    // make user arithmetic bits disappear from the saved frame.
    assert_eq!(SYSCALL_SFMASK & 0b1, 0);
    assert_eq!(SYSCALL_SFMASK & (1 << 6), 0);
    assert_eq!(SYSCALL_SFMASK & (1 << 7), 0);
}

#[test]
fn only_the_exact_provisioned_link_is_followed() {
    assert_eq!(PROVISIONED_INSTALL_LINKS, &[("lib64", "usr/lib64")]);
    assert!(is_provisioned_in_root_link("lib64", "usr/lib64"));
    for (path, target) in [
        // Same path, different target: the primitive that would still let a
        // package plant `/lib64 -> etc` and a later one write through it.
        ("lib64", "etc"),
        ("lib64", "usr/lib"),
        ("lib64", "/usr/lib64"),
        ("lib64", "../usr/lib64"),
        ("lib64", "usr/lib64/.."),
        ("lib64", ""),
        // Attack shapes from the round-4 report and its variants.
        ("x/evil", "../etc"),
        ("x/evil", "etc"),
        ("pkgpre/link", "real"),
        ("lib", "usr/lib"),
        ("usr/lib64", "lib64"),
    ] {
        assert!(
            !is_provisioned_in_root_link(path, target),
            "({path}, {target}) must not be followed"
        );
    }
}

proptest! {
    #![proptest_config(ProptestConfig::with_cases(256))]

    /// Only the exact pair is accepted; any mutation of path or target is a
    /// cross-package redirector and must be refused.
    #[test]
    fn provisioned_link_allowlist_is_exact(
        path in ".*",
        target in ".*",
    ) {
        let expected = path == "lib64" && target == "usr/lib64";
        prop_assert_eq!(
            is_provisioned_in_root_link(&path, &target),
            expected,
            "allowlist must match exactly, got ({}, {})",
            path,
            target
        );
    }
}
