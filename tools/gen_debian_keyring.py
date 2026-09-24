#!/usr/bin/env python3
"""Generate `src/pkg/openpgp_keys.rs` — the kernel's pinned Debian archive keys.

Issue #32, contract §4. The generated file is COMMITTED; re-run this script only
to deliberately refresh it (a Debian key rotation, a new `debian-archive-keyring`
release, or a reaching expiry) and commit the regenerated module after review.

TRUST MODEL — the download is NOT trusted on its own:

  * the source `.deb` is PINNED by its own sha256 (`SOURCE_SHA256` below), so a
    new upstream release cannot silently rewrite the kernel's trust store;
    bumping the version/URL and the pin is one reviewed commit;
  * every key is SELECTED BY ITS v4 FINGERPRINT, computed from the packet bytes;
  * everything the fingerprint does not already cover is cross-checked against
    the `SELECTION` table: the user ID string, the algorithm, the key size, the
    creation time, the expiry and the signing subkeys — a key that is present but
    differs in any of those is a hard error, never a silent pick;
  * a subkey listed in `SELECTION` must exist and must carry a binding signature
    (0x18) issued by the pinned primary.

The generated module pins, per key: the label, the primary fingerprint, the
subkey fingerprints, the validity window, and the exact keyring bytes (the whole
primary-key block, i.e. primary + user IDs + certifications + subkeys + binding
signatures, so a reviewer can pipe it to `gpg --list-packets`). The runtime
re-derives the fingerprints from those bytes (host property P53) and refuses on
any disagreement.

Usage:
    python3 tools/gen_debian_keyring.py                  # download the pinned .deb
    python3 tools/gen_debian_keyring.py --deb FILE       # use a local copy
    python3 tools/gen_debian_keyring.py --check --deb FILE   # diff only, exit 1 on drift
"""

import argparse
import hashlib
import io
import lzma
import os
import subprocess
import sys
import tarfile
import tempfile
import urllib.request

sys.path.insert(0, os.path.dirname(os.path.abspath(__file__)))
import pgp_packets  # noqa: E402  (tool-local helper)

SOURCE_URL = (
    "https://deb.debian.org/debian/pool/main/d/"
    "debian-archive-keyring/debian-archive-keyring_2025.1_all.deb"
)
SOURCE_SHA256 = "9ea7778e443144ca490668737a8ab22dd3e748bb99e805e22ec055abeb3c7fac"
KEYRING_MEMBER = "usr/share/keyrings/debian-archive-keyring.gpg"

OUT_PATH = "src/pkg/openpgp_keys.rs"

# The keys that sign today's `stable` metadata, selected by fingerprint and
# cross-checked field by field. Every value here was established from the source
# keyring and from live `dists/stable` metadata (see OPENPGP-VERIFY-CONTRACT.md
# §12.6). Update only as a deliberate, reviewed act: this changes what pagh
# trusts.
SELECTION = [
    {
        # The release key that signs `stable`/`trixie` directly (Ed25519).
        "label": "Debian Stable Release Key (13/trixie) <debian-release@lists.debian.org>",
        "fpr": "41587F7DB8C774BCCF131416762F67A0B2C39DE4",
        "algo": 22,
        "bits": 255,
        "created": 1742842581,
        "expires": 1995130581,
        "subkeys": [],
    },
    {
        # The current archive signing key; signs through its signing subkey.
        "label": "Debian Archive Automatic Signing Key (13/trixie) <ftpmaster@debian.org>",
        "fpr": "04B54C3CDCA79751B16BC6B5225629DF75B188BD",
        "algo": 1,
        "bits": 4096,
        "created": 1743339029,
        "expires": 2058699029,
        "subkeys": ["B8E5F13176D2A7A75220028078DBA3BC47EF2265"],
    },
    {
        # The previous stable's archive key: Debian cross-signs `stable` with it
        # during the transition, so removing it would break the live mirror.
        "label": "Debian Archive Automatic Signing Key (12/bookworm) <ftpmaster@debian.org>",
        "fpr": "B8B80B5B623EAB6AD8775C45B7C5D7D6350947F8",
        "algo": 1,
        "bits": 4096,
        "created": 1674301461,
        "expires": 1926589461,
        "subkeys": ["4CB50190207B4758A3F73A796ED0E7B82643E131"],
    },
]


def fetch_source(url=SOURCE_URL, retries=3):
    last = None
    for attempt in range(1, retries + 1):
        try:
            req = urllib.request.Request(
                url,
                headers={"User-Agent": "pagh-gen-debian-keyring/1.0", "Accept-Encoding": "identity"},
            )
            with urllib.request.urlopen(req, timeout=60) as resp:
                data = resp.read()
            return data
        except Exception as e:  # noqa: BLE001 — report and retry
            last = e
            print(f"  fetch attempt {attempt}/{retries} failed: {e}", file=sys.stderr)
    raise SystemExit(f"cannot download {url}: {last}")


