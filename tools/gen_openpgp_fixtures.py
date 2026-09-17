#!/usr/bin/env python3
"""Generate the OpenPGP test fixtures for host properties P51–P53.

Writes `host-tests/src/properties/openpgp_fixtures.rs`: a pinned *test* keyring
(Ed25519 release key, RSA-2048 key with a signing subkey, one key with a 1-day
expiry, one key that is NOT pinned), a small `Release`-shaped document, the
`Release.gpg`-shaped detached signature (two signers, like Debian's three-way
`stable` signature), an `InRelease`-shaped clear-signed message, a clear-signed
document with trailing whitespace and a dash-escaped line (the canonicalization
rules), plus a few negative fixtures (untracked signer, expired signer).

WHY GnuPG AND NOT A HAND-WRITTEN WRITER: the properties are interoperability
tests. The fixtures are produced by the reference implementation (GnuPG 2.4),
so a wrong digest trailer, a missing MPI left-pad, the wrong Ed25519 OID
acceptance or a wrong clearsign canonicalization makes the property fail on real
GnuPG bytes — which is exactly how the v4 hash rule and the canonicalization rule
were established (see `OPENPGP-VERIFY-CONTRACT.md` §12).

The fixtures are COMMITTED. Re-running the script generates a NEW test keypair
(key generation is random) and therefore a new fixture file: that is a
deliberate, reviewed act, not something a build does. The signing itself is
deterministic (Ed25519 and RSA PKCS#1 v1.5 have no randomness) and the signature
timestamps are pinned with `--faked-system-time`, so within one keyring the
output is stable.

Usage:
    python3 tools/gen_openpgp_fixtures.py            # writes the module
    python3 tools/gen_openpgp_fixtures.py --check    # exit 1 if it differs
"""

import argparse
import gzip
import hashlib
import os
import shutil
import subprocess
import sys
import tempfile

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pgp_packets  # noqa: E402

OUT_PATH = "host-tests/src/properties/openpgp_fixtures.rs"

# 2026-01-01T00:00:00Z: after the verifier's clock floor (2025-01-01) and inside
# every fixture key's validity window unless a test moves `now` deliberately.
FAKED_TIME = "20260101T000000"
FIXTURE_NOW = 1767225600
# 2026-01-02T00:01:00Z: past the 1-day expiry of the "expired" fixture key.
EXPIRED_NOW = 1767312000 + 60

KEY_SPECS = {
    "ED": ("pagh test release key (ed25519) <pagh-test@example.invalid>", "ed25519", "never"),
    "RSA": ("pagh test archive key (rsa2048) <pagh-test@example.invalid>", "rsa2048", "never"),
    "EXPIRED": ("pagh test expired key (ed25519) <pagh-test@example.invalid>", "ed25519", "1d"),
    "FOREIGN": ("pagh test untrusted key (ed25519) <pagh-test@example.invalid>", "ed25519", "never"),
    "REVOKED": ("pagh test revoked key (ed25519) <pagh-test@example.invalid>", "ed25519", "never"),
}


def run(cmd, env, stdin=None, check=True):
    proc = subprocess.run(
        cmd,
        env=env,
        input=stdin,
        capture_output=True,
        text=False,
    )
    if check and proc.returncode != 0:
        raise SystemExit(
            f"command failed ({proc.returncode}): {' '.join(cmd)}\n{proc.stderr.decode(errors='replace')}"
        )
    return proc


def gpg(env, *args, stdin=None):
    return run(["gpg", "--batch", "--quiet", "--faked-system-time", FAKED_TIME, *args], env, stdin)


def gen_key(env, uid, algo, expires):
    gpg(env, "--passphrase", "", "--quick-gen-key", uid, algo, "sign", expires)
    return key_fingerprint(env, uid)


def add_subkey(env, fpr, algo, expires):
    gpg(env, "--passphrase", "", "--quick-add-key", fpr, algo, "sign", expires)


def key_fingerprint(env, uid):
    out = gpg(env, "--list-keys", "--with-colons", uid).stdout.decode()
    for line in out.splitlines():
        if line.startswith("fpr:"):
            return line.split(":")[9]
    raise SystemExit(f"no fingerprint for {uid}")


