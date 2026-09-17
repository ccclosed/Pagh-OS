#!/usr/bin/env python3
"""Minimal OpenPGP *reading* helpers shared by the pagh fixture generators.

Only what the generators need: walk a keyring into primary-key blocks, summarise
the key material, read signature packets and their subpackets, and compute v4
fingerprints. This is deliberately NOT a general OpenPGP library — the kernel's
verifier (`src/pkg/openpgp*.rs`) is the implementation that matters, and the
fixtures generated here are validated against it (and against `gpgv` when
available).

Used by `tools/gen_debian_keyring.py` and `tools/gen_openpgp_fixtures.py`.
Standard library only.
"""

import hashlib

__all__ = [
    "PacketError",
    "read_packets",
    "key_blocks",
    "fingerprint",
    "key_summary",
    "subpackets",
    "signature_info",
    "find_certifications",
    "find_binding",
]


class PacketError(Exception):
    """Malformed or unsupported OpenPGP framing."""


def read_packets(buf):
    """Yield `(offset, tag, body)` for every top-level packet of `buf`.

    Definite lengths only (old and new format). Partial body lengths are refused:
    the generator feeds the tool only GnuPG-exported keyrings and signatures,
    which use definite lengths, and silently mishandling a partial-length stream
    would corrupt the byte ranges we commit.
    """
    i = 0
    while i < len(buf):
        start = i
        ctb = buf[i]
        i += 1
        if ctb & 0x80 == 0:
            raise PacketError(f"no CTB at offset {start}")
        if ctb & 0x40:
            tag = ctb & 0x3F
            first = buf[i]
            i += 1
            if first < 192:
                length = first
            elif first < 224:
                length = ((first - 192) << 8) + buf[i] + 192
                i += 1
            elif first == 255:
                length = int.from_bytes(buf[i : i + 4], "big")
                i += 4
            else:
                raise PacketError(f"partial body length at offset {start}")
        else:
            tag = (ctb >> 2) & 0x0F
            lt = ctb & 0x03
            if lt == 0:
                length = buf[i]
                i += 1
            elif lt == 1:
                length = int.from_bytes(buf[i : i + 2], "big")
                i += 2
            elif lt == 2:
                length = int.from_bytes(buf[i : i + 4], "big")
                i += 4
            else:
                raise PacketError(f"indeterminate length at offset {start}")
        if i + length > len(buf):
            raise PacketError(f"truncated packet at offset {start}")
        yield start, tag, buf[i : i + length]
        i += length


def key_blocks(buf):
    """Yield `(fingerprint_hex, start, end)` for every primary key block.

    A block runs from a tag-6 (public key) packet to the next tag-6 packet, so it
    carries that key's user IDs, certifications, subkeys and binding signatures —
    exactly the byte range the kernel's trust store pins.
    """
    packets = list(read_packets(buf))
    starts = [off for off, tag, _ in packets if tag == 6]
    for idx, start in enumerate(starts):
        end = starts[idx + 1] if idx + 1 < len(starts) else len(buf)
        body = next(body for off, tag, body in packets if off == start)
        yield fingerprint(body).hex().upper(), start, end


def fingerprint(body):
    """OpenPGP v4 fingerprint of a public key / subkey packet body."""
    return hashlib.sha1(b"\x99" + len(body).to_bytes(2, "big") + body).digest()


def _mpis(buf, pos):
    out = []
    while pos < len(buf):
        bits = int.from_bytes(buf[pos : pos + 2], "big")
        pos += 2
        nbytes = (bits + 7) // 8
        out.append((bits, buf[pos : pos + nbytes]))
        pos += nbytes
    return out


