#!/usr/bin/env python3
"""Generate `src/pkg/openpgp_test_keys.rs` — the E2E *test* trust anchor.

Issue #32, contract §4.6. The local-mirror E2E harness runs a kernel built with
the `lx_selftest` feature, and that kernel must accept a repository signed by a
key we control — otherwise the harness can only ever prove refusals. This script
emits that key as a `PinnedKey` table entry, together with the three pinned
Debian keys (so the selftest build still trusts real Debian metadata).

TEST ANCHOR ONLY. The generated module is compiled **only** under
`#[cfg(feature = "lx_selftest")]` (see `src/pkg/mod.rs`) and `pkg::apt` selects
it only in that configuration, so a production/default build cannot trust the
test key even by accident. The key itself is derived from a fixed seed written
below — there is no committed secret: the private key exists only as this seed,
inside the test tooling, and the kernel only ever verifies.

The seed is deterministic on purpose: an E2E run must produce byte-identical
fixtures, so the repository, its signatures and the anchor can be regenerated and
compared. The companion signer (`tools/openpgp_sign.py`) implements RFC 8032 and
is checked against the RFC test vectors, and its output is cross-checked first
with `gpgv` and finally by the kernel's own verifier.

Usage:
    python3 tools/gen_openpgp_testkey.py               # write the module
    python3 tools/gen_openpgp_testkey.py --print-keyring DIR
                                                       # dump binary blocks for review
"""

import argparse
import hashlib
import os
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import openpgp_sign as sign  # noqa: E402

OUT_PATH = "src/pkg/openpgp_test_keys.rs"

# Deterministic TEST-ONLY seeds. Changing either one invalidates every committed
# digest in tools/mini_repo/ (regenerate both).
SEED_SIGNING = hashlib.sha256(b"pagh E2E repository signing key (issue #32)").digest()
SEED_UNTRUSTED = hashlib.sha256(b"pagh E2E untrusted key (issue #32)").digest()

UID_SIGNING = b"pagh E2E test repository key (ed25519) <pagh-test@example.invalid>"
UID_UNTRUSTED = b"pagh E2E untrusted key (ed25519) <pagh-test@example.invalid>"

# 2026-01-01T00:00:00Z: after the verifier's clock floor, and old enough that the
# guest's clock (the host's) always sees the signatures as made in the past.
CREATED = 1_767_225_600


def fixture_key(seed: bytes, uid: bytes) -> dict:
    body = sign.public_key_packet(seed, CREATED)
    return {
        "seed": seed,
        "uid": uid,
        "body": body,
        "block": sign.keyring_block(seed, uid, CREATED),
        "fingerprint": sign.fingerprint(body),
    }


def render_bytes(data: bytes, indent: str) -> str:
    lines = []
    for i in range(0, len(data), 16):
        lines.append(indent + ", ".join(f"0x{b:02x}" for b in data[i : i + 16]) + ",")
    return "\n".join(lines)


def rust_fpr(fpr: bytes) -> str:
    return "[" + ", ".join(f"0x{b:02x}" for b in fpr) + "]"


def render_module(signing: dict, untrusted: dict) -> str:
    out = []
    out.append(
        "//! E2E *test* trust anchor for the OpenPGP verifier (issue #32).\n"
        "//!\n"
        "//! GENERATED FILE — do not edit by hand; regenerate with\n"
        "//! `python3 tools/gen_openpgp_testkey.py` (the deterministic seed lives in that\n"
        "//! script, so the anchor and every `tools/mini_repo/` digest can be reproduced).\n"
        "//!\n"
        "//! TEST ANCHOR ONLY. This module is compiled only under the `lx_selftest` feature\n"
        "//! and `pkg::apt` uses it only in that configuration, so a normal or release build\n"
        "//! cannot trust the key below. It exists so the local-mirror harness can run against\n"
        "//! a repository that is *signed* — the positive half of the E2E proof; the negative\n"
        "//! half (tampered Packages, tampered .deb, unsigned mirror, untrusted signer) is\n"
        "//! driven by `tools/mini_repo.py` against this same anchor.\n"
        "\n"
        "#![allow(dead_code)]\n"
        "\n"
        "use super::openpgp::PinnedKey;\n"
        "use super::openpgp_keys::{DEBIAN_KEY_0, DEBIAN_KEY_1, DEBIAN_KEY_2};\n"
        "\n"
        "/// The E2E repository signing key: a deterministic Ed25519 key, test-only.\n"
        "pub const TEST_KEY_PRIMARY: PinnedKey = PinnedKey {\n"
        f"    label: \"{UID_SIGNING.decode()}\",\n"
        f"    fingerprint: {rust_fpr(signing['fingerprint'])},\n"
        "    algo: 22,\n"
        "    bits: 255,\n"
        f"    created: {CREATED},\n"
        "    expires: None,\n"
        "    block: &[\n"
        f"{render_bytes(signing['block'], '        ')}\n"
        "    ],\n"
        "    subkeys: &[],\n"
        "};\n"
        "\n"
        "/// The keyring the `lx_selftest` build verifies against: the committed Debian\n"
        "/// archive keys plus the test key above.\n"
        "pub static TEST_TRUST_ANCHORS: &[PinnedKey] = &[\n"
        "    DEBIAN_KEY_0,\n"
        "    DEBIAN_KEY_1,\n"
        "    DEBIAN_KEY_2,\n"
        "    TEST_KEY_PRIMARY,\n"
        "];\n"
        "\n"
        "/// The *untrusted* E2E key (a second deterministic seed, never added to any trust\n"
        "/// anchor): `tools/mini_repo.py` signs one suite with it so the harness can prove a\n"
        "/// valid signature by an unpinned key is still refused.\n"
        "pub const TEST_KEY_UNTRUSTED_FINGERPRINT: [u8; 20] = "
        f"{rust_fpr(untrusted['fingerprint'])};\n"
    )
    return "".join(out)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--print-keyring",
        metavar="DIR",
        help="also write the two binary keyring blocks there, for `gpg --list-packets`",
    )
    args = ap.parse_args()

    sign.self_test()  # the signer must match RFC 8032 before anything is generated
    signing = fixture_key(SEED_SIGNING, UID_SIGNING)
    untrusted = fixture_key(SEED_UNTRUSTED, UID_UNTRUSTED)

    rendered = render_module(signing, untrusted)
    with tempfile.TemporaryDirectory() as tmp:
        raw = os.path.join(tmp, "openpgp_test_keys.rs")
        with open(raw, "w", newline="\n", encoding="utf-8") as fh:
            fh.write(rendered)
        subprocess.run(["rustfmt", "--edition", "2021", raw], check=True)
        with open(raw, encoding="utf-8") as fh:
            rendered = fh.read()

    with open(OUT_PATH, "w", newline="\n", encoding="utf-8") as fh:
        fh.write(rendered)
    print(f"wrote {OUT_PATH} ({len(rendered)} bytes)")
    print(f"  signing   {signing['fingerprint'].hex()}  ({len(signing['block'])} B block)")
    print(f"  untrusted {untrusted['fingerprint'].hex()}  ({len(untrusted['block'])} B block)")

    if args.print_keyring:
        os.makedirs(args.print_keyring, exist_ok=True)
        for name, key in (("test-signing", signing), ("test-untrusted", untrusted)):
            path = os.path.join(args.print_keyring, f"{name}.gpg")
            with open(path, "wb") as fh:
                fh.write(key["block"])
            print(f"  wrote {path}")


if __name__ == "__main__":
    main()