def ar_members(buf):
    """Yield `(name, data)` for the members of a Unix `ar` archive."""
    if buf[:8] != b"!<arch>\n":
        raise SystemExit("not an ar archive")
    pos = 8
    while pos + 60 <= len(buf):
        header = buf[pos : pos + 60]
        name = header[:16].decode("ascii", "replace").strip().rstrip("/")
        size = int(header[48:58].decode("ascii").strip() or "0")
        data = buf[pos + 60 : pos + 60 + size]
        yield name, data
        pos += 60 + size + (size % 2)


def extract_keyring(deb_bytes):
    for name, data in ar_members(deb_bytes):
        if name == "data.tar.xz":
            with tarfile.open(fileobj=io.BytesIO(lzma.decompress(data))) as tf:
                member = tf.extractfile("./" + KEYRING_MEMBER) or tf.extractfile(KEYRING_MEMBER)
                if member is None:
                    raise SystemExit(f"{KEYRING_MEMBER} not in the .deb")
                return member.read()
    raise SystemExit("data.tar.xz not found in the .deb")


def render_bytes(data, indent):
    """Render a byte string as a Rust array literal body (16 per line)."""
    lines = []
    for i in range(0, len(data), 16):
        chunk = ", ".join(f"0x{b:02x}" for b in data[i : i + 16])
        lines.append(indent + chunk + ",")
    return "\n".join(lines)


def rust_string(s):
    return '"' + s.replace("\\", "\\\\").replace('"', '\\"') + '"'


def rust_fpr(fpr_hex):
    raw = bytes.fromhex(fpr_hex)
    assert len(raw) == 20, fpr_hex
    return "[" + ", ".join(f"0x{b:02x}" for b in raw) + "]"


def select_keys(keyring):
    """Select and cross-check every `SELECTION` entry. Returns [(entry, block)]."""
    blocks = {fpr: (start, end) for fpr, start, end in pgp_packets.key_blocks(keyring)}
    out = []
    for entry in SELECTION:
        fpr = entry["fpr"]
        if fpr not in blocks:
            raise SystemExit(
                f"PINNED KEY MISSING: {fpr} ({entry['label']}) is not in the source keyring. "
                "If Debian rotated a key, update SELECTION as a reviewed act — this changes "
                "what pagh trusts."
            )
        start, end = blocks[fpr]
        block = keyring[start:end]
        packets = list(pgp_packets.read_packets(block))
        primary_body = packets[0][2]
        summary = pgp_packets.key_summary(primary_body)

        problems = []
        if summary["algo"] != entry["algo"]:
            problems.append(f"algo {summary['algo']} != pinned {entry['algo']}")
        if summary["bits"] != entry["bits"]:
            problems.append(f"bits {summary['bits']} != pinned {entry['bits']}")
        if summary["created"] != entry["created"]:
            problems.append(f"created {summary['created']} != pinned {entry['created']}")

        # User ID cross-check (the label must be a UID the key actually carries).
        uids = [b.decode("utf-8", "replace") for _o, tag, b in packets if tag == 13]
        if entry["label"] not in uids:
            problems.append(f"label not among the key's user IDs: {uids}")

        # Expiry from the primary's own certifications (min over self-signatures,
        # which is what the verifier computes).
        _p, _u, certs = pgp_packets.find_certifications(block)
        self_certs = [c for c in certs if (c["issuer_fpr"] or "").upper() == fpr]
        expiries = [
            summary["created"] + c["key_expiry"]
            for c in self_certs
            if c["key_expiry"]
        ]
        expires = min(expiries) if expiries else None
        if expires != entry["expires"]:
            problems.append(f"expires {expires} != pinned {entry['expires']}")

        # Subkeys: every pinned subkey must exist with a binding signature.
        found_subkeys = []
        for _o, tag, body in packets:
            if tag == 14:
                found_subkeys.append(pgp_packets.fingerprint(body).hex().upper())
        for want in entry["subkeys"]:
            if want not in found_subkeys:
                problems.append(f"subkey {want} missing")
            elif pgp_packets.find_binding(block, want) is None:
                problems.append(f"subkey {want} has no binding signature")

        if problems:
            raise SystemExit(
                f"PIN MISMATCH for {fpr} ({entry['label']}):\n  - " + "\n  - ".join(problems)
            )
        out.append((entry, block, found_subkeys))
    return out


