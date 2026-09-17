#!/usr/bin/env python3
"""Adversarial OpenPGP repository fixtures for issue #32 (verification only).

Builds a set of self-contained `apt` repository trees, one per attack, plus the
positive controls that prove the negative set is not a tautology ("refuse
everything" must fail the control cases).

THE TRUST ROOT DECIDES WHAT IS TESTABLE. `src/pkg/apt.rs::trusted_keyring()` is
`&DEBIAN_KEYRING` — three pinned Debian archive keys, no test anchor. A locally
generated key therefore only ever reaches the *signature* stage
(`cause=NoTrustedSignature`), so the two cases issue #32 actually demands —
"a tampered `Packages` is refused" and "a tampered `.deb` is refused" — CANNOT be
built from a self-signed repository: the signature must be ACCEPTED first, and the
only keys the kernel accepts are Debian's. Those cases are therefore built from
**real, hash-pinned Debian metadata** (`tools/openpgp_fixtures/real/`, checked in)
whose *unsigned* parts (the index or the `.deb`) are tampered with.

Two families come out of that:

  * `a*` — real Debian trust root: the accept path (positive control, both the
    `InRelease` and the `Release.gpg` route), the index/payload binding attacks,
    the "present-but-invalid InRelease must not fall back" case, the unsigned
    mirror, and the rollback replay (expected to be ACCEPTED: a documented
    residual, not a bug — `stable` carries no `Valid-Until`).
  * `b*` — locally signed with committed fixture keys: the refusals that live at
    the signature/armor/clearsign stages. Their signatures are *valid* GnuPG
    signatures by keys outside the pinned set, which is exactly the "valid
    signature, wrong signer" attack.

Inputs are committed and hash-pinned (`tools/openpgp_fixtures/…/inventory.json`),
so the build is deterministic and needs no network. Output trees are written under
`.cache/openpgp_cases/` (git-ignored) together with `manifest.json`.

Usage:
    python3 tools/openpgp_attack_fixtures.py build [--out DIR] [--only ID …]
    python3 tools/openpgp_attack_fixtures.py check [--out DIR]
    python3 tools/openpgp_attack_fixtures.py list
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import re
import shutil
import subprocess
import sys
import tempfile

HERE = os.path.dirname(os.path.abspath(__file__))
ROOT = os.path.dirname(HERE)
INPUTS = os.path.join(HERE, "openpgp_fixtures")
REAL = os.path.join(INPUTS, "real")
REAL_OLD = os.path.join(INPUTS, "real_old")
KEYS = os.path.join(INPUTS, "keys")
DEFAULT_OUT = os.path.join(ROOT, ".cache", "openpgp_cases")

SUITE = "stable"
COMPONENT = "contrib"
ARCH = "binary-amd64"
INDEX_NAME = "Packages.xz"

# ─── OpenPGP armor helpers (CRC24 + dearmor/rearmor) ────────────────────────


def crc24(data: bytes) -> int:
    """The OpenPGP armor CRC (RFC 4880 §6.1): poly 0x1864CFB, init 0xB704CE."""
    crc = 0xB704CE
    for byte in data:
        crc ^= byte << 16
        for _ in range(8):
            crc <<= 1
            if crc & 0x1000000:
                crc ^= 0x1864CFB
    return crc & 0xFFFFFF


def dearmor(text: str) -> tuple[str, bytes]:
    """`(kind, payload)` of the armor block.

    A clearsigned `InRelease` wraps the signature in a *nested* armor block
    (`BEGIN PGP SIGNED MESSAGE` … `BEGIN PGP SIGNATURE`), so the LAST BEGIN line
    is the block that carries the signature packets — taking the first one would
    slice the clear text into the base64.
    """
    lines = text.splitlines()
    begins = [i for i, l in enumerate(lines) if l.startswith("-----BEGIN PGP ")]
    if not begins:
        raise ValueError("no armor BEGIN line")
    start = begins[-1]
    begin = lines[start]
    kind = begin[len("-----BEGIN PGP ") : -len("-----")].strip()
    body: list[str] = []
    crc_line = None
    for line in lines[start + 1 :]:
        if line.startswith("-----END PGP"):
            break
        if line.startswith("=") and len(line) == 5:
            crc_line = line
            continue
        body.append(line.strip())
    payload = base64.b64decode("".join(body))
    if crc_line is not None:
        want = int.from_bytes(base64.b64decode(crc_line[1:]), "big")
        got = crc24(payload)
        if want != got:
            raise ValueError(f"armor CRC mismatch while reading fixture: {want:#x} != {got:#x}")
    return kind, payload


def rewrite_clearsign(text: str, payload: bytes) -> bytes:
    """Keep a clearsigned document's framing and replace its signature payload."""
    lines = text.splitlines()
    idx = max(i for i, l in enumerate(lines) if l.startswith("-----BEGIN PGP SIGNATURE-----"))
    head = "\n".join(lines[:idx]) + "\n"
    return (head + armor("SIGNATURE", payload).decode()).encode()


