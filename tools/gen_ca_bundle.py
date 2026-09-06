#!/usr/bin/env python3
"""Generate src/net/ca_bundle.rs — the kernel's TLS trust-anchor bundle.

Issue #14 series (PR 6): the TLS verifier must authenticate server chains
against a fixed set of trusted self-signed roots. This script downloads ONE
stable source (the curl/Mozilla CA extract), selects a curated list of roots
by exact subject CN, normalizes each certificate to raw DER, and emits a
pure-`core` Rust module (static byte arrays + labels). The generated file is
COMMITTED to the repo; re-run this script only to deliberately refresh it.

Determinism: the output depends only on the selection list and the source
certificates — no timestamps, no environment data, fixed order, fixed
formatting. Re-runs against the same source produce byte-identical output.

The selected set covers the real HTTPS surfaces the kernel talks to:
  * ISRG Root X1 / X2 — Let's Encrypt chains (deb.debian.org and friends);
  * GlobalSign Root R3 — Fastly/CDN chains (historical deb.debian.org);
  * GTS Root R1 / R4   — Google Trust Services chains (other CDN edges).

Usage: python tools/gen_ca_bundle.py
"""

import base64
import hashlib
import ssl
import sys
import urllib.request

SOURCE_URL = "https://curl.se/ca/cacert.pem"

# (subject CN) — the exact bundle contents, in output order.
# Covers the real HTTPS surfaces the kernel talks to:
#   * ISRG Root X1 / X2 — Let's Encrypt chains: deb.debian.org was probed
#     serving a leaf issued by a Let's Encrypt intermediate, so ISRG anchors
#     the Debian mirror surface (X1 = RSA intermediates, X2 = ECC);
#   * GTS Root R1 / R4  — Google Trust Services chains (other CDN edges).
# NOTE: GlobalSign Root R3 was considered but is no longer part of the
# Mozilla store the source bundle mirrors (removed 2025), so it is absent.
SELECTION = [
    "ISRG Root X1",
    "ISRG Root X2",
    "GTS Root R1",
    "GTS Root R4",
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
                import gzip

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


def subject_cn(der: bytes) -> str:
    """Extract the subject CN (OID 2.5.4.3 = 55 04 03) of a certificate DER."""
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
    for oid, val in walk_name(subject_el):
        if oid == b"\x55\x04\x03":
            return val.decode("latin1")
    raise ValueError("certificate has no subject CN")


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


def main() -> None:
    print(f"downloading {SOURCE_URL} ...")
    data = fetch_source(SOURCE_URL)
    certs = list(pem_blocks(data))
    print(f"source contains {len(certs)} certificates")

    by_cn = {}
    for der in certs:
        try:
            cn = subject_cn(der)
        except Exception as e:  # noqa: BLE001 — skip anything unparsable
            print(f"  skip unparsable cert: {e}", file=sys.stderr)
            continue
        by_cn.setdefault(cn, der)

    missing = [cn for cn in SELECTION if cn not in by_cn]
    if missing:
        raise SystemExit(f"selection not found in source: {missing}")

    # Emit the module: fixed order, fixed formatting — deterministic.
    out = []
    out.append("//! Trust-anchor bundle for the TLS certificate verifier (issue #14).\n")
    out.append("//!\n")
    out.append("//! GENERATED FILE — do not edit by hand; regenerate with\n")
    out.append("//! `tools/gen_ca_bundle.py` (source: the curl/Mozilla CA extract,\n")
    out.append(f"//! `{SOURCE_URL}`), then commit the result. Determinism: no\n")
    out.append("//! timestamps, fixed selection order and formatting — identical input\n")
    out.append("//! produces a byte-identical file.\n")
    out.append("//!\n")
    out.append("//! Each entry is one self-signed root CA: raw DER plus a human label\n")
    out.append("//! (the subject CN) for diagnostics. `net::tls_chain::TrustAnchor`\n")
    out.append("//! pairs the root's subject with its key at verifier start-up.\n")
    out.append("//! Pure `core` — also `#[path]`-included by the host tests (P48).\n")
    out.append("\n#![allow(dead_code)] // consumed by the TlsVerifier in the next PR of the series.\n\n")
    out.append("/// One trust anchor: label (subject CN) + raw DER of the root certificate.\n")
    out.append("pub static CA_BUNDLE: &[(&str, &[u8])] = &[\n")
    for cn in SELECTION:
        der = by_cn[cn]
        fp = hashlib.sha256(der).hexdigest()
        out.append(f"    // sha256 {fp}\n")
        out.append(f"    (\n        \"{cn}\",\n        &[\n{rust_bytes(der, '            ')},\n        ],\n    ),\n")
    out.append("];\n")

    with open(OUT_PATH, "w", newline="\n", encoding="utf-8") as f:
        f.write("".join(out))
    print(f"wrote {OUT_PATH} with {len(SELECTION)} anchors:")
    for cn in SELECTION:
        der = by_cn[cn]
        print(f"  {cn}: {len(der)} bytes, sha256 {hashlib.sha256(der).hexdigest()[:16]}...")


if __name__ == "__main__":
    main()
