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
# Two further TEST-ONLY seeds for the key-lifecycle branches. They are generated and
# validated (host property P53) but deliberately NOT listed in `TEST_TRUST_ANCHORS`:
# wiring them into the live E2E run would widen the surface the verifier must cover,
# while the same branches are already proven against this verifier by P52/P53.
# Generating them here means the tool CAN express "expired" and "not yet valid"
# when a future case needs it, without touching the anchor.
SEED_EXPIRED = hashlib.sha256(b"pagh E2E expired key (issue #32)").digest()
SEED_FUTURE = hashlib.sha256(b"pagh E2E not-yet-valid key (issue #32)").digest()

UID_SIGNING = b"pagh E2E test repository key (ed25519) <pagh-test@example.invalid>"
UID_UNTRUSTED = b"pagh E2E untrusted key (ed25519) <pagh-test@example.invalid>"
UID_EXPIRED = b"pagh E2E expired key (ed25519) <pagh-test@example.invalid>"
UID_FUTURE = b"pagh E2E not-yet-valid key (ed25519) <pagh-test@example.invalid>"

# 2026-01-01T00:00:00Z: after the verifier's clock floor, and old enough that the
# guest's clock (the host's) always sees the signatures as made in the past.
CREATED = 1_767_225_600
# The expired key's window closes one hour after creation (long past for any run);
# the future key is created ten years after `CREATED`.
EXPIRED_AFTER = 3_600
FUTURE_CREATED = CREATED + 10 * 365 * 86_400


def fixture_key(
    seed: bytes, uid: bytes, created: int = CREATED, expiry: int | None = None
) -> dict:
    body = sign.public_key_packet(seed, created)
    return {
        "seed": seed,
        "uid": uid,
        "body": body,
        "block": sign.keyring_block(seed, uid, created, expiry),
        "fingerprint": sign.fingerprint(body),
        "created": created,
        "expires": None if expiry is None else created + expiry,
    }


def render_bytes(data: bytes, indent: str) -> str:
    lines = []
    for i in range(0, len(data), 16):
        lines.append(indent + ", ".join(f"0x{b:02x}" for b in data[i : i + 16]) + ",")
    return "\n".join(lines)


def rust_fpr(fpr: bytes) -> str:
    return "[" + ", ".join(f"0x{b:02x}" for b in fpr) + "]"


def pinned_const(name: str, key: dict, doc: str) -> str:
    """A `PinnedKey` const for one fixture key (same shape as the Debian entries)."""
    expires = "None" if key["expires"] is None else f"Some({key['expires']})"
    return (
        f"\n/// {doc}\n"
        f"pub const {name}: PinnedKey = PinnedKey {{\n"
        f"    label: \"{key['uid'].decode()}\",\n"
        f"    fingerprint: {rust_fpr(key['fingerprint'])},\n"
        "    algo: 22,\n"
        "    bits: 255,\n"
        f"    created: {key['created']},\n"
        f"    expires: {expires},\n"
        "    block: &[\n"
        f"{render_bytes(key['block'], '        ')}\n"
        "    ],\n"
        "    subkeys: &[],\n"
        "};\n"
    )


def render_module(signing: dict, untrusted: dict, expired: dict, future: dict) -> str:
    out = []
    out.append(
        "//! E2E *test* trust anchor for the OpenPGP verifier (issue #32).\n"
        "//!\n"
        "//! GENERATED FILE — do not edit by hand; regenerate with\n"
        "//! `python3 tools/gen_openpgp_testkey.py` (the deterministic seeds live in that\n"
        "//! script, so the anchor and every `tools/mini_repo/` digest can be reproduced).\n"
        "//!\n"
        "//! TEST ANCHOR ONLY. This module is compiled only for the harness builds that run\n"
        "//! `apt update` against the LOCAL test mirror — `lx_selftest` and `lx_bigindex`\n"
        "//! (see `src/pkg/mod.rs`) — and `pkg::apt::trusted_keyring` selects it only there,\n"
        "//! so a normal, release or live-test build cannot trust the key below. It exists so\n"
        "//! the local-mirror harness can run against a repository that is *signed*: the\n"
        "//! positive half of the E2E proof, plus the negative half (tampered Packages,\n"
        "//! tampered .deb, unsigned mirror, unpinned signer) driven by `tools/mini_repo.py`.\n"
        "\n"
        "#![allow(dead_code)]\n"
        "\n"
        "use super::openpgp::PinnedKey;\n"
        "use super::openpgp_keys::{DEBIAN_KEY_0, DEBIAN_KEY_1, DEBIAN_KEY_2};\n"
    )
    out.append(pinned_const(
        "TEST_KEY_PRIMARY",
        signing,
        "The E2E repository signing key: a deterministic Ed25519 key, test-only.",
    ))
    out.append(
        "\n/// The trust anchor table the harness builds verify against: every committed\n"
        "/// Debian archive key plus the test key above. Selected under\n"
        "/// `#[cfg(any(feature = \"lx_selftest\", feature = \"lx_bigindex\", feature = \"lx_bigindex_inram\"))]`;\n"
        "/// every other build trusts `DEBIAN_KEYRING` only, so the test key does not exist\n"
        "/// there at all. Nothing can REMOVE an anchor from this table — only add.\n"
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
    out.append(pinned_const(
        "TEST_KEY_EXPIRED",
        expired,
        "TEST-ONLY lifecycle key whose validity window closed one hour after creation.\n"
        "/// Deliberately NOT in `TEST_TRUST_ANCHORS`: it exists so the tool can express an\n"
        "/// expired pinned key (host property P53 proves the verifier refuses it), and\n"
        "/// wiring it into the E2E run is a separate, deliberate act.",
    ))
    out.append(pinned_const(
        "TEST_KEY_NOT_YET_VALID",
        future,
        "TEST-ONLY lifecycle key created ten years in the future (`created > now`): same\n"
        "/// rationale as `TEST_KEY_EXPIRED` — generated and validated by P53, not anchored,\n"
        "/// not part of the E2E run.",
    ))
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
    expired = fixture_key(SEED_EXPIRED, UID_EXPIRED, CREATED, EXPIRED_AFTER)
    future = fixture_key(SEED_FUTURE, UID_FUTURE, FUTURE_CREATED)

    rendered = render_module(signing, untrusted, expired, future)
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
    print(f"  expired   {expired['fingerprint'].hex()}  (expires {expired['expires']})")
    print(f"  future    {future['fingerprint'].hex()}  (created {future['created']})")

    if args.print_keyring:
        os.makedirs(args.print_keyring, exist_ok=True)
        for name, key in (
            ("test-signing", signing),
            ("test-untrusted", untrusted),
            ("test-expired", expired),
            ("test-not-yet-valid", future),
        ):
            path = os.path.join(args.print_keyring, f"{name}.gpg")
            with open(path, "wb") as fh:
                fh.write(key["block"])
            print(f"  wrote {path}")


if __name__ == "__main__":
    main()