def armor(kind: str, payload: bytes) -> bytes:
    """Wrap `payload` in a fresh armored block with a correct CRC24."""
    b64 = base64.b64encode(payload).decode()
    body = "\n".join(b64[i : i + 64] for i in range(0, len(b64), 64))
    crc = base64.b64encode(crc24(payload).to_bytes(3, "big")).decode()
    return f"-----BEGIN PGP {kind}-----\n\n{body}\n={crc}\n-----END PGP {kind}-----\n".encode()


# ─── keys / gpg ─────────────────────────────────────────────────────────────


def keys_meta() -> dict:
    with open(os.path.join(KEYS, "keys.json"), encoding="utf-8") as fh:
        return json.load(fh)


def gpg(home: str, *args: str, faked: str | None = None, check: bool = True,
        binary: bool = False):
    cmd = ["gpg", "--homedir", home, "--batch", "--yes", "--pinentry-mode", "loopback",
           "--passphrase", ""]
    if faked:
        cmd += ["--faked-system-time", faked]
    cmd += list(args)
    # `--export` writes key packets, not text: never decode those as UTF-8.
    res = subprocess.run(cmd, capture_output=True, text=not binary)
    if check and res.returncode != 0:
        raise SystemExit(f"gpg {' '.join(args)} failed:\n{res.stderr}")
    return res


def import_keys(home: str) -> None:
    """Import the committed fixture keys.

    A faked time past every key's creation is required: GnuPG refuses to import a
    key whose creation timestamp is in ITS future (the `future` fixture is dated
    2030 on purpose), so the import runs at 2031-01-01 and the per-case signing
    steps set their own `--faked-system-time`.
    """
    os.makedirs(home, mode=0o700, exist_ok=True)
    for entry in keys_meta()["keys"]:
        gpg(home, "--import", os.path.join(KEYS, entry["secret"]),
            faked="20310101T000000!")
    # Combined public keyring for `check` (gpgv cannot fake its clock, so the
    # expired-key fixture is verified as-is and only *notes* the expiry).
    pub = gpg(home, "--export", faked="20310101T000000!", binary=True)
    with open(os.path.join(KEYS, "fixture_keys.pub.gpg"), "wb") as fh:
        fh.write(pub.stdout)


def sign_detached(home: str, key: str, data_path: str, out_path: str, faked: str) -> None:
    gpg(home, "--local-user", key, "--armor", "--detach-sign",
        "--output", out_path, data_path, faked=faked)


def sign_clearsign(home: str, key: str, data_path: str, out_path: str, faked: str) -> None:
    gpg(home, "--local-user", key, "--armor", "--clearsign",
        "--output", out_path, data_path, faked=faked)


# ─── tree construction ──────────────────────────────────────────────────────


def write(path: str, data: bytes) -> None:
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "wb") as fh:
        fh.write(data)


def read(path: str) -> bytes:
    with open(path, "rb") as fh:
        return fh.read()


def real_tree(dest: str, *, source: str = REAL, inrelease: bool = True,
              release_gpg: bool = True, suite_dir: str = SUITE) -> dict:
    """Lay out a complete repository tree from a pinned Debian inventory."""
    inv = json.load(open(os.path.join(source, "inventory.json"), encoding="utf-8"))
    d = os.path.join(dest, "dists", suite_dir)
    idx = os.path.join(d, COMPONENT, ARCH, INDEX_NAME)
    write(idx, read(os.path.join(source, "contrib_binary-amd64_Packages.xz")))
    write(os.path.join(d, "Release"), read(os.path.join(source, "Release")))
    if inrelease:
        write(os.path.join(d, "InRelease"), read(os.path.join(source, "InRelease")))
    if release_gpg:
        write(os.path.join(d, "Release.gpg"), read(os.path.join(source, "Release.gpg")))
    deb = os.path.join(dest, inv["deb"]["filename"])
    write(deb, read(os.path.join(source, inv["deb"]["local"])))
    return {"inventory": inv, "index": idx, "deb": deb,
            "release": os.path.join(d, "Release"),
            "inrelease": os.path.join(d, "InRelease"),
            "release_gpg": os.path.join(d, "Release.gpg")}


