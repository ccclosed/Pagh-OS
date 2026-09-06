#!/usr/bin/env python3
"""Generate src/net/ca_bundle.rs — the kernel's TLS trust-anchor bundle.

Issue #14 series (PR 6): the TLS verifier must authenticate server chains
against a fixed set of trusted self-signed roots. The generated file is
COMMITTED to the repo; re-run this script only to deliberately refresh it.

TRUST MODEL — the download is NOT trusted on its own:
  * every root is PINNED by its sha256 in SELECTION below (verified by hand
    against the operator's own independent copy at pinning time);
  * the script selects each root BY THE PINNED HASH, then cross-checks the
    subject CN inside the pinned DER against the expected CN — a substituted
    or re-issued certificate fails the run instead of silently entering the
    kernel's trust base;
  * only SELF-SIGNED certificates (issuer == subject) are eligible; if a CN
    collision appears in the source (historically: cross-signed variants of
    GTS roots share a subject CN), the self-signed copy is chosen, and
    ambiguity that survives the filter is a hard error — never a silent pick.

Determinism: no timestamps, fixed selection order and formatting. The script
writes the raw module and then runs `rustfmt` over it — the committed
artifact is the output of THIS pipeline (generate + rustfmt), so a plain
regeneration must be byte-identical to what is committed.

Usage: python tools/gen_ca_bundle.py
"""

import base64
import gzip
import hashlib
import shutil
import subprocess
import sys
import urllib.request

SOURCE_URL = "https://curl.se/ca/cacert.pem"

# The exact bundle contents, in output order: (expected subject CN, pinned
# sha256 of the root's DER). A root is selected BY THE HASH; the CN is a
# cross-check against substitution (same hash collision aside, a different
# certificate cannot pass both). Update a pin only as a deliberate,
# reviewed act: it changes what the kernel's TLS will trust.
SELECTION = [
    # Let's Encrypt roots: deb.debian.org was probed serving a Let's
    # Encrypt-issued leaf, so ISRG anchors the apt mirror surface
    # (X1 = RSA intermediates, X2 = ECC intermediates).
    ("ISRG Root X1", "96bcec06264976f37460779acf28c5a7cfe8a3c0aae11a8ffcee05c0bddf08c6"),
    ("ISRG Root X2", "69729b8e15a86efc177a57afb7171dfc64add28c2fca8cf1507e34453ccb1470"),
    # Google Trust Services roots (CDN edges).
    ("GTS Root R1", "d947432abde7b7fa90fc2e6b59101b1280e0e1c7e4e40fa3c6887fff57a7f4cf"),
    ("GTS Root R4", "349dfa4058c5e263123b398ae795573c4e1313c83fe68f93556cd5e8031b3c7d"),
]

OUT_PATH = "src/net/ca_bundle.rs"


def fetch_source(url: str, retries: int = 3) -> bytes:
    last = None
    for attempt in range(1, retries + 1):
        try:
            req = urllib.request.Request(
                url,
                headers={
                    "User-Agent": "pagh-gen-ca-bundle/1.0",
                    "Accept-Encoding": "identity",
                },
            )
            with urllib.request.urlopen(req, timeout=60) as resp:
                data = resp.read()
            # Defensive: if a proxy compressed the body despite `identity`,
            # detect the gzip magic and decompress.
            if data[:2] == b"\x1f\x8b":
                data = gzip.decompress(data)
            return data
        except Exception as e:  # noqa: BLE001 — report and retry
            last = e
            print(f"  fetch attempt {attempt}/{retries} failed: {e}", file=sys.stderr)
    raise SystemExit(f"cannot download {SOURCE_URL}: {last}")


def pem_blocks(data: bytes):
    text = data.decode("utf-8", errors="strict")
    marker_begin = "-----BEGIN CERTIFICATE-----"
    marker_end = "-----END CERTIFICATE-----"
    block = []
    inside = False
    for line in text.splitlines():
        if line.strip() == marker_begin:
            inside = True
            block = []
        elif line.strip() == marker_end:
            inside = False
            yield base64.b64decode("".join(block))
        elif inside:
            block.append(line.strip())


def read_tlv(buf: bytes, i: int):
    tag = buf[i]
    i += 1
    first = buf[i]
    i += 1
    if first < 0x80:
        length = first
    else:
        n = first & 0x7F
        if n == 0 or n > 4 or i + n > len(buf):
            raise ValueError("non-DER length")
        length = int.from_bytes(buf[i : i + n], "big")
        i += n
    if i + length > len(buf):
        raise ValueError("truncated")
    return tag, buf[i : i + length], i + length


def walk_name(name_content: bytes):
    """CONTENT of a Name SEQUENCE -> list of (OID bytes, value bytes).

    Name = SEQUENCE OF SET OF AttributeTypeAndValue, and
    AttributeTypeAndValue = SEQUENCE { type OID, value } — so exactly TWO
    levels of TLV live inside the Name content: SET, then the AtV SEQUENCE
    whose content is the OID element followed by the value element.
    """
    pairs = []
    j = 0
    while j < len(name_content):
        tag, rdnset, j = read_tlv(name_content, j)
        assert tag == 0x31
        k = 0
        while k < len(rdnset):
            tag, atv, k = read_tlv(rdnset, k)
            assert tag == 0x30
            _, oid, p = read_tlv(atv, 0)
            _, val, _ = read_tlv(atv, p)
            pairs.append((oid, val))
    return pairs