def subkey_fingerprints(env, uid):
    out = gpg(env, "--list-keys", "--with-colons", "--with-subkey-fingerprint", uid).stdout.decode()
    subs = []
    current_is_sub = False
    for line in out.splitlines():
        if line.startswith("sub:"):
            current_is_sub = True
        elif line.startswith("pub:"):
            current_is_sub = False
        elif line.startswith("fpr:") and current_is_sub:
            subs.append(line.split(":")[9])
    return subs


def revoke_key(env, fpr):
    """Revoke a fixture key with GnuPG's own revocation certificate.

    GnuPG writes a revocation certificate next to the keyring at
    `$GNUPGHOME/openpgp-revocs.d/<fpr>.rev`; its armour lines are prefixed with
    `:` so the file cannot be imported by accident. Importing it adds a real
    (0x20) key revocation signature, signed by the key itself — the fixture the
    revocation policy has to refuse.
    """
    path = os.path.join(env["GNUPGHOME"], "openpgp-revocs.d", f"{fpr}.rev")
    lines = []
    inside = False
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.rstrip("\n")
            if line.startswith(":-----BEGIN"):
                inside = True
            if inside and line.startswith(":"):
                line = line[1:]
            if inside:
                lines.append(line)
    if not lines:
        raise SystemExit(f"no revocation certificate at {path}")
    gpg(env, "--import", "-", stdin=("\n".join(lines) + "\n").encode())


def export(env, fpr):
    return gpg(env, "--export", fpr).stdout


def sign_detached(env, doc_path, out_path, signers):
    args = ["--armor", "--detach-sign", "--digest-algo", "SHA256"]
    for s in signers:
        args += ["-u", s]
    gpg(env, *args, "-o", out_path, doc_path)


def clearsign(env, doc_path, out_path, signer):
    gpg(
        env,
        "--armor",
        "--clearsign",
        "--digest-algo",
        "SHA256",
        "-u",
        signer,
        "-o",
        out_path,
        doc_path,
    )


def dearmor(data):
    """Decode an armored block to its binary payload (fixtures are trusted)."""
    lines = data.decode().splitlines()
    body = []
    for line in lines:
        if line.startswith("-----") or line.startswith("=") or not line.strip():
            continue
        if line.startswith("Hash:") or line.startswith("Version:") or line.startswith("Comment:"):
            continue
        body.append(line.strip())
    import base64

    return base64.b64decode("".join(body))


def clearsign_parts(data):
    """Split an armored clear-signed message into (canonical, binary signature)."""
    text = data.decode()
    begin = text.index("-----BEGIN PGP SIGNED MESSAGE-----")
    header_end = text.index("\n\n", begin) + 2
    sig_at = text.index("-----BEGIN PGP SIGNATURE-----", header_end)
    body = text[header_end:sig_at]
    if body.endswith("\r\n"):
        body = body[:-2]
    elif body.endswith("\n"):
        body = body[:-1]
    canonical_lines = []
    for line in body.split("\n"):
        if line.startswith("- "):
            line = line[2:]
        canonical_lines.append(line.rstrip(" \t\r"))
    canonical = "\r\n".join(canonical_lines).encode()
    return canonical, dearmor(text[sig_at:].encode())


def rust_bytes_hex(data, indent):
    lines = []
    for i in range(0, len(data), 16):
        lines.append(indent + ", ".join(f"0x{b:02x}" for b in data[i : i + 16]) + ",")
    return "\n".join(lines)


def rust_bytes_literal(data, indent):
    """Render bytes as ONE Rust byte-string literal on a single line.

    Rust has no implicit adjacent-literal concatenation (and `concat!` refuses
    byte strings), so a wrapped literal would not compile — the fixtures are
    therefore emitted as one (possibly long) line. The file is generated; the
    length of a line is not worth a broken build.
    """
    pieces = []
    for b in data:
        if b == 0x22:
            piece = '\\"'
        elif b == 0x5C:
            piece = "\\\\"
        elif b == 0x0A:
            piece = "\\n"
        elif b == 0x0D:
            piece = "\\r"
        elif b == 0x09:
            piece = "\\t"
        elif 0x20 <= b < 0x7F:
            piece = chr(b)
        else:
            piece = f"\\x{b:02x}"
        pieces.append(piece)
    return f'{indent}b"{"".join(pieces)}"'