def set_suite_field(path: str, suite: str) -> None:
    """Point the `Suite:` field of a Release we sign ourselves at `suite`.

    `release.matches_suite()` is true only when `Suite:` or `Codename:` equals the
    configured suite (or both are empty), so a tree served under
    `dists/<case_id>/` MUST carry that name in the signed bytes. Only ever called
    for trees whose Release we sign; a Debian-signed Release keeps `Suite: stable`
    forever.
    """
    text = read(path).decode()
    out = []
    seen = False
    for line in text.splitlines():
        if line.startswith("Suite:"):
            out.append(f"Suite: {suite}")
            seen = True
        else:
            out.append(line)
    if not seen:
        raise SystemExit(f"no Suite: field in {path}")
    write(path, ("\n".join(out) + "\n").encode())


def flip_byte(path: str, *, at: str = "middle") -> None:
    """Flip one bit in the middle (or near the end) of a file, keeping its size."""
    data = bytearray(read(path))
    pos = len(data) // 2 if at == "middle" else len(data) - 2
    data[pos] ^= 0x01
    write(path, bytes(data))


def cases() -> list[dict]:
    """The case table: id → what is served, what must happen, which marker."""
    return [
        dict(
            id="a01-reference-inrelease",
            family="real-trust-root",
            expect="accept",
            stage=None,
            cause=None,
            marker="apt: verify OK release=InRelease",
            attacks="positive control: real `stable` InRelease + real contrib index + real "
                    "`.deb`, nothing modified — the tree a correct mirror serves",
            notes="Without this case `refuse everything` would also pass the negative set. "
                  "Both the index and the package digest must verify.",
        ),
        dict(
            id="a02-reference-detached",
            family="real-trust-root",
            expect="accept",
            stage=None,
            cause=None,
            marker="apt: verify OK release=Release.gpg",
            attacks="positive control for the detached route: `InRelease` removed, real "
                    "`Release.gpg` + `Release` must verify instead",
            notes="Only the *absence* of InRelease may trigger the fallback (HTTP 404).",
        ),
        dict(
            id="a03-index-hash-mismatch",
            family="real-trust-root",
            expect="reject",
            stage="index",
            cause="HashMismatch",
            marker="apt: verify FAIL stage=index cause=HashMismatch",
            attacks="payload swap under a valid signature: real signed Release, one byte "
                    "flipped inside `Packages.xz` (same size, so only SHA-256 catches it)",
            notes="The minimum demanded by issue #32. The index must NOT be published: no "
                  "`apt: index ready` line.",
        ),
        dict(
            id="a03b-index-size-mismatch",
            family="real-trust-root",
            expect="reject",
            stage="index",
            cause="SizeMismatch",
            marker="apt: verify FAIL stage=index cause=SizeMismatch",
            attacks="truncated index (one byte short): the size check runs before SHA-256",
            notes="Pins the check order of `verify_index_body` (size, then hash).",
        ),
        dict(
            id="a04-deb-hash-mismatch",
            family="real-trust-root",
            expect="reject",
            stage="deb",
            cause="HashMismatch",
            marker="apt: verify FAIL stage=deb cause=HashMismatch",
            attacks="payload swap under a valid signature: real signed index, one byte "
                    "flipped inside the `.deb` (same size)",
            notes="Second half of the issue's minimum. The digest is checked BEFORE "
                  "`deb::parse_ar`/unpack: the harness must also assert that "
                  "`/mnt/usr/...` from the package does not appear.",
        ),
        dict(
            id="a05-deb-size-mismatch",
            family="real-trust-root",
            expect="reject",
            stage="deb",
            cause="SizeMismatch",
            marker="apt: verify FAIL stage=deb cause=SizeMismatch",
            attacks="`.deb` with one extra byte appended: size is checked before SHA-256",
            notes="Isolates the size check of `verify_package_body`.",
        ),
        dict(
            id="a06-signature-bad-no-fallback",
            family="real-trust-root",
            expect="reject",
            stage="signature",
            cause="BadSignature",
            marker="apt: verify FAIL stage=signature cause=BadSignature",
            attacks="a real InRelease whose signature is corrupted (armor CRC recomputed, so "
                    "the failure is the signature, not the armor) while a VALID `Release.gpg` "
                    "is served next to it",
            notes="Proof that a present-but-invalid InRelease is fatal and does not fall back "
                  "to the detached route. A tree with a valid Release.gpg and an invalid "
                  "InRelease must still be refused.",
        ),
        dict(
            id="a07-unsigned",
            family="real-trust-root",
            expect="reject",
            stage="metadata",
            cause="Unsigned",
            marker="apt: verify FAIL stage=metadata cause=Unsigned",
            attacks="a mirror with metadata but no signature at all (`InRelease` and "
                    "`Release.gpg` both absent)",
            notes="The user-visible refusal of §6.4: no 'continue at your own risk' path.",
        ),
        dict(
            id="a08-rollback-old-stable",
            family="real-trust-root",
            expect="accept",
            stage=None,
            cause=None,
            marker="apt: verify OK release=InRelease",
            attacks="replay of a COMPLETE older `stable` triplet (real signatures from "
                    "snapshot.debian.org, internally consistent index and `.deb`)",
            notes="EXPECTED RESULT IS 'ACCEPTED': `stable` carries no `Valid-Until` and the "
                  "kernel persists no 'highest Release seen', so a valid old triplet is "
                  "replayed (contract §0/§6.4 residual). Serve this AFTER a successful "
                  "update from a01; the case exists to make the residual visible in an "
                  "actual run instead of only in prose.",
        ),
        dict(
            id="b01-untrusted-signer",
            family="local-keys",
            expect="reject",
            stage="signature",
            cause="NoTrustedSignature",
            marker="apt: verify FAIL stage=signature cause=NoTrustedSignature",
            attacks="a FOREIGN key: valid GnuPG signature over an otherwise well-formed "
                    "`Release` by a key outside the pinned set (the classic 'mirror signed by "
                    "somebody else' attack)",
            notes="Covers both of the captain's phrasings — 'чужой ключ' and 'valid signature "
                  "of a key outside the pinned set'. The local key CANNOT reach the "
                  "index/deb stages, which is why those cases use real Debian metadata.",
        ),
        dict(
            id="b02-no-signature-packets",
            family="local-keys",
            expect="reject",
            stage="signature",
            cause="NoSignature",
            marker="apt: verify FAIL stage=signature cause=NoSignature",
            attacks="`Release.gpg` is a well-formed armor block that carries a marker packet "
                    "and no signature packet at all (CRC valid)",
            notes="Distinct from `metadata/Unsigned`: the signature *file* exists, it simply "
                  "has no signature in it.",
        ),
        dict(
            id="b03-armor-crc-mismatch",
            family="local-keys",
            expect="reject",
            stage="armor",
            cause="CrcMismatch",
            marker="apt: verify FAIL stage=armor cause=CrcMismatch",
            attacks="valid armor with a corrupted body and a stale CRC24",
            notes="Armor integrity is checked before any packet parsing.",
        ),
        dict(
            id="b04-clearsign-malformed",
            family="local-keys",
            expect="reject",
            stage="clearsign",
            cause="Malformed",
            marker="apt: verify FAIL stage=clearsign cause=Malformed",
            attacks="`InRelease` without its `-----BEGIN PGP SIGNATURE-----` line: the "
                    "clearsign framing is incomplete while the rest of the document is intact",
            notes="Two different layers are involved and both are worth a case: this one "
                  "fails the clearsign framing check, `b04b` fails the armor dearmor "
                  "(measured against the verifier, contract §16).",
        ),
        dict(
            id="b04b-armor-malformed-end-missing",
            family="local-keys",
            expect="reject",
            stage="armor",
            cause="MalformedArmor",
            marker="apt: verify FAIL stage=armor cause=MalformedArmor",
            attacks="`InRelease` without its `-----END PGP SIGNATURE-----` line: the armor "
                    "block is dearmored before any signature packet is read",
            notes="Kept next to `b04` on purpose: the same document with a different line "
                  "removed fails a different check, which is what makes the two layers "
                  "visible (the earlier revision of this file expected clearsign/Malformed "
                  "here — the verifier proved otherwise).",
        ),
        dict(
            id="b05-expired-untrusted-signer",
            family="local-keys",
            expect="reject",
            stage="signature",
            cause="NoTrustedSignature",
            marker="apt: verify FAIL stage=signature cause=NoTrustedSignature",
            attacks="signature made by an EXPIRED key (signed while it was valid, verified "
                    "after its expiry)",
            notes="Documents an ordering fact rather than a new attack: key validity is "
                  "evaluated only for pinned keys, so an expired *foreign* key is refused at "
                  "the trust check first. `key/Expired` for a PINNED key is not constructible "
                  "end-to-end (see `not_constructible` in the manifest).",
        ),
    ]


