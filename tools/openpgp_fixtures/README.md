# Adversarial OpenPGP fixtures for issue #32 (verification only)

**This branch is a verification artifact and is never merged.** It carries the attack
fixtures for the `apt` trust chain (issue #32), the generator that rebuilds them
deterministically, and the manifest that tells the e2e harness what each case must produce.

## Why the fixtures look the way they do

`src/pkg/apt.rs::trusted_keyring()` is `&DEBIAN_KEYRING`: three pinned Debian archive keys,
**no test anchor**. A locally generated key therefore only ever reaches the *signature*
stage (`cause=NoTrustedSignature`). That splits the attacks into two families:

* **`a*` — real Debian trust root.** The two cases issue #32 actually demands ("a tampered
  `Packages` is refused", "a tampered `.deb` is refused") require the signature to be
  **accepted** first, and the only keys the kernel accepts are Debian's. These trees are
  therefore built from **real, hash-pinned Debian metadata** with only the *unsigned* parts
  (the index, the `.deb`, one signature packet) tampered with.
* **`b*` — locally signed with committed fixture keys.** Refusals that live at the
  signature / armor / clearsign stages. The signatures are *valid* GnuPG signatures by keys
  outside the pinned set, which is exactly the "valid signature, wrong signer" attack.

The distinction matters: without it one could "test" the index binding with a self-signed
repository and get a `NoTrustedSignature` refusal that never touches the binding code — a
green test that proves nothing.

## Usage

```sh
python3 tools/openpgp_attack_fixtures.py build      # -> .cache/openpgp_cases/ + manifest.json
python3 tools/openpgp_attack_fixtures.py check      # gpgv cross-check of the fixtures
python3 tools/openpgp_attack_fixtures.py list       # the case table
```

`build` needs no network: the real Debian inputs and the fixture keys are committed (each
pinned by SHA-256 in `tools/openpgp_fixtures/…/inventory.json`). The generated trees live
under `.cache/openpgp_cases/` (git-ignored).

`check` is the anti-tautology net for the *fixtures themselves*: it verifies with GnuPG
(the reference implementation) that the `b*` fixtures really are valid signatures, that
`b03` really is a broken armor, and that the `a*` reference trees verify under the shipped
Debian keyring. Its output is part of the evidence for t24.

## Case table

| id | family | what is served | expected | marker |
|---|---|---|---|---|
| `a01-reference-inrelease` | real | unmodified `stable` `InRelease` + real `contrib` index + real `.deb` | **accept** | `apt: verify OK release=InRelease` |
| `a02-reference-detached` | real | same, `InRelease` removed (real `Release.gpg` + `Release`) | **accept** | `apt: verify OK release=Release.gpg` |
| `a03-index-hash-mismatch` | real | real signed `Release`, one byte flipped in `Packages.xz` (same size) | reject | `apt: verify FAIL stage=index cause=HashMismatch` |
| `a03b-index-size-mismatch` | real | index truncated by one byte | reject | `… stage=index cause=SizeMismatch` |
| `a04-deb-hash-mismatch` | real | real signed index, one byte flipped in the `.deb` (same size) | reject | `apt: verify FAIL stage=deb cause=HashMismatch` |
| `a05-deb-size-mismatch` | real | `.deb` with one extra byte | reject | `… stage=deb cause=SizeMismatch` |
| `a06-signature-bad-no-fallback` | real | real `InRelease` with one signature packet corrupted (armor CRC recomputed) **plus a valid `Release.gpg`** | reject | `apt: verify FAIL stage=signature cause=BadSignature` |
| `a07-unsigned` | real | metadata with no signature at all | reject | `apt: verify FAIL stage=metadata cause=Unsigned` |
| `a08-rollback-old-stable` | real | a complete, internally consistent **older** `stable` triplet (snapshot.debian.org) | **accept** | `apt: verify OK release=InRelease` |
| `b01-untrusted-signer` | local | well-formed `Release` signed by a key outside the pinned set | reject | `apt: verify FAIL stage=signature cause=NoTrustedSignature` |
| `b02-no-signature-packets` | local | valid armor carrying a marker packet, no signature packet | reject | `… stage=signature cause=NoSignature` |
| `b03-armor-crc-mismatch` | local | armor with a corrupted body and stale CRC24 | reject | `… stage=armor cause=CrcMismatch` |
| `b04-clearsign-malformed` | local | `InRelease` without its `END PGP SIGNATURE` line | reject | `… stage=clearsign cause=Malformed` |
| `b05-expired-untrusted-signer` | local | signature made by an expired key (valid when signed) | reject | `… stage=signature cause=NoTrustedSignature` |

`a01`/`a02` are mandatory: without a case that must be **accepted**, "refuse everything"
would also pass the negative set. `a08` is an *expected accept* — see below.

## The rollback answer (asked for explicitly)

**The replay of an old, valid `stable` triplet is NOT detected: the attack passes.**
`stable` carries no `Valid-Until` (contract §0, re-verified against the live mirror), and the
kernel persists no "highest `Release` seen", so `a08` must be *accepted* after a successful
update from `a01`. The case exists so the residual is visible in an actual run instead of
only in prose. This is an inherited property of the Debian `stable` suite, not a defect of
the implementation — but it must be written down where users read it. Proposed wording for
`SECURITY.md` (to land with the #32 PR, whose §9 already rewrites that section):

> **Repository rollback is not detected.** The signature chain proves that the metadata came
> from Debian and that it was not modified in transit or on the mirror; it does not prove
> that the metadata is the *newest* one. `stable` carries no `Valid-Until`, and pagh keeps no
> record of the highest `Release` it has seen, so a mirror that serves a complete older
> triplet (`Release` + `Packages` + `.deb`) is accepted. `Valid-Until` **is** enforced
> whenever a suite carries it, and a signature or `Date` far in the future is refused.

## What cannot be tested end-to-end, and why

These failures need a **pinned** key to be expired/revoked/future-dated, or a signature by a
pinned key whose signed bytes we cannot produce — none of which we can create without
Debian's private keys. They stay with the host properties; the manifest lists them under
`not_constructible` with the property that covers each:

`key/Expired`, `key/Revoked`, `key/NotYetValid`, `signature/FutureSignature`,
`release/FutureDate`, `release/ValidUntilExpired`, `index/NoIndexEntry`.

One more is reachable but needs a harness flag: **`clock/ClockUnset`** requires the *guest*
clock below 2025-01-01, i.e. QEMU `-rtc base=2020-01-01T00:00:00`; serve the `a01` tree with
that flag and expect `stage=clock cause=ClockUnset`. The manifest records the same.

## Interface for the t23 harness

* Every case directory is a **complete apt repository root**: `dists/stable/…` + `pool/…`.
  Serve `<case>/` at the mirror root (the layout matches what `apt` fetches) and run
  `apt update` / `apt install` against it.
* `manifest.json` carries, per case: `id`, `family`, `expect` (`accept`/`reject`), `stage`,
  `cause`, `marker`, `tree`, `attacks`, `notes`; plus `reference_signers` and
  `not_constructible`.
* The package under test is the smallest real one in the pinned index:
  `hashcat-nvidia` (`pool/contrib/h/hashcat-meta/hashcat-nvidia_20210201_all.deb`).
* `a01`/`a02`/`a08` need a real `apt update`; `a04`/`a05` additionally need
  `apt install hashcat-nvidia` so the `.deb` is fetched at all.

## Reference verification recorded in the manifest

`reference_signers` (produced by `gpgv` over `a01`) shows the current `stable` `InRelease`
is signed by

* `41587F7DB8C774BCCF131416762F67A0B2C39DE4` — Debian Stable Release Key (13/trixie), a
  **pinned primary**, and
* `B8E5F13176D2A7A75220028078DBA3BC47EF2265` — the RSA signing **subkey** of the pinned
  primary `04B54C3CDCA79751B16BC6B5225629DF75B188BD`.

So the positive controls also exercise the contract's subkey-to-pinned-primary mapping
(§3.5) end to end. A verifier comparing raw `gpgv` output against the pinned *primary*
fingerprints will see "not pinned" for the first signature — that is the subkey, not an
untracked key (this check was performed here after that exact false alarm).

## Inputs (committed, hash-pinned)

| path | what |
|---|---|
| `tools/openpgp_fixtures/real/` | current `stable` `Release`/`InRelease`/`Release.gpg`, `contrib/binary-amd64/Packages.xz`, the smallest `.deb` from that index, the Debian archive keyring (for the reference checks) — `inventory.json` pins every SHA-256 |
| `tools/openpgp_fixtures/real_old/` | the same set from `snapshot.debian.org` (`20250901T000000Z`) for the rollback case |
| `tools/openpgp_fixtures/keys/` | three fixture keys (release / expired / future) with their secret material, so a *valid* signature by an unpinned key can be produced; created with `gpg 2.4.9` under `--faked-system-time` |
