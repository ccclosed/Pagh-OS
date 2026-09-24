#!/usr/bin/env python3
"""Deterministic OpenPGP *signing* for the apt test fixtures (issue #32).

The kernel's verifier is the reviewed implementation; this tool is the producer
side for the E2E fixtures, and it exists so that `tools/mini_repo.py` can serve a
**signed** repository without a committed secret key, without `gpg` on the host
and without randomness (an E2E run must be reproducible byte for byte).

WHY HAND-ROLLED CRYPTO IS ACCEPTABLE HERE — and only here: the kernel's rule
("do not approximate cryptography") is about what *pagh* trusts. This file never
runs in the kernel; it produces test data whose acceptance is decided by the
Rust verifier (`pkg::openpgp`) and, at generation time, is cross-checked against
the reference implementation (`gpgv`) and against the reviewed `ed25519-dalek`
crate in the host tests. The Ed25519 implementation below is the RFC 8032
reference algorithm (extended coordinates, ~40 lines), pinned by RFC 8032 test
vectors in `self_test()`.

Only what the fixtures need is implemented:

  * Ed25519 signing from a fixed 32-octet seed (RFC 8032 §5.1.6);
  * OpenPGP v4 packets: public key (algo 22, the LEGACY GnuPG Ed25519 OID
    1.3.6.1.4.1.11591.15.1 that Debian's own keys carry), user ID, and v4
    signatures of type 0x13 (positive UID certification), 0x00 (binary
    document) and 0x01 (canonical text document);
  * the hashing rules the verifier expects, so a fixture cannot be "signed
    wrongly" without the kernel noticing: `H = SHA256(data || hashed_portion ||
    0x04 0xFF || be32(6 + hashed_len))`, with the key hashed as
    `0x99 || be16(len) || body` and a user ID as `0xB4 || be32(len) || uid`;
  * ASCII armor with the CRC24 trailer.
"""

import base64
import hashlib
import struct