# ─── per-case builders ──────────────────────────────────────────────────────


def build_case(case: dict, out_root: str, home: str, *, suite_mode: str = "stable") -> dict:
    cid = case["id"]
    # `a*` trees carry Debian's own signature, whose `Suite: stable` we cannot
    # recreate: they are served as `stable` whatever the caller asked for.
    per_case = suite_mode == "case" and case["family"] == "local-keys"
    suite_dir = cid if per_case else SUITE
    if suite_mode == "case" and case["family"] != "local-keys":
        pass  # documented in the manifest; not an error, `stable` is the only option
    tree = os.path.join(out_root, cid)
    if os.path.exists(tree):
        shutil.rmtree(tree)
    os.makedirs(tree)
    meta = keys_meta()
    release_fpr = meta["keys"][0]["fingerprint"]
    expired_fpr = meta["keys"][1]["fingerprint"]
    # 2025-01-02T01:00:00Z: inside the release key's validity; a fixed instant, so
    # every generated signature is reproducible.
    SIGN_TIME = "20250102T010000!"
    EXPIRED_SIGN_TIME = "20240101T010000!"  # inside the expired key's 1-day window

    if cid == "a01-reference-inrelease":
        real_tree(tree)
    elif cid == "a02-reference-detached":
        real_tree(tree, inrelease=False)
    elif cid == "a03-index-hash-mismatch":
        t = real_tree(tree)
        flip_byte(t["index"])
    elif cid == "a03b-index-size-mismatch":
        t = real_tree(tree)
        write(t["index"], read(t["index"])[:-1])
    elif cid == "a04-deb-hash-mismatch":
        t = real_tree(tree)
        flip_byte(t["deb"])
    elif cid == "a05-deb-size-mismatch":
        t = real_tree(tree)
        write(t["deb"], read(t["deb"]) + b"\x00")
    elif cid == "a06-signature-bad-no-fallback":
        t = real_tree(tree)
        _, payload = dearmor(read(t["inrelease"]).decode())
        tampered = bytearray(payload)
        tampered[-1] ^= 0x01  # inside the last signature packet's MPI data
        # Re-armor with a CORRECT CRC so the failure is the signature itself, and
        # keep the clearsign framing (a bare armor block would not be an InRelease).
        write(t["inrelease"], rewrite_clearsign(read(t["inrelease"]).decode(), bytes(tampered)))
    elif cid == "a07-unsigned":
        real_tree(tree, inrelease=False, release_gpg=False)
    elif cid == "a08-rollback-old-stable":
        real_tree(tree, source=REAL_OLD)
    elif cid in ("b01-untrusted-signer", "b05-expired-untrusted-signer"):
        t = real_tree(tree, suite_dir=suite_dir)
        set_suite_field(t["release"], suite_dir)
        # keep the real Release TEXT (a realistic document), replace both signatures
        fpr, when = (release_fpr, SIGN_TIME) if cid == "b01-untrusted-signer" else (
            expired_fpr, EXPIRED_SIGN_TIME)
        sign_clearsign(home, fpr, t["release"], t["inrelease"], when)
        sign_detached(home, fpr, t["release"], t["release_gpg"], when)
    elif cid == "b02-no-signature-packets":
        t = real_tree(tree, inrelease=False, suite_dir=suite_dir)
        set_suite_field(t["release"], suite_dir)
        # A marker packet (tag 10, body "PGP") and nothing else: well-formed armor,
        # no signature packet.
        write(t["release_gpg"], armor("SIGNATURE", bytes([0xCA, 0x03]) + b"PGP"))
    elif cid == "b03-armor-crc-mismatch":
        t = real_tree(tree, inrelease=False, suite_dir=suite_dir)
        set_suite_field(t["release"], suite_dir)
        sign_detached(home, release_fpr, t["release"], t["release_gpg"], SIGN_TIME)
        text = read(t["release_gpg"]).decode()
        lines = text.splitlines()
        for i, line in enumerate(lines):
            if line.startswith("="):  # corrupt the CRC line only
                lines[i] = "=" + base64.b64encode(b"\x00\x00\x00").decode()
        write(t["release_gpg"], ("\n".join(lines) + "\n").encode())
    elif cid in ("b04-clearsign-malformed", "b04b-armor-malformed-end-missing"):
        t = real_tree(tree, suite_dir=suite_dir)
        set_suite_field(t["release"], suite_dir)
        sign_clearsign(home, release_fpr, t["release"], t["inrelease"], SIGN_TIME)
        drop = "-----BEGIN PGP SIGNATURE-----" if cid == "b04-clearsign-malformed" \
            else "-----END PGP SIGNATURE-----"
        kept = [l for l in read(t["inrelease"]).decode().splitlines() if l != drop]
        write(t["inrelease"], ("\n".join(kept) + "\n").encode())
    else:
        raise SystemExit(f"unknown case {cid}")
    return {"tree": os.path.relpath(tree, ROOT), "configured_suite": suite_dir}