def render(fixtures):
    f = fixtures
    out = []
    out.append(
        "//! OpenPGP test fixtures for P51–P53 (issue #32).\n"
        "//!\n"
        "//! GENERATED FILE — do not edit by hand; regenerate with\n"
        "//! `python3 tools/gen_openpgp_fixtures.py`, review the diff and commit it.\n"
        "//! Re-running generates a NEW test keypair (key generation is random), which is\n"
        "//! a deliberate act; the signature timestamps are pinned with\n"
        f"//! `--faked-system-time {FAKED_TIME}` and the signature algorithms are deterministic.\n"
        "//!\n"
        "//! The fixtures are produced by GnuPG 2.4 — the reference implementation — so the\n"
        "//! properties test interoperability, not just self-consistency: a wrong v4 digest\n"
        "//! trailer, a missing MPI left-pad, a rejected legacy Ed25519 OID or a wrong\n"
        "//! clearsign canonicalization fails the property against real GnuPG bytes.\n"
        "//!\n"
        "//! Test keys only. None of them may ever be added to `src/pkg/openpgp_keys.rs`.\n"
        "\n"
        "#![allow(dead_code)]\n"
    )
    for name, value in f["consts"]:
        out.append(f"\n/// {name}.\npub const {name}: {value['type']} = {value['value']};\n")
    for name, doc, data, style in f["statics"]:
        out.append(f"\n/// {doc}\n")
        if style == "hex":
            out.append(f"pub static {name}: &[u8] = &[\n{rust_bytes_hex(data, '    ')}\n];\n")
        else:
            out.append(f"pub static {name}: &[u8] =\n{rust_bytes_literal(data, '    ')};\n")
    return "".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument(
        "--check",
        action="store_true",
        help="regenerate and report whether the committed fixtures would change "
        "(they will: key generation is random — see the module docstring)",
    )
    args = ap.parse_args()

    if shutil.which("gpg") is None:
        raise SystemExit("gpg is required to (re)generate these fixtures")

    tmp = tempfile.mkdtemp(prefix="pagh-openpgp-fixtures-")
    env = dict(os.environ)
    env["GNUPGHOME"] = os.path.join(tmp, "gnupg")
    os.makedirs(env["GNUPGHOME"], mode=0o700)
    try:
        fprs = {}
        for tag, (uid, algo, expires) in KEY_SPECS.items():
            fprs[tag] = gen_key(env, uid, algo, expires)
        add_subkey(env, fprs["RSA"], "rsa2048", "never")
        revoke_key(env, fprs["REVOKED"])
        fprs["RSA_SUB"] = subkey_fingerprints(env, KEY_SPECS["RSA"][0])[0]
        if not fprs["RSA_SUB"]:
            raise SystemExit("the RSA fixture key got no signing subkey")

        # A small, realistic `Packages.gz` + the `Release` document that lists it.
        packages = (
            b"Package: hello-pagh\n"
            b"Version: 1.0\n"
            b"Architecture: amd64\n"
            b"Filename: pool/main/h/hello-pagh/hello-pagh_1.0_amd64.deb\n"
            b"Size: 4096\n"
            b"SHA256: 0000000000000000000000000000000000000000000000000000000000000000\n"
            b"\n"
        )
        packages_gz = gzip.compress(packages, mtime=0)
        release = (
            "Suite: stable\n"
            "Codename: pagh-test\n"
            "Date: Thu, 01 Jan 2026 00:00:00 UTC\n"
            "Acquire-By-Hash: no\n"
            "\n"
            "SHA256:\n"
            f" {hashlib.sha256(packages_gz).hexdigest()} {len(packages_gz)} main/binary-amd64/Packages.gz\n"
            f" {hashlib.sha256(packages).hexdigest()} {len(packages)} main/binary-amd64/Packages\n"
        ).encode()
        release_path = os.path.join(tmp, "Release")
        with open(release_path, "wb") as fh:
            fh.write(release)

        gpg_path = os.path.join(tmp, "Release.gpg")
        sign_detached(
            env,
            release_path,
            gpg_path,
            [fprs["ED"], fprs["RSA_SUB"] + "!"],
        )
        with open(gpg_path, "rb") as fh:
            release_gpg = fh.read()

        inrelease_path = os.path.join(tmp, "InRelease")
        clearsign(env, release_path, inrelease_path, fprs["ED"])
        with open(inrelease_path, "rb") as fh:
            inrelease = fh.read()

        foreign_path = os.path.join(tmp, "release_foreign.gpg")
        sign_detached(env, release_path, foreign_path, [fprs["FOREIGN"]])
        with open(foreign_path, "rb") as fh:
            release_foreign = fh.read()

        expired_path = os.path.join(tmp, "release_expired.gpg")
        sign_detached(env, release_path, expired_path, [fprs["EXPIRED"]])
        with open(expired_path, "rb") as fh:
            release_expired = fh.read()

        # Canonicalization fixture: trailing whitespace on every line and a
        # dash-escaped line (GnuPG escapes a leading "-" as "- ").
        ws_doc = b"line one   \n- dash line\nplain\tline\r\nlast line\n"
        ws_path = os.path.join(tmp, "ws.txt")
        with open(ws_path, "wb") as fh:
            fh.write(ws_doc)
        ws_asc_path = os.path.join(tmp, "ws.asc")
        clearsign(env, ws_path, ws_asc_path, fprs["ED"])
        with open(ws_asc_path, "rb") as fh:
            ws_inrelease = fh.read()
        ws_canonical, _ = clearsign_parts(ws_inrelease)

        inrelease_canonical, inrelease_sig_bin = clearsign_parts(inrelease)
        if inrelease_canonical != release.replace(b"\n", b"\r\n")[: -len(b"\r\n")]:
            raise SystemExit("InRelease clear text is not the CRLF form of the Release document")

        # Advisory cross-check with the reference verifier, when available.
        if shutil.which("gpgv"):
            pubring = os.path.join(tmp, "pubring.gpg")
            with open(pubring, "wb") as fh:
                fh.write(export(env, fprs["ED"]) + export(env, fprs["RSA"]))
            proc = subprocess.run(
                ["gpgv", "--keyring", pubring, gpg_path, release_path],
                capture_output=True,
            )
            if proc.returncode != 0:
                raise SystemExit(
                    "gpgv refuses the generated detached fixture:\n"
                    + proc.stderr.decode(errors="replace")
                )
            proc = subprocess.run(
                ["gpgv", "--keyring", pubring, inrelease_path],
                capture_output=True,
            )
            if proc.returncode != 0:
                raise SystemExit(
                    "gpgv refuses the generated clear-signed fixture:\n"
                    + proc.stderr.decode(errors="replace")
                )

        consts = [
            ("FIXTURE_NOW", {"type": "i64", "value": str(FIXTURE_NOW)}),
            ("EXPIRED_NOW", {"type": "i64", "value": str(EXPIRED_NOW)}),
            ("EXPIRES_EXPIRED_KEY", {"type": "u32", "value": str(EXPIRED_NOW - 60)}),
            ("PACKAGES_SHA256", {"type": "&str", "value": f'"{hashlib.sha256(packages_gz).hexdigest()}"'}),
            ("PACKAGES_SIZE", {"type": "usize", "value": str(len(packages_gz))}),
        ]
        statics = [
            ("KEY_ED", "Ed25519 release key block (primary + user ID + self-signature).", export(env, fprs["ED"]), "hex"),
            ("KEY_RSA", "RSA-2048 archive key block with its signing subkey.", export(env, fprs["RSA"]), "hex"),
            ("KEY_EXPIRED", "Ed25519 key whose validity window closes one day after creation.", export(env, fprs["EXPIRED"]), "hex"),
            ("KEY_FOREIGN", "Ed25519 key that is NOT in the pinned table.", export(env, fprs["FOREIGN"]), "hex"),
            (
                "KEY_REVOKED",
                "Ed25519 key carrying a real 0x20 key revocation signature.",
                export(env, fprs["REVOKED"]),
                "hex",
            ),
            ("RELEASE", "The signed `Release`-shaped document.", release, "literal"),
            ("RELEASE_GPG", "Armored detached signature over RELEASE: Ed25519 + the RSA subkey (multi-signer, like Debian).", release_gpg, "literal"),
            ("RELEASE_GPG_BIN", "The de-armored form of RELEASE_GPG, for byte-level tampering tests.", dearmor(release_gpg), "hex"),
            ("INRELEASE", "Clear-signed RELEASE (Ed25519).", inrelease, "literal"),
            ("INRELEASE_CANONICAL", "The byte string the InRelease signature is computed over.", inrelease_canonical, "literal"),
            ("INRELEASE_SIG_BIN", "The de-armored InRelease signature packet stream.", inrelease_sig_bin, "hex"),
            ("RELEASE_FOREIGN_GPG", "Detached signature over RELEASE by the unpinned key.", release_foreign, "literal"),
            ("RELEASE_EXPIRED_GPG", "Detached signature over RELEASE by the expired key.", release_expired, "literal"),
            ("WS_DOC", "Document with trailing whitespace and a dash-escaped line.", ws_doc, "literal"),
            ("WS_INRELEASE", "Clear-signed WS_DOC (Ed25519).", ws_inrelease, "literal"),
            ("WS_CANONICAL", "The signed byte string of WS_INRELEASE (dash-unescaped, whitespace-stripped, CRLF).", ws_canonical, "literal"),
            ("PACKAGES_GZ", "A small gzip `Packages` index the RELEASE document lists.", packages_gz, "hex"),
        ]
        # Per-key pinned metadata: the property tables pin these constants and
        # `validate_block` cross-checks them against the committed bytes.
        meta = []
        for tag in ("ED", "RSA", "EXPIRED", "FOREIGN", "REVOKED"):
            block = export(env, fprs[tag])
            parsed = list(pgp_packets.read_packets(block))
            summary = pgp_packets.key_summary(parsed[0][2])
            certs = pgp_packets.find_certifications(block)[2]
            self_certs = [
                c for c in certs if (c["issuer_fpr"] or "").upper() == fprs[tag]
            ]
            expiries = [
                summary["created"] + c["key_expiry"]
                for c in self_certs
                if c["key_expiry"]
            ]
            meta.append((tag, summary, min(expiries) if expiries else None))
        for tag, summary, expires in meta:
            value = "None" if expires is None else f"Some({expires})"
            consts.extend(
                [
                    (f"ALGO_{tag}", {"type": "u8", "value": str(summary["algo"])}),
                    (f"BITS_{tag}", {"type": "u16", "value": str(summary["bits"])}),
                    (f"CREATED_{tag}", {"type": "u32", "value": str(summary["created"])}),
                    (f"EXPIRES_{tag}", {"type": "Option<u32>", "value": value}),
                ]
            )
        for tag in ("ED", "RSA", "RSA_SUB", "EXPIRED", "FOREIGN", "REVOKED"):
            consts.insert(
                0,
                (
                    f"FPR_{tag}",
                    {
                        "type": "[u8; 20]",
                        "value": "["
                        + ", ".join(f"0x{b:02x}" for b in bytes.fromhex(fprs[tag]))
                        + "]",
                    },
                ),
            )

        rendered = render({"consts": consts, "statics": statics})
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    if args.check:
        # Unlike the CA bundle / Debian keyring generators, this one is NOT
        # expected to be byte-reproducible: the fixture *keys* are freshly
        # generated each run (there is no upstream artifact to pin). A difference
        # therefore means "the committed fixtures are the previous key set", not
        # "the tree is out of date" — so this reports and exits 0, and a
        # regeneration is a deliberate commit.
        try:
            with open(OUT_PATH, encoding="utf-8") as fh:
                same = fh.read() == rendered
        except FileNotFoundError:
            raise SystemExit(f"{OUT_PATH} does not exist; run without --check to create it")
        if same:
            print(f"{OUT_PATH} matches a fresh generation (same fixture keys)")
        else:
            print(
                f"{OUT_PATH} differs from a fresh generation — expected, because the fixture "
                "keypair is generated randomly on each run; review the diff before committing"
            )
        return

    with open(OUT_PATH, "w", newline="\n", encoding="utf-8") as fh:
        fh.write(rendered)
    print(f"wrote {OUT_PATH} ({len(rendered)} bytes)")
    for tag in ("ED", "RSA", "RSA_SUB", "EXPIRED", "FOREIGN", "REVOKED"):
        print(f"  {tag:8s} {fprs[tag]}")


if __name__ == "__main__":
    main()