def render_module(selected):
    out = []
    out.append(
        "//! Pinned Debian archive keys for the OpenPGP verifier (issue #32).\n"
        "//!\n"
        "//! GENERATED FILE — do not edit by hand; regenerate with\n"
        "//! `python3 tools/gen_debian_keyring.py` (add `--deb FILE` to use a local copy of\n"
        "//! the pinned `debian-archive-keyring` package, which is what offline runs do)\n"
        "//! and commit the result. The generator selects every key BY ITS v4\n"
        "//! FINGERPRINT, cross-checks label/algorithm/size/creation/expiry/subkeys\n"
        "//! against its `SELECTION` table and fails closed on any deviation.\n"
        "//!\n"
        f"//! Source: {SOURCE_URL}\n"
        f"//! sha256: {SOURCE_SHA256}\n"
        "//!\n"
        "//! Each `block` is the exact keyring byte range for one primary key\n"
        "//! (primary + user IDs + certifications + subkeys + binding signatures), so\n"
        "//! it can be inspected with `gpg --list-packets`. The runtime re-derives the\n"
        "//! fingerprints and the validity window from these bytes and refuses on any\n"
        "//! disagreement (host property P53); trust never rests on a value parsed out\n"
        "//! of the block alone.\n"
        "\n"
        "#![allow(dead_code)] // consumed by the apt trust chain in the next PR of the series.\n"
        "\n"
        "use super::openpgp::PinnedKey;\n"
    )
    out.append(
        "\n/// The Debian archive keys pagh trusts for repository metadata.\n"
        "///\n"
        "/// Adding or refreshing a key is a deliberate act: run\n"
        "/// `tools/gen_debian_keyring.py`, review the diff and commit it. The kernel\n"
        "/// never fetches or updates keys at runtime.\n"
        "pub static DEBIAN_KEYRING: &[PinnedKey] = &[\n"
    )
    for entry, block, subkeys in selected:
        out.append("    PinnedKey {\n")
        out.append(f"        label: {rust_string(entry['label'])},\n")
        out.append(f"        fingerprint: {rust_fpr(entry['fpr'])},\n")
        out.append(f"        algo: {entry['algo']},\n")
        out.append(f"        bits: {entry['bits']},\n")
        out.append(f"        created: {entry['created']},\n")
        if entry["expires"] is None:
            out.append("        expires: None,\n")
        else:
            out.append(f"        expires: Some({entry['expires']}),\n")
        out.append("        block: &[\n")
        out.append(render_bytes(block, "            "))
        out.append("\n        ],\n")
        if subkeys:
            out.append("        subkeys: &[\n")
            for sub in subkeys:
                out.append(f"            {rust_fpr(sub)},\n")
            out.append("        ],\n")
        else:
            out.append("        subkeys: &[],\n")
        out.append("    },\n")
    out.append("];\n")
    return "".join(out)


def main():
    ap = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    ap.add_argument("--deb", help="local debian-archive-keyring .deb (skips the download)")
    ap.add_argument("--check", action="store_true", help="exit 1 if the committed file differs")
    args = ap.parse_args()

    if args.deb:
        with open(args.deb, "rb") as f:
            deb = f.read()
        origin = args.deb
    else:
        print(f"downloading {SOURCE_URL} ...")
        deb = fetch_source()
        origin = SOURCE_URL
    got = hashlib.sha256(deb).hexdigest()
    if got != SOURCE_SHA256:
        raise SystemExit(
            f"SOURCE SHA256 MISMATCH for {origin}:\n  expected {SOURCE_SHA256}\n  got      {got}\n"
            "If the upstream package legitimately changed, update SOURCE_URL and SOURCE_SHA256 "
            "as a reviewed act — this changes what pagh trusts."
        )

    keyring = extract_keyring(deb)
    print(f"source ok (sha256 {got[:16]}…), keyring {len(keyring)} bytes")
    selected = select_keys(keyring)
    for entry, block, subkeys in selected:
        print(
            f"  {entry['label']}\n"
            f"    fpr {entry['fpr']} algo {entry['algo']} bits {entry['bits']} "
            f"created {entry['created']} expires {entry['expires']} "
            f"subkeys {subkeys or '-'} block {len(block)} B"
        )

    rendered = render_module(selected)

    # The committed artifact is the RUSTFMT-CANONICAL form: run the same
    # formatter `cargo fmt --all` would apply so a plain regeneration is
    # byte-identical to what is committed.
    with tempfile.TemporaryDirectory() as tmp:
        raw = os.path.join(tmp, "openpgp_keys.rs")
        with open(raw, "w", newline="\n", encoding="utf-8") as f:
            f.write(rendered)
        subprocess.run(["rustfmt", "--edition", "2021", raw], check=True)
        with open(raw, encoding="utf-8") as f:
            rendered = f.read()

    if args.check:
        try:
            with open(OUT_PATH, encoding="utf-8") as f:
                committed = f.read()
        except FileNotFoundError:
            raise SystemExit(f"{OUT_PATH} does not exist")
        if committed != rendered:
            raise SystemExit(
                f"{OUT_PATH} differs from a fresh generation — review and commit the regenerated file"
            )
        print(f"{OUT_PATH} is up to date")
        return

    with open(OUT_PATH, "w", newline="\n", encoding="utf-8") as f:
        f.write(rendered)
    print(f"wrote {OUT_PATH} ({len(rendered)} bytes, {len(selected)} keys)")


if __name__ == "__main__":
    main()