NOT_CONSTRUCTIBLE = [
    dict(cause="key/Expired", why="requires a PINNED key that is expired and a signature "
         "made while it was valid — we hold no Debian private key",
         covered_by="P53 (pinned keyring: pins, real-GnuPG interop, expiry) + code review of "
                    "the expiry branch"),
    dict(cause="key/Revoked", why="no pinned key carries a revocation today (contract §0, "
         "verified), and revocation inside the keyring is a host-side property",
         covered_by="P53 (revocation is honoured only inside the committed keyring block)"),
    dict(cause="key/NotYetValid", why="same as Expired: needs a pinned key with a future "
         "creation time",
         covered_by="P53"),
    dict(cause="signature/FutureSignature", why="a signature time > now+86400 by a PINNED "
         "key cannot be produced locally",
         covered_by="P52 (`signature_time_is_sanity_checked_against_the_clock_and_the_key`, "
                    "both directions)"),
    dict(cause="release/FutureDate", why="the `Date` field lives inside the signed bytes of "
         "a real Release; we cannot re-sign it as Debian",
         covered_by="P54 (Release parsing) + code review of the skew gate in apt.rs"),
    dict(cause="release/ValidUntilExpired", why="`stable` carries no `Valid-Until` at all "
         "(contract §0, verified); a suite that has one would have to be signed by Debian",
         covered_by="P54 (Valid-Until parsing) + code review; the residual is a08"),
    dict(cause="ECDSA coverage by a real archive key", why="no live Debian archive key today is "
         "ECDSA-signed, so the end-to-end path cannot exercise the curve",
         covered_by="P52 (`ecdsa_curves_verify_the_digest_through_the_shared_backends`, "
                    "host-generated P-256/P-384) — added in the OpenPGP branch"),
    dict(cause="index/NoIndexEntry", why="the requested path is derived from the signed "
         "Release's own Components/Architectures fields, so an entry cannot be 'missing' "
         "for a path the client actually asks for",
         covered_by="P54 (release_file path lookup miss)"),
    dict(cause="clock/ClockUnset", why="needs the GUEST clock below 2025-01-01, i.e. QEMU "
         "`-rtc base=2020-01-01`; the fixture is the unmodified a01 tree, the harness must "
         "boot with that flag",
         covered_by="e2e: run a01 with `-rtc base=2020-01-01T00:00:00` and expect "
                    "`stage=clock cause=ClockUnset`"),
]