# ── Ed25519 (RFC 8032 §5.1.6, reference algorithm) ───────────────────────────
P = 2**255 - 19
L = 2**252 + 27742317777372353535851937790883648493
D = (-121665 * pow(121666, P - 2, P)) % P
I = pow(2, (P - 1) // 4, P)
BY = (4 * pow(5, P - 2, P)) % P


def _sha512(data: bytes) -> bytes:
    return hashlib.sha512(data).digest()


def _recover_x(y: int, sign: int) -> int:
    xx = (y * y - 1) * pow(D * y * y + 1, P - 2, P)
    x = pow(xx, (P + 3) // 8, P)
    if (x * x - xx) % P != 0:
        x = (x * I) % P
    if (x * x - xx) % P != 0:
        raise ValueError("not a square")
    if x % 2 != sign:
        x = P - x
    return x


def _edwards(p1, p2):
    x1, y1 = p1
    x2, y2 = p2
    prod = D * x1 * x2 * y1 * y2
    x3 = (x1 * y2 + x2 * y1) * pow(1 + prod, P - 2, P)
    y3 = (y1 * y2 + x1 * x2) * pow(1 - prod, P - 2, P)
    return (x3 % P, y3 % P)


def _scalarmult(point, scalar):
    result = (0, 1)
    addend = point
    while scalar:
        if scalar & 1:
            result = _edwards(result, addend)
        addend = _edwards(addend, addend)
        scalar >>= 1
    return result


BASE = (_recover_x(BY, 0), BY)


def _encode_point(point) -> bytes:
    x, y = point
    bits = [(y >> i) & 1 for i in range(255)] + [x & 1]
    return bytes(sum(bits[i * 8 + j] << j for j in range(8)) for i in range(32))


def _secret_scalar(seed: bytes) -> int:
    h = _sha512(seed)
    return 2**254 + sum(2**i * ((h[i // 8] >> (i % 8)) & 1) for i in range(3, 254))


def public_key(seed: bytes) -> bytes:
    """The 32-octet Ed25519 public key for a seed."""
    return _encode_point(_scalarmult(BASE, _secret_scalar(seed)))


def sign(seed: bytes, message: bytes) -> bytes:
    """The 64-octet Ed25519 signature (R || S) over `message`."""
    h = _sha512(seed)
    a = _secret_scalar(seed)
    public = _encode_point(_scalarmult(BASE, a))
    r = int.from_bytes(_sha512(h[32:] + message), "little") % L
    big_r = _scalarmult(BASE, r)
    k = int.from_bytes(_sha512(_encode_point(big_r) + public + message), "little") % L
    s = (r + k * a) % L
    return _encode_point(big_r) + s.to_bytes(32, "little")


def self_test() -> None:
    """RFC 8032 §7.1 test vectors — a wrong signer must not silently sign."""
    seed = bytes.fromhex("9d61b19deffd5a60ba844af492ec2cc44449c5697b326919703bac031cae7f60")
    expected_pub = bytes.fromhex("d75a980182b10ab7d54bfed3c964073a0ee172f3daa62325af021a68f707511a")
    expected_sig = bytes.fromhex(
        "e5564300c360ac729086e2cc806e828a84877f1eb8e5d974d873e06522490155"
        "5fb8821590a33bacc61e39701cf9b46bd25bf5f0595bbe24655141438e7a100b"
    )
    assert public_key(seed) == expected_pub, "RFC 8032 vector 1: public key"
    assert sign(seed, b"") == expected_sig, "RFC 8032 vector 1: signature"

    seed = bytes.fromhex("4ccd089b28ff96da9db6c346ec114e0f5b8a319f35aba624da8cf6ed4fb8a6fb")
    expected_pub = bytes.fromhex("3d4017c3e843895a92b70aa74d1b7ebc9c982ccf2ec4968cc0cd55f12af4660c")
    expected_sig = bytes.fromhex(
        "92a009a9f0d4cab8720e820b5f642540a2b27b5416503f8fb3762223ebdb69da"
        "085ac1e43e15996e458f3613d0f11d8c387b2eaeb4302aeeb00d291612bb0c00"
    )
    assert public_key(seed) == expected_pub, "RFC 8032 vector 2: public key"
    assert sign(seed, b"\x72") == expected_sig, "RFC 8032 vector 2: signature"


# ── OpenPGP packets ──────────────────────────────────────────────────────────

# The legacy GnuPG Ed25519 OID (1.3.6.1.4.1.11591.15.1): the form Debian's own
# Ed25519 keys use, and the one the kernel's parser must keep accepting.
OID_ED25519_LEGACY = bytes.fromhex("2b06010401da470f01")


def packet(tag: int, body: bytes) -> bytes:
    """A new-format packet with a definite length (1, 2 or 5 length octets)."""
    n = len(body)
    if n < 192:
        header = bytes([0xC0 | tag, n])
    elif n < 8384:
        n -= 192
        header = bytes([0xC0 | tag, 192 + (n >> 8), n & 0xFF])
    else:
        header = bytes([0xC0 | tag, 0xFF]) + struct.pack(">I", n)
    return header + body


def mpi(value: int) -> bytes:
    """An OpenPGP MPI: two-octet bit length plus the magnitude (no zero pad)."""
    if value == 0:
        return b"\x00\x00"
    raw = value.to_bytes((value.bit_length() + 7) // 8, "big")
    return struct.pack(">H", value.bit_length()) + raw


def mpi_bytes(raw: bytes) -> bytes:
    """An MPI whose octets are given directly (leading zeros dropped)."""
    trimmed = raw.lstrip(b"\x00")
    return mpi(int.from_bytes(trimmed, "big") if trimmed else 0)


def fingerprint(body: bytes) -> bytes:
    """OpenPGP v4 fingerprint of a key packet body."""
    return hashlib.sha1(b"\x99" + struct.pack(">H", len(body)) + body).digest()


def key_hashed(body: bytes) -> bytes:
    """How GnuPG hashes a key for key signatures / certifications."""
    return b"\x99" + struct.pack(">H", len(body)) + body


def uid_hashed(uid: bytes) -> bytes:
    """How GnuPG hashes a user ID for a certification."""
    return b"\xB4" + struct.pack(">I", len(uid)) + uid


def public_key_packet(seed: bytes, created: int, algo: int = 22) -> bytes:
    """A v4 primary public-key packet body (EdDSA + the legacy OID)."""
    point = b"\x40" + public_key(seed)
    oid = bytes([len(OID_ED25519_LEGACY)]) + OID_ED25519_LEGACY
    return bytes([4]) + struct.pack(">I", created) + bytes([algo]) + oid + mpi_bytes(point)


def subpacket(ty: int, data: bytes) -> bytes:
    if len(data) + 1 < 192:
        return bytes([len(data) + 1, ty]) + data
    if len(data) + 1 < 8384:
        n = len(data) + 1 - 192
        return bytes([192 + (n >> 8), n & 0xFF, ty]) + data
    return bytes([255]) + struct.pack(">I", len(data) + 1) + bytes([ty]) + data


def signature_packet(
    seed: bytes,
    key_body: bytes,
    signed_data: bytes,
    sig_type: int,
    created: int,
    hash_algo: int = 8,
    key_expiry: int | None = None,
) -> bytes:
    """A v4 signature packet over `signed_data` (already key/uid/body parts).

    `signed_data` is everything GnuPG hashes before the signature's own hashed
    portion: the document for a data signature, `key_hashed(key)` for a direct
    key signature, `key_hashed(key) + uid_hashed(uid)` for a certification.
    """
    hashed = (
        subpacket(33, bytes([4]) + fingerprint(key_body))
        + subpacket(2, struct.pack(">I", created))
        + subpacket(27, bytes([0x03]))  # key flags: certify + sign
    )
    if key_expiry is not None:
        # Subpacket 9: seconds after the key's creation time. Only fixture keys
        # meant to exercise the expiry policy carry it.
        hashed += subpacket(9, struct.pack(">I", key_expiry))
    hashed_portion = (
        bytes([4, sig_type, 22, hash_algo]) + struct.pack(">H", len(hashed)) + hashed
    )
    digest = hashlib.sha256(
        signed_data + hashed_portion + b"\x04\xFF" + struct.pack(">I", len(hashed_portion))
    ).digest()
    signature = sign(seed, digest)
    r = int.from_bytes(signature[:32], "big")
    s = int.from_bytes(signature[32:], "big")
    unhashed = subpacket(16, fingerprint(key_body)[-8:])
    body = (
        bytes([4, sig_type, 22, hash_algo])
        + struct.pack(">H", len(hashed))
        + hashed
        + struct.pack(">H", len(unhashed))
        + unhashed
        + digest[:2]
        + mpi(r)
        + mpi(s)
    )
    return packet(2, body)


def keyring_block(
    seed: bytes, uid: bytes, created: int, key_expiry: int | None = None
) -> bytes:
    """A complete one-key keyring: primary key, user ID, self-certification.

    `key_expiry` (seconds after `created`) is written into the self-signature's
    subpacket 9, which is where the verifier reads a primary key's expiry from.
    """
    primary = public_key_packet(seed, created)
    cert = signature_packet(
        seed,
        primary,
        key_hashed(primary) + uid_hashed(uid),
        0x13,
        created,
        key_expiry=key_expiry,
    )
    return packet(6, primary) + packet(13, uid) + cert


# ── ASCII armor ──────────────────────────────────────────────────────────────

def crc24(data: bytes) -> bytes:
    crc = 0x00B704CE
    for byte in data:
        crc ^= byte << 16
        for _ in range(8):
            crc <<= 1
            if crc & 0x01000000:
                crc ^= 0x01864CFB
    crc &= 0x00FFFFFF
    return crc.to_bytes(3, "big")


def armor(data: bytes, kind: str = "SIGNATURE") -> bytes:
    """ASCII-armor `data` with the CRC24 trailer (RFC 9580 §6)."""
    b64 = base64.b64encode(data).decode()
    lines = [b64[i : i + 64] for i in range(0, len(b64), 64)]
    crc = base64.b64encode(crc24(data)).decode()
    return (
        f"-----BEGIN PGP {kind}-----\n\n"
        + "\n".join(lines)
        + f"\n={crc}\n-----END PGP {kind}-----\n"
    ).encode()


def canonicalize(text: bytes) -> bytes:
    """The byte string a clear-signed signature is computed over.

    Mirrors the verifier's rule exactly (and GnuPG's): unescape `- ` line
    prefixes, strip trailing spaces/tabs/CR from every line, join with CRLF and
    emit **no** line ending after the last line. Signing the raw text instead is
    the classic clearsign bug — `gpgv` refuses the result.
    """
    lines = []
    for raw in text.split(b"\n"):
        line = raw[2:] if raw.startswith(b"- ") else raw
        lines.append(line.rstrip(b" \t\r"))
    if lines and lines[-1] == b"":
        lines.pop()  # the line ending before the signature block is not signed
    return b"\r\n".join(lines)


def cleartext_signed(text: bytes, signature: bytes) -> bytes:
    """A clear-signed message whose canonicalization signs `text` exactly.

    The verifier signs `dash-unescall + strip trailing whitespace + CRLF, no
    final EOL`; `text` here is a `Release` body without trailing whitespace, so
    the armored body can be the text itself, followed by the mandatory line
    ending before the signature block.
    """
    assert not any(line != line.rstrip() for line in text.decode().split("\n")), (
        "fixture text must not have trailing whitespace: the signer would strip it"
    )
    body = text
    if not body.endswith(b"\n"):
        body += b"\n"
    return (
        b"-----BEGIN PGP SIGNED MESSAGE-----\nHash: SHA256\n\n"
        + body
        + armor(signature, "SIGNATURE")
    )