def key_summary(body):
    """Summarise a v4 public key/subkey packet body.

    Returns a dict with `version`, `created`, `algo`, `bits`, `fingerprint` (hex)
    and, for the elliptic-curve algorithms, `oid` (hex) and `point`.
    """
    if body[0] != 4:
        raise PacketError(f"unsupported key version {body[0]}")
    created = int.from_bytes(body[1:5], "big")
    algo = body[5]
    rest = body[6:]
    out = {
        "version": 4,
        "created": created,
        "algo": algo,
        "fingerprint": fingerprint(body).hex().upper(),
    }
    if algo in (1, 3):
        mpis = _mpis(rest, 0)
        if len(mpis) != 2:
            raise PacketError("RSA key with != 2 MPIs")
        out["bits"] = mpis[0][0]
        out["n"] = mpis[0][1]
        out["e"] = mpis[1][1]
    elif algo in (19, 22):
        oid_len = rest[0]
        out["oid"] = rest[1 : 1 + oid_len].hex()
        pos = 1 + oid_len
        bits = int.from_bytes(rest[pos : pos + 2], "big")
        pos += 2
        point = rest[pos : pos + (bits + 7) // 8]
        out["point"] = point
        if algo == 22:
            out["bits"] = 255
        else:
            out["bits"] = {"2a8648ce3d030107": 256, "2b81040022": 384, "2b81040023": 521}.get(
                out["oid"], 0
            )
    else:
        raise PacketError(f"unsupported public-key algorithm {algo}")
    return out


def subpackets(area):
    """Yield `(type, data)` for one signature subpacket area."""
    i = 0
    while i < len(area):
        first = area[i]
        i += 1
        if first < 192:
            length = first
        elif first < 255:
            length = ((first - 192) << 8) + area[i] + 192
            i += 1
        else:
            length = int.from_bytes(area[i : i + 4], "big")
            i += 4
        if length == 0 or i + length > len(area):
            raise PacketError("malformed subpacket area")
        yield area[i], area[i + 1 : i + length]
        i += length


def signature_info(body):
    """Summarise a v4 signature packet body."""
    if body[0] != 4:
        raise PacketError(f"unsupported signature version {body[0]}")
    sig_type = body[1]
    pub_algo = body[2]
    hash_algo = body[3]
    hashed_len = int.from_bytes(body[4:6], "big")
    hashed = body[6 : 6 + hashed_len]
    pos = 6 + hashed_len
    unhashed_len = int.from_bytes(body[pos : pos + 2], "big")
    unhashed = body[pos + 2 : pos + 2 + unhashed_len]
    info = {
        "type": sig_type,
        "pub_algo": pub_algo,
        "hash_algo": hash_algo,
        "hashed": hashed,
        "issuer_fpr": None,
        "issuer_keyid": None,
        "created": None,
        "key_expiry": None,
        "key_flags": None,
    }
    for area in (hashed, unhashed):
        for ty, data in subpackets(area):
            if ty == 33 and len(data) == 21 and data[0] == 4 and info["issuer_fpr"] is None:
                info["issuer_fpr"] = data[1:].hex().upper()
            elif ty == 16 and len(data) == 8 and info["issuer_keyid"] is None:
                info["issuer_keyid"] = data.hex().upper()
            elif ty == 2 and len(data) == 4 and info["created"] is None:
                info["created"] = int.from_bytes(data, "big")
            elif ty == 9 and len(data) == 4 and info["key_expiry"] is None:
                info["key_expiry"] = int.from_bytes(data, "big")
            elif ty == 27 and data and info["key_flags"] is None:
                info["key_flags"] = data[0]
    return info


def find_certifications(block):
    """Certifications over a block's primary key, paired with their user IDs."""
    primary = None
    uids = []
    certs = []
    for _off, tag, body in read_packets(block):
        if tag == 6 and primary is None:
            primary = body
        elif tag == 13:
            uids.append(body)
        elif tag == 2:
            info = signature_info(body)
            if 0x10 <= info["type"] <= 0x13:
                certs.append(info)
    return primary, uids, certs


def find_binding(block, subkey_fpr_hex):
    """The binding signature of the subkey with the given fingerprint, or None."""
    current = None
    for _off, tag, body in read_packets(block):
        if tag == 14:
            current = fingerprint(body).hex().upper()
        elif tag == 2 and current is not None and current == subkey_fpr_hex:
            info = signature_info(body)
            if info["type"] == 0x18:
                return info
    return None