def keyring_map(kr_path: str) -> dict:
    """`fpr -> (role, primary_fpr, uid)` for the committed Debian archive keyring.

    Derived from the keyring bytes, not from a hand-written table: the earlier
    revision of this file listed two signers and called the third one "NOT PINNED",
    while it was a subkey of the third pinned primary. GnuPG 2.4 ignores `--keyring`
    when keyboxd is in play, so the keyring is imported into a throwaway home first
    and read back from there — the same bytes the kernel pins.
    """
    if not os.path.exists(kr_path):
        return {}
    with tempfile.TemporaryDirectory() as home:
        os.chmod(home, 0o700)
        imp = subprocess.run(["gpg", "--homedir", home, "--batch", "--yes", "--import", kr_path],
                             capture_output=True, text=True)
        if imp.returncode != 0:
            return {}
        col = subprocess.run(["gpg", "--homedir", home, "--batch", "--list-keys", "--with-colons",
                              "--with-subkey-fingerprint"], capture_output=True, text=True).stdout
    out: dict = {}
    kind = None
    primary = None
    primary_uid = ""
    pending = None
    for line in col.splitlines():
        parts = line.split(":")
        tag = parts[0]
        if tag in ("pub", "sub"):
            kind = "pub" if tag == "pub" else "sub"
            pending = None
        elif tag == "fpr" and kind and pending is None:
            pending = parts[9]
            if kind == "pub":
                primary = pending
                primary_uid = ""
            if primary:
                out[pending] = (kind, primary, primary_uid)
        elif tag == "uid" and kind == "pub":
            primary_uid = parts[9]
            if primary:
                out[primary] = ("pub", primary, primary_uid)
    return out


