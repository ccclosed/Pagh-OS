#!/usr/bin/env python3
"""Host-side model of `misc::random_bytes_16()`'s fallback (main, issue #16 baseline).

Reimplements the kernel's arithmetic byte-for-byte so the observed AT_RANDOM blocks
can be *predicted* from the publicly observable inputs the kernel itself logs
(ticks / RTC second / pid / whether hardware entropy was available). If the model
reproduces the kernel output exactly, the block carries no secret: it is a
deterministic function of values a third party can read (or enumerate).

Usage:  model.py <serial_log>   — parse `[atrand]` lines and check every sample
"""

from __future__ import annotations

import re
import sys

MASK = (1 << 64) - 1
# Constants as they appear in src/arch/x86_64/linux/misc.rs.
K = 0x9E37_79B9_7F4A_7C15
K2 = 0xBF58_476D_1CE4_E5B9
INIT = 0x9E37_79B9_7F4A_7C15


def rotl(v: int, n: int) -> int:
    return ((v << n) | (v >> (64 - n))) & MASK


class Fallback:
    """The kernel's static xorshift stream, seeded at the fixed public constant."""

    def __init__(self) -> None:
        self.state = INIT

    def next(self, ticks: int, rtc: int, pid: int, hw_term: int = 0) -> bytes:
        x = (self.state ^ (ticks * K) ^ rotl(rtc, 32) ^ ((pid << 48) & MASK) ^ hw_term) & MASK
        x ^= (x << 13) & MASK
        x ^= x >> 7
        x ^= (x << 17) & MASK
        x &= MASK
        second = (x * K2) & MASK
        self.state = x
        return x.to_bytes(8, "little") + second.to_bytes(8, "little")


ATRAND = re.compile(
    r"\[atrand\] hw=(\w+) state_in=([0-9a-f]{16}) ticks=(\d+) rtc=(\d+) pid=(\d+) "
    r"hw_term=([0-9a-f]{16}) bytes=([0-9a-f]{32})"
)


def parse(path: str) -> list[dict]:
    out = []
    with open(path, errors="replace") as fh:
        for line in fh:
            m = ATRAND.search(line)
            if m:
                out.append(
                    {
                        "hw": m.group(1),
                        "state_in": int(m.group(2), 16),
                        "ticks": int(m.group(3)),
                        "rtc": int(m.group(4)),
                        "pid": int(m.group(5)),
                        "hw_term": int(m.group(6), 16),
                        "bytes": bytes.fromhex(m.group(7)),
                    }
                )
    return out


def check_determinism(samples: list[dict]) -> bool:
    """Every logged block must be reproduced from the logged inputs alone."""
    model = Fallback()
    all_ok = True
    for i, s in enumerate(samples):
        # The model must track the kernel's real state; the first sample starts at
        # the public constant, later ones continue the same stream.
        if i == 0:
            assert s["state_in"] == INIT, f"first sample state_in={s['state_in']:#x} != INIT"
        else:
            assert s["state_in"] == model.state, (
                f"sample {i}: kernel state_in={s['state_in']:#018x} "
                f"model={model.state:#018x}"
            )
        predicted = model.next(s["ticks"], s["rtc"], s["pid"], s["hw_term"])
        ok = predicted == s["bytes"]
        all_ok &= ok
        print(
            f"sample {i}: ticks={s['ticks']} rtc={s['rtc']} pid={s['pid']} "
            f"hw_term={s['hw_term']:#x} kernel={s['bytes'].hex()} model={predicted.hex()} "
            f"{'MATCH' if ok else 'MISMATCH'}"
        )
    return all_ok


def enumerate_first_block(sample: dict, window: int = 2000) -> None:
    """Attacker view: rtc second and pid are readable, `ticks` is known to ±window.

    Enumerate the tick candidates and see how many tries it takes to reproduce the
    observed first block — this is the whole 'predictability' claim.
    """
    target = sample["bytes"][:8]
    tries = 0
    hits = []
    for ticks in range(max(0, sample["ticks"] - window), sample["ticks"] + window + 1):
        m = Fallback()
        cand = m.next(ticks, sample["rtc"], sample["pid"], sample["hw_term"])[:8]
        tries += 1
        if cand == target:
            hits.append(ticks)
    print(
        f"\nenumeration attack: {tries} candidate ticks (window ±{window}), "
        f"seed candidates found: {hits} "
        f"(true ticks={sample['ticks']})"
    )
    if hits:
        seed_ticks = hits[0]
        m = Fallback()
        blk = m.next(seed_ticks, sample["rtc"], sample["pid"], sample["hw_term"])
        print(f"  recovered block from public inputs: {blk.hex()}")
        print(
            "  => the seed (and therefore the whole later stream) is recoverable "
            f"in <= {tries} guesses with NO secret input."
        )


def second_half_is_derived(sample: dict) -> None:
    x = int.from_bytes(sample["bytes"][:8], "little")
    derived = (x * K2) & MASK
    print(
        f"\nsecond-half derivation: bytes[8:16]={sample['bytes'][8:].hex()} "
        f"== (bytes[0:8] * K2)={derived.to_bytes(8, 'little').hex()} "
        f"{'CONFIRMED (no independent entropy)' if derived.to_bytes(8, 'little') == sample['bytes'][8:] else 'unexpected'}"
    )


def zero_block_reachability(sample: dict) -> None:
    """All-zero output needs the pre-xorshift value to be 0 (xorshift(0) == 0).

    Solve for the tick value that would do it and report how far away it is — the
    point is that nothing in the code *guarantees* a non-zero block.
    """
    k_inv = pow(K, -1, 1 << 64)
    need = (sample["state_in"] ^ rotl(sample["rtc"], 32) ^ ((sample["pid"] << 48) & MASK) ^ sample["hw_term"]) & MASK
    ticks = (need * k_inv) & MASK
    print(
        f"\nall-zero block: requires ticks={ticks} (true ticks={sample['ticks']}) — "
        f"{'REACHABLE' if ticks < (1 << 40) else 'not reachable in practice'} "
        "(the 'never all-zero' claim is not enforced by the code)"
    )


def main() -> None:
    if len(sys.argv) != 2:
        sys.exit(__doc__)
    samples = parse(sys.argv[1])
    if not samples:
        sys.exit("no [atrand] lines found — was the fallback (no RDRAND/RDSEED) even taken?")
    print(f"parsed {len(samples)} fallback samples (hw={samples[0]['hw']})")
    ok = check_determinism(samples)
    print(f"\nDETERMINISM: {'model reproduces every block (no secret)' if ok else 'MODEL MISMATCH'}")
    enumerate_first_block(samples[0])
    second_half_is_derived(samples[0])
    zero_block_reachability(samples[0])


if __name__ == "__main__":
    main()