def cert_names(der: bytes):
    """Return (issuer pairs, subject pairs) of a certificate DER."""
    tag, cert, _ = read_tlv(der, 0)
    assert tag == 0x30
    tag, tbs, _ = read_tlv(cert, 0)
    assert tag == 0x30
    i = 0
    tag, first, i = read_tlv(tbs, 0)
    if tag != 0xA0:
        # v1 certificate: no [0] EXPLICIT version — the element we just read
        # WAS the serial; rewind so the loop below re-reads it.
        i = 0
    # elements after the optional [0] version: serial, sigAlg, issuer,
    # validity, subject (note validity sits BETWEEN issuer and subject).
    tag, serial_el, i = read_tlv(tbs, i)
    tag, alg_el, i = read_tlv(tbs, i)
    tag, issuer_el, i = read_tlv(tbs, i)
    tag, validity_el, i = read_tlv(tbs, i)
    tag, subject_el, i = read_tlv(tbs, i)
    assert tag == 0x30
    return walk_name(issuer_el), walk_name(subject_el)


def cn_of(pairs) -> str:
    for oid, val in pairs:
        if oid == b"\x55\x04\x03":
            return val.decode("latin1")
    raise ValueError("name has no CN")


def rust_bytes(der: bytes, indent: str) -> str:
    hexed = ", ".join(f"0x{b:02x}" for b in der)
    # wrap at ~100 columns: chunk the hex list
    parts = hexed.split(", ")
    lines = []
    cur = []
    width = 0
    for part in parts:
        if width + len(part) + 2 > 96 and cur:
            lines.append(", ".join(cur))
            cur = []
            width = 0
        cur.append(part)
        width += len(part) + 2
    if cur:
        lines.append(", ".join(cur))
    body = f",\n{indent}".join(lines)
    return body


def select_pinned_roots(certs: list[bytes]):
    """Select every SELECTION root by its pinned hash; fail closed on any
    deviation. Returns [(cn, der)] in SELECTION order."""
    by_hash = {hashlib.sha256(der).hexdigest(): der for der in certs}
    out = []
    for cn, pin in SELECTION:
        der = by_hash.get(pin)
        if der is None:
            raise SystemExit(
                f"PINNED ROOT MISSING: {cn} (sha256 {pin}) is not in the source. "
                "If the source legitimately changed, update the pin as a reviewed act — "
                "this changes what the kernel's TLS trusts."
            )
        issuer, subject = cert_names(der)
        actual_cn = cn_of(subject)
        if actual_cn != cn:
            raise SystemExit(
                f"PIN MISMATCH: {cn} (sha256 {pin}) has subject CN {actual_cn!r} — "
                "the pin does not describe the certificate it selects."
            )
        if issuer != subject:
            raise SystemExit(
                f"NOT SELF-SIGNED: {cn} (sha256 {pin}) has issuer != subject — "
                "a trust anchor must be a self-signed root."
            )
        out.append((cn, der))
    return out


def main() -> None:
    print(f"downloading {SOURCE_URL} ...")
    data = fetch_source(SOURCE_URL)
    certs = list(pem_blocks(data))
    print(f"source contains {len(certs)} certificates")

    roots = select_pinned_roots(certs)

    # Emit the module: fixed order, fixed formatting — deterministic; the
    # rustfmt pass below makes the committed artifact canonical.
    out = []
    out.append("//! Trust-anchor bundle for the TLS certificate verifier (issue #14).\n")
    out.append("//!\n")
    out.append("//! GENERATED FILE — do not edit by hand; regenerate with\n")
    out.append("//! `tools/gen_ca_bundle.py` (source: the curl/Mozilla CA extract,\n")
    out.append(f"//! `{SOURCE_URL}`; every root is pinned by sha256 inside the\n")
    out.append("//! generator and the run fails closed on any mismatch), then commit\n")
    out.append("//! the result. The pipeline is `generate` + `rustfmt`, so a plain\n")
    out.append("//! regeneration is byte-identical to this file.\n")
    out.append("//!\n")
    out.append("//! Each entry is one self-signed root CA: raw DER plus a human label\n")
    out.append("//! (the subject CN, cross-checked against the DER at generation time\n")
    out.append("//! and re-checked by property P48 against the committed bytes).\n")
    out.append("//! `net::tls_chain::TrustAnchor` pairs the root's subject with its\n")
    out.append("//! key at verifier start-up. Pure `core` — also `#[path]`-included\n")
    out.append("//! by the host tests (P48).\n")
    out.append("\n#![allow(dead_code)] // consumed by the TlsVerifier in the next PR of the series.\n\n")
    out.append("/// One trust anchor: label (subject CN) + raw DER of the root certificate.\n")
    out.append("pub static CA_BUNDLE: &[(&str, &[u8])] = &[\n")
    for cn, der in roots:
        fp = hashlib.sha256(der).hexdigest()
        out.append(f"    // sha256 {fp}\n")
        out.append(f"    (\n        \"{cn}\",\n        &[\n{rust_bytes(der, '            ')},\n        ],\n    ),\n")
    out.append("];\n")

    with open(OUT_PATH, "w", newline="\n", encoding="utf-8") as f:
        f.write("".join(out))

    # The committed artifact is the RUSTFMT-CANONICAL form: run the same
    # formatter `cargo fmt --all` would apply, so a regeneration equals the
    # committed bytes (no silent formatting drift between script and repo).
    rustfmt = shutil.which("rustfmt")
    if rustfmt is None:
        raise SystemExit("rustfmt not found on PATH; cannot produce the canonical artifact")
    subprocess.run([rustfmt, "--edition", "2021", OUT_PATH], check=True)

    print(f"wrote {OUT_PATH} with {len(roots)} anchors:")
    for cn, der in roots:
        print(f"  {cn}: {len(der)} bytes, sha256 {hashlib.sha256(der).hexdigest()}")


if __name__ == "__main__":
    main()