def pinned_primaries(root: str) -> dict:
    """`primary fpr -> label` for the keys the kernel pins, when that file is here.

    The fixture branch is checked out from `main`, where the trust-store module may
    not exist yet (it arrives with the OpenPGP PR), so its absence is a NOTE, not a
    failure: the archive keyring above is the self-contained source.
    """
    path = os.path.join(root, "src/pkg/openpgp_keys.rs")
    text = open(path, encoding="utf-8", errors="replace").read() if os.path.exists(path) else ""
    out = {}
    for m in re.finditer(r'fingerprint:\s*\[([^\]]+)\]', text):
        nums = [int(x, 16) if x.strip().startswith("0x") else int(x.strip())
                for x in m.group(1).split(",") if x.strip()]
        fpr = "".join(f"{b:02X}" for b in nums)
        ctx = text[max(0, m.start() - 1500):m.start()]
        labels = re.findall(r'label:\s*"([^"]+)"', ctx)
        out[fpr] = labels[-1] if labels else "?"
    return out


def real_signers(out_root: str) -> dict:
    """Who actually signs the pinned inputs, as an independent verifier sees it.

    `VALIDSIG` reports the fingerprint of the *signing* key, and Debian signs with
    subkeys, so the mapping is resolved through the committed keyring: reporting a
    bare fingerprint list invited exactly the false alarm this function used to
    contain.
    """
    kr = os.path.join(REAL, "debian-archive-keyring.gpg")
    doc = os.path.join(out_root, "a01-reference-inrelease", "dists", SUITE, "InRelease")
    if not (os.path.exists(kr) and os.path.exists(doc)):
        return {"note": "reference cases not built"}
    res = subprocess.run(["gpgv", "--keyring", kr, "--status-fd", "1", doc],
                         capture_output=True, text=True)
    sigs = [l.split()[2] for l in res.stdout.splitlines() if l.startswith("[GNUPG:] VALIDSIG")]
    kmap = keyring_map(kr)
    pinned = pinned_primaries(ROOT)
    via_subkey = 0
    entries = []
    for fpr in sigs:
        role, primary, uid = kmap.get(fpr, (None, None, ""))
        if role == "sub":
            via_subkey += 1
            where = f"SUBKEY of primary {primary}"
        elif role == "pub":
            where = "PRIMARY"
            primary = fpr
        else:
            where = "NOT in the committed Debian archive keyring"
        pin = pinned.get(primary or "", None) if primary else None
        entries.append({
            "fpr": fpr,
            "role": where,
            "primary": primary,
            "uid": uid,
            "pinned_as": pin or ("(src/pkg/openpgp_keys.rs absent in this tree — cross-check "
                                 "against the OpenPGP branch before reading this as unpinned)"
                                 if not pinned else "NOT PINNED (unexpected!)"),
        })
    return {
        "verified_by": "gpgv (GnuPG 2.4) with the committed debian-archive-keyring.gpg",
        "valid_signatures": sigs,
        "fingerprints": entries,
        "subkey_signatures": f"{via_subkey} of {len(sigs)} signatures come from a SUBKEY of a "
                             f"pinned primary",
        "why_it_matters": "the kernel must map a signing subkey to its pinned primary "
                          "(contract §3.5); reference cases signed through a subkey exercise "
                          "that path end-to-end",
    }


