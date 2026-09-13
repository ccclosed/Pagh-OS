// Shared DER certificate-building helpers for the TLS server-authentication
// properties (P47+). Moved out of P47 so later properties of the issue #14
// series reuse ONE builder instead of drifting copies (same policy as
// `det_rng`).
//
// The certificates are hand-encoded DER of exactly the shapes
// `x509::parse_certificate` accepts, signed with the ISSUER's key, so the
// properties exercise the same code path a real Debian mirror chain takes.

#![cfg(test)]

use signature::Signer;

/// Encode one DER TLV with a minimal-length definite length.
pub fn tlv(tag: u8, content: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    out.push(tag);
    let l = content.len();
    if l < 0x80 {
        out.push(l as u8);
    } else {
        let bytes = l.to_be_bytes();
        let skip = bytes.iter().take_while(|&&b| b == 0).count();
        out.push(0x80 | (8 - skip) as u8);
        out.extend_from_slice(&bytes[skip..]);
    }
    out.extend_from_slice(content);
    out
}

/// A `Name` with a single CN RDN (byte-compared by the chain builder, so the
/// EXACT same encoding must be reused as a child's issuer and a parent's
/// subject — this helper guarantees that by construction).
pub fn name(cn: &[u8]) -> Vec<u8> {
    // Name ::= SEQUENCE { SET { SEQUENCE { OID 2.5.4.3, UTF8String cn } } }
    let rdn_set = tlv(
        0x31,
        &tlv(
            0x30,
            &[
                tlv(0x06, &[0x55, 0x04, 0x03]).as_slice(),
                tlv(0x0c, cn).as_slice(),
            ]
            .concat(),
        ),
    );
    tlv(0x30, &rdn_set)
}

/// ECDSA-with-SHA256 `AlgorithmIdentifier` element (no parameters).
pub fn ecdsa_sig_alg() -> Vec<u8> {
    tlv(
        0x30,
        &tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x04, 0x03, 0x02]),
    )
}

/// P-256 SPKI element from an uncompressed 65-byte point.
pub fn spki_p256(point: &[u8]) -> Vec<u8> {
    let mut bits = vec![0x00u8];
    bits.extend_from_slice(point);
    tlv(
        0x30,
        &[
            tlv(
                0x30,
                &[
                    tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x02, 0x01]).as_slice(),
                    tlv(0x06, &[0x2a, 0x86, 0x48, 0xce, 0x3d, 0x03, 0x01, 0x07]).as_slice(),
                ]
                .concat(),
            )
            .as_slice(),
            tlv(0x03, &bits).as_slice(),
        ]
        .concat(),
    )
}

/// `Validity ::= SEQUENCE { UTCTime, UTCTime }` (RFC 5280 shapes).
pub fn validity(not_before: &[u8], not_after: &[u8]) -> Vec<u8> {
    tlv(
        0x30,
        &[
            tlv(0x17, not_before).as_slice(),
            tlv(0x17, not_after).as_slice(),
        ]
        .concat(),
    )
}

/// `Extension ::= SEQUENCE { OID, critical BOOLEAN?, OCTET STRING }`.
pub fn extension(oid: &[u8], critical: bool, value_der: &[u8]) -> Vec<u8> {
    let mut fields = vec![tlv(0x06, oid)];
    if critical {
        fields.push(tlv(0x01, &[0xff]));
    }
    fields.push(tlv(0x04, value_der));
    tlv(0x30, &fields.concat())
}

/// basicConstraints with `cA` as given (a CA cert MUST carry it explicitly).
pub fn basic_constraints(ca: bool) -> Vec<u8> {
    if ca {
        tlv(0x30, &tlv(0x01, &[0xff]))
    } else {
        tlv(0x30, &tlv(0x01, &[0x00]))
    }
}

/// subjectAltName with one dNSName entry.
pub fn san_dns(host: &[u8]) -> Vec<u8> {
    tlv(0x30, &tlv(0x82, host))
}

/// subjectAltName with a left-most-wildcard dNSName entry.
pub fn san_wildcard(pattern: &[u8]) -> Vec<u8> {
    san_dns(pattern) // same [2] dNSName encoding; `*` is plain label content
}

/// subjectAltName with one iPAddress (IPv4) entry (raw 4 octets).
pub fn san_ip(octets: &[u8; 4]) -> Vec<u8> {
    tlv(0x30, &tlv(0x87, octets))
}

/// OID 2.5.4.3-wrapped extension tags used by the chain properties.
pub const OID_BASIC_CONSTRAINTS: &[u8] = &[0x55, 0x1d, 0x13];
pub const OID_SAN: &[u8] = &[0x55, 0x1d, 0x11];

/// Build the TBSCertificate for one chain level and return the complete
/// signed certificate DER. `subject`/`issuer` are pre-encoded `Name` DERs;
/// `exts` are the raw Extension elements (empty → no extensions field);
/// `signer` is the ISSUER's key.
pub fn build_cert(
    serial: u8,
    subject: &[u8],
    issuer: &[u8],
    point: &[u8],
    not_before: &[u8],
    not_after: &[u8],
    exts: &[Vec<u8>],
    signer: &p256::ecdsa::SigningKey,
) -> Vec<u8> {
    let sig_alg = ecdsa_sig_alg();
    let mut fields = vec![
        // [0] EXPLICIT version v3.
        tlv(0xa0, &tlv(0x02, &[0x02])),
        tlv(0x02, &[serial]),
        sig_alg.clone(),
        issuer.to_vec(),
        validity(not_before, not_after),
        subject.to_vec(),
        spki_p256(point),
    ];
    if !exts.is_empty() {
        let ext_seq: Vec<u8> = exts.concat();
        fields.push(tlv(0xa3, &tlv(0x30, &ext_seq)));
    }
    let tbs = tlv(0x30, &fields.concat());
    // ECDSA signature over the EXACT TBS element bytes, DER-encoded r,s.
    let sig: p256::ecdsa::DerSignature = signer.sign(&tbs);
    // BIT STRING content: 0x00 unused-bits byte || raw DER signature blob.
    let mut sig_bits = vec![0x00u8];
    sig_bits.extend_from_slice(sig.as_ref());
    tlv(
        0x30,
        &[
            tbs.as_slice(),
            sig_alg.as_slice(),
            tlv(0x03, &sig_bits).as_slice(),
        ]
        .concat(),
    )
}

/// "Now" comfortably inside every default validity window (2025-06).
pub const NOW: i64 = 1_750_000_000;
/// Default validity: 2025-01-01 .. 2035-01-01 (UTCTime).
pub const NB: &[u8] = b"250101000000Z";
pub const NA: &[u8] = b"350101000000Z";