def build(args) -> None:
    out_root = os.path.abspath(args.out)
    os.makedirs(out_root, exist_ok=True)
    home = os.path.join(out_root, "_gnupg")
    if os.path.exists(home):
        shutil.rmtree(home)
    import_keys(home)
    table = cases()
    manifest_cases = []
    for case in table:
        if args.only and case["id"] not in args.only:
            continue
        built = build_case(case, out_root, home, suite_mode=args.suite)
        entry = dict(case)
        entry.update(built)
        entry["tree"] = built["tree"]
        manifest_cases.append(entry)
        print(f"built {case['id']:34} -> {entry['tree']}")
    reference = real_signers(out_root)
    manifest = {
        "generated_by": "tools/openpgp_attack_fixtures.py",
        "reference_signers": reference,
        "trust_root": "src/pkg/apt.rs::trusted_keyring() = &DEBIAN_KEYRING "
                      "(3 pinned Debian keys, no test anchor)",
        "serving": "each case directory is a complete apt repository root: serve "
                   "`<case>/` at the mirror root (the pool/ and dists/ layout matches what "
                   "apt fetches), then run `apt update` / `apt install`",
        "suite_mode": args.suite,
        "serving_suite": {c["id"]: c.get("configured_suite", SUITE) for c in manifest_cases},
        "index_path": f"dists/*/{COMPONENT}/{ARCH}/{INDEX_NAME}",
        "package": json.load(open(os.path.join(REAL, "inventory.json")))["deb"]["package"],
        "signing_keys": [
            {"purpose": k["purpose"], "fingerprint": k["fingerprint"], "public": k["public"]}
            for k in keys_meta()["keys"]
        ],
        "cases": manifest_cases,
        "not_constructible": NOT_CONSTRUCTIBLE,
    }
    with open(os.path.join(out_root, "manifest.json"), "w", encoding="utf-8") as fh:
        json.dump(manifest, fh, indent=2, ensure_ascii=False)
        fh.write("\n")
    print(f"manifest: {os.path.relpath(os.path.join(out_root, 'manifest.json'), ROOT)}")


def check(args) -> None:
    """Reference check with GnuPG: the local-key fixtures must be VALID signatures
    (so the kernel's refusal is about trust, not about malformed input), and the
    real ones must verify under the Debian keyring."""
    out_root = os.path.abspath(args.out)
    home = os.path.join(out_root, "_gnupg")
    ok = True
    # The served suite directory is per case: `stable` for the Debian-signed trees,
    # the case id for the ones we sign ourselves (`--suite case`).
    manifest_path = os.path.join(out_root, "manifest.json")
    suites = {}
    if os.path.exists(manifest_path):
        with open(manifest_path, encoding="utf-8") as fh:
            for c in json.load(fh)["cases"]:
                suites[c["id"]] = c.get("configured_suite", SUITE)
    for cid in ["b01-untrusted-signer", "b05-expired-untrusted-signer",
                "b03-armor-crc-mismatch"]:
        tree = os.path.join(out_root, cid, "dists", suites.get(cid, SUITE))
        if not os.path.isdir(tree):
            # A missing tree must fail the check: "nothing to verify" is not "verified".
            print(f"FAIL {cid}: not built (run `build` first)")
            ok = False
            continue
        # The combined fixture keyring: gpgv has no --faked-system-time, so the
        # expired-key case is proven well-formed by the signature itself (gpgv
        # reports it as good and only notes the expiry).
        cmd = ["gpgv", "--keyring", os.path.join(KEYS, "fixture_keys.pub.gpg")]
        cmd += [os.path.join(tree, "Release.gpg"), os.path.join(tree, "Release")]
        res = subprocess.run(cmd, capture_output=True, text=True,
                             env=dict(os.environ, GNUPGHOME=home))
        valid = "Good signature" in res.stderr or "Действительная подпись" in res.stderr \
            or res.returncode == 0
        want = cid != "b03-armor-crc-mismatch"
        ok &= valid == want
        print(f"{'ok ' if valid == want else 'FAIL'} {cid}: gpgv says "
              f"{'valid' if valid else 'invalid'} (expected {'valid' if want else 'invalid'})")
    print("reference check:", "OK" if ok else "FAILED")
    if not ok:
        sys.exit(1)


def main() -> None:
    ap = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    sub = ap.add_subparsers(dest="cmd", required=True)
    b = sub.add_parser("build", help="build the case trees + manifest")
    b.add_argument("--out", default=DEFAULT_OUT)
    b.add_argument("--only", nargs="*", default=None)
    b.add_argument("--suite", choices=["stable", "case"], default="stable",
                   help="suite directory the trees are laid out under, and the field the "
                        "Release carries: `stable` for everything (required for the `a*` "
                        "trees, whose signature is Debian's), `case` puts the locally signed "
                        "`b*` trees under dists/<case_id>/ with `Suite: <case_id>` so a "
                        "harness can use `apt setsuite <case_id>`")
    b.set_defaults(func=build)
    c = sub.add_parser("check", help="reference-verify the built fixtures with gpgv")
    c.add_argument("--out", default=DEFAULT_OUT)
    c.set_defaults(func=check)
    l = sub.add_parser("list", help="print the case table")
    l.set_defaults(func=lambda a: [print(f"{c['id']:34} {c['expect']:6} "
                                        f"{c['stage'] or '-':10} {c['cause'] or '-'}") for c in cases()])
    args = ap.parse_args()
    args.func(args)


if __name__ == "__main__":
    main()
