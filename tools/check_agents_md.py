#!/usr/bin/env python3
"""Gate: the *verifiable* claims in `AGENTS.md` must match their sources of truth.

WHY THIS EXISTS. Three times in one session the agent docs handed a reader a stale
fact: the version line lagged `Cargo.toml` by two releases, `Known open gaps` still
said procfs did not exist after it shipped, and the documented end-to-end entry point
was not the one the team runs. Prose review catches that only when someone happens to
read the paragraph.

WHAT IT CHECKS (an explicit list — see `CHECKS` — not a heuristic):

  1. version        the "current … X.Y.Z" claim == `Cargo.toml [package].version`
  2. lockfile       `Cargo.lock`'s `pagh` entry == `Cargo.toml` (AGENTS.md requires the
                    bump to carry the lockfile in the same commit)
  3. tag-list       a "tags A → B → …" claim: every listed version is a git tag and the
                    list ascends; an untagged current version is NOTE-level (the
                    documented pre-tag bump window)
  4. command-set    the fenced block under `## Commands` == the `run:` steps of
                    `.github/workflows/ci.yml`, in BOTH directions. Compared
                    *declaratively*: no PATH lookup, no interpreter execution, and no
                    `python`/`python3` folding — the canonical form is `python3`, and CI
                    runs on ubuntu-latest where an unversioned `python` may not exist, so
                    a gate that resolved the interpreter would be green locally and red
                    in CI. The evidence for the canon is itself checked: every shebang in
                    `tools/*.py` is `#!/usr/bin/env python3` (a bare one would make the
                    tool unrunnable in CI), and ci.yml's `python3` steps spell it that way
                    (its remaining `run:` step is `cargo fmt`). A file that is
                    still in flight is listed in `CANON_PENDING`: reported as NOTE, never a
                    silent pass, and the entry itself fails once the file is clean, so it
                    cannot outlive the branch it describes.
  5. command-count  "Commands (all <word> must be green)" == the number of commands in
                    the block below it
  6. canon-python   no bare `python` inside an AGENTS.md command block, and no stale
                    `python <tool>.py` example anywhere a Linux/CI reader copies commands
                    from (the tools' own docstrings included); every `tools/*.py` shebang
                    is `python3`. Windows-only launchers (`.cmd`, `.ps1`) are exempt by RULE,
                    not by exception: on Windows the interpreter is `python`/`py` (python.org
                    installs no `python3.exe`) and those files call it that way in their
                    runtime lines — the `-legacy` rows of the entry-points table mark them
  7. entry-points   the declared `canonical entry points` registry: every canonical path
                    exists (or is an `ALLOWED_MISSING` in-flight path, see 8), a `-legacy`
                    companion exists, is marked as legacy, and is not the canonical path. **Which tool is current comes from that
                    declaration, not from this gate** — the filesystem cannot tell a
                    legacy script that still exists from the working one, so a
                    declaration is what makes the claim checkable at all.
  8. paths          referenced repository paths exist; globs must match ≥1 file;
                    `ALLOWED_MISSING` exceptions are checked in BOTH directions so an
                    exception cannot outlive its reason — and they expire on
                    *trackedness*, not on a file merely lying around in the working
                    tree, because that is what a reader of a fresh clone gets
  9. absence        absence claims (`does not exist`, `still unverified`, `hangs`) are
                    matched — on the WHITESPACE-FLATTENED document, because these sentences
                    wrap across lines — against the evidence that would contradict them,
                    wherever they live: `Known open gaps` *and* `Hard invariants`. Every
                    entry uses the same polarity (`contradicted_by(root) -> (bool, found)`),
                    and `--probe` exercises each entry in BOTH directions: an earlier version
                    mixed the polarity, which made the class inert — the invariant-9 claim
                    would have survived the merge that falsified it, with a green gate
 10. safety-list    the invariant-10 path list == `tools/check_safety.py::critical`
                    (both directions: a doc describing something else than the gate
                    enforces is a false claim with consequences). Findings carry the line of
                    the path INSIDE invariant 10 — and of the entry inside `check_safety.py`
                    in the reverse direction; when the line cannot be located the gate says
                    "line unknown" rather than pointing at line 1
 11. tracked        a referenced path that is present but NOT tracked by git is a finding:
                    CI and every reader clone the repository, not this working tree. A
                    `tools/e2e.py` that only ever existed here is how a documented entry
                    point became unreachable
 12. folder-docs    every `src/<subsystem>/` that holds `.rs` files has a `README.md`
                    ("each folder documents itself"); nested folders documented by their
                    subsystem README are reported as NOTE
 13. pattern-claims "this file contains this": the vendored embedded-tls patch symbols,
                    `vendor/** -text`, `[workspace] exclude = ["host-tests"]`,
                    `crate-type = ["staticlib"]` — each with its own probe

WHAT IT DELIBERATELY DOES NOT CHECK (judgment, not facts): wording quality, whether a
limitation list is complete, whether an explanation convinces, whether a path is used
correctly, and every absence claim without mechanical evidence (listed in NOT_CHECKED and
printed as NOTE).

Usage:
    python3 tools/check_agents_md.py [--root DIR] [--quiet]
    python3 tools/check_agents_md.py --probe      # + break each class and require failure
Exit status: 0 when every check passes, 1 otherwise.
"""

from __future__ import annotations

import argparse
import glob as globmod
import os
import re
import shutil
import subprocess
import sys
import tempfile

GIT = shutil.which("git") or ""  # empty: every git-backed check degrades to NOTE
DOCS = ["AGENTS.md"]
CI_PATH = ".github/workflows/ci.yml"
SAFETY_PATH = "tools/check_safety.py"

CHECKS = [
    "version-claim", "lockfile", "tag-list", "command-set", "command-count",
    "canon-python", "entry-points", "paths", "absence-claims", "safety-list",
    "tracked-paths", "folder-docs", "pattern-claims",
]

#: The gate reads "which tool is current" from the declaration: see the docstring above
#: (`read_declaration`). The constant exists so the disclaimer cannot be edited away
#: silently — `check_honesty` asserts it stays in this file.
DECLARATION_DISCLAIMER = "Which tool is current comes from that"

#: Stated version claims, as AGENTS.md phrases them (bounded on purpose: a version
#: stated some other way is outside this gate, see NOT_CHECKED).
VERSION_CLAIM = re.compile(r"current(?:ly)?\s+(?:reads?\s+)?[`*]*(\d+\.\d+\.\d+)", re.I)
TAG_LIST_CLAIM = re.compile(r"tags\s+((?:\d+\.\d+\.\d+)(?:\s*[→>-]+\s*\d+\.\d+\.\d+)*)", re.I)
COUNT_CLAIM = re.compile(r"Commands\s*\(all\s+([a-z]+)\s+must be green", re.I)
NUMBER_WORDS = {"one": 1, "two": 2, "three": 3, "four": 4, "five": 5, "six": 6,
                "seven": 7, "eight": 8, "nine": 9, "ten": 10, "1": 1, "2": 2, "3": 3,
                "4": 4, "5": 5, "6": 6, "7": 7, "8": 8, "9": 9, "10": 10}
ENTRY_HEADER = re.compile(r"canonical entry points", re.I)
ENTRY_LINE = re.compile(r"^\s*([a-z0-9-]+)\s*(?:->|→)\s*(\S+)\s*(.*)$")
LEGACY_WORDS = ("legacy", "deprecated", "windows-only", "windows only")

#: Doc shorthands that stand for a path elsewhere in the tree: the gate checks that the
#: target exists, so a shorthand cannot silently dangle (the vendored-crate mention below
#: is written relative to the crate, not to the repository root).
PATH_ALIASES: dict[str, str] = {
    "src/connection.rs": "vendor/embedded-tls/src/connection.rs",
}

#: Paths the doc may name before they are part of the repository. Add only with a reason
#: AND the condition that removes it: the entry itself becomes a finding the moment the
#: path is tracked by git (see `_in_repo`), so this list cannot rot the way the prose did.
ALLOWED_MISSING: dict[str, str] = {
    "docs/procfs.md": "written on vfs/procfs (ac339c8) and in flight for issue #11; "
                      "delete this entry when that PR merges",
    "tools/e2e.py": "committed on tools/e2e-verify-integrity (cd66944) and in flight; the "
                    "Linux/CI replacement for the e2e_*.ps1 harnesses, delete this entry "
                    "when that PR merges",
}

PATH_PREFIXES = ("src/", "tools/", "docs/", "host-tests/", "vendor/", "third_party/",
                 "assets/", "tests/", "rust-apps/", "prebuilt/", "iso_root/", "boot/",
                 ".github/")
BARE_PATHS = ("Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "linker.ld",
              ".gitattributes", "AGENTS.md", "README.md", "CONTRIBUTING.md",
              "SECURITY.md", "HARDENING.md")
PATH_TOKEN = re.compile(
    r"(?<![\w/.-])(\.?/?(?:" + "|".join(p.rstrip("/") for p in PATH_PREFIXES) + r")/"
    r"[A-Za-z0-9_./*<>-]+|" + "|".join(re.escape(b) for b in BARE_PATHS) + r")"
)
FENCE_BLOCK = re.compile(r"```[a-z]*\n(.*?)```", re.S)

#: Absence claims, each with the evidence that CONTRADICTS it. The contract is one-way on
#: purpose — `contradicted_by(root)` returns `(True, "what was found")` when the claim has
#: become false, and `check_absence` fails exactly then. An earlier version mixed the
#: polarity between entries (two of them returned "the evidence is here", which read as
#: "the claim is fine") and the whole class was inert: `repository metadata signatures are
#: still unverified` would have survived the merge that falsified it, with a green gate.
#:
#: The `claim` patterns are matched against the WHITESPACE-FLATTENED document (see
#: `flatten_with_lines`): these sentences wrap across lines, so a pattern written with
#: literal spaces misses the real text and a pattern written to tolerate any spacing misses
#: the edit that matters. Each pattern must also quote the sentence as it is written TODAY —
#: `embedded-tls deterministically hangs on large streams` — not a convenient paraphrase.
ABSENCE_CLAIMS: list[dict] = [
    {
        "claim": re.compile(r"procfs does not exist", re.I),
        "contradicted_by": lambda root: (
            os.path.exists(os.path.join(root, "src/vfs/procfs.rs")),
            "src/vfs/procfs.rs exists"),
        "fix": "rewrite the bullet: name what the first /proc slice serves and what still "
               "returns ENOENT",
        "where": "Known open gaps",
    },
    {
        "claim": re.compile(r"repository metadata signatures are still unverified", re.I),
        "contradicted_by": lambda root: (
            os.path.exists(os.path.join(root, "src/pkg/openpgp.rs"))
            and "openpgp::verify" in _read(os.path.join(root, "src/pkg/apt.rs")),
            "src/pkg/openpgp.rs + apt.rs wire the trust chain"),
        "fix": "rewrite invariant 9: metadata signatures and package digests ARE verified; "
               "name what is still unverified (e.g. rollback, upstream revocation)",
        "where": "Hard invariants (this section is easy to forget — that is why it is checked)",
    },
    {
        "claim": re.compile(r"embedded-tls deterministically hangs on large streams", re.I),
        "contradicted_by": lambda root: (
            "lx_tlsbig" in _read(os.path.join(root, "Cargo.toml"))
            and "run_tls_big_check" in _read(os.path.join(root, "src/selftest_lx.rs")),
            "the lx_tlsbig feature + src/selftest_lx.rs::run_tls_big_check exist"),
        "fix": "drop or reword the gap: the harness that closes it exists",
        "where": "Known open gaps / docs",
    },
]

#: The documentation table promises `src/<subsystem>/README.md`, "each folder documents
#: itself". Its own scope is the subsystem folder; nested folders are documented by their
#: subsystem README, and that wider reading is reported as NOTE so the convention stays
#: visible without turning it into a claim the tree does not meet.
FOLDER_DOC_CLAIM = re.compile(r"each folder documents itself", re.I)

#: Declarative claims of the form "this file contains this". `quote` is the sentence in the
#: guide that makes the claim; `probe` is the (old, new) pair that breaks it for `--probe`.
PATTERN_CLAIMS: list[dict] = [
    {
        "what": "the vendored embedded-tls patch carries `certificate_received` and "
                "`certificate_verified`",
        "quote": "One deliberate local patch",
        "path": "vendor/embedded-tls/src/connection.rs",
        "patterns": (r"certificate_received", r"certificate_verified"),
        "probe": ("certificate_received", "certificate_was_received"),
    },
    {
        "what": "`vendor/**` is `-text` in `.gitattributes` (checksums are byte-sensitive)",
        "quote": "never re-save vendored files",
        "path": ".gitattributes",
        "patterns": (r"^vendor/\*\*\s+-text\s*$",),
        "probe": ("-text", "text"),
    },
    {
        "what": "`[workspace] exclude` keeps `host-tests` out of the kernel workspace",
        "quote": "excluded from the kernel workspace",
        "path": "Cargo.toml",
        "patterns": (r'^exclude\s*=\s*\[\s*"host-tests"\s*\]',),
        "probe": ('exclude = ["host-tests"]', "exclude = []"),
    },
    {
        "what": "the kernel crate is `staticlib`",
        "quote": "`staticlib`",
        "path": "Cargo.toml",
        "patterns": (r'^crate-type\s*=\s*\[\s*"staticlib"\s*\]',),
        "probe": ('crate-type = ["staticlib"]', 'crate-type = ["lib"]'),
    },
]

NOT_CHECKED = [
    "the interpreter spelling inside Windows-only `.cmd`/`.ps1` runtime lines "
    "(CANON_SKIP_SUFFIX: a platform rule, not a defect — Windows has `python`, not `python3`)",
    "NVMe 'no PRP lists and no queue depth > 1' (driver-level knowledge)",
    "signals 'no timer-tick delivery' (judgement about scheduling semantics)",
    "whether a limitation list is complete, or an explanation convincing",
    "a version stated in prose without the word 'current'",
    "whether a 'legacy' entry is really legacy (only the declaration and the marker word)",
]


def self_name(root: str) -> str:
    """This file, named relative to `root` when that is meaningful."""
    rel = os.path.relpath(os.path.abspath(__file__), root)
    return rel if not rel.startswith("..") else "tools/check_agents_md.py"


def _read(path: str) -> str:
    try:
        with open(path, encoding="utf-8", errors="replace") as fh:
            return fh.read()
    except OSError:
        return ""


def _git_index(root: str) -> set[str] | None:
    """Tracked (and staged) paths, or `None` when `root` is not a checkout.

    `None` is the probe's scratch copy and means 'trackedness is not observable here':
    the check then falls back to existence instead of inventing a finding.
    """
    if not GIT:
        return None
    try:
        out = subprocess.run([GIT, "ls-files", "-z"], cwd=root, capture_output=True,
                             text=True)
    except OSError:
        return None
    if out.returncode != 0:
        return None
    return {p for p in out.stdout.split("\0") if p}


def _tracked(index: set[str], rel: str) -> bool:
    """Is `rel` in the index — itself or, for a directory, through its files?

    `git ls-files` lists files only, so a directory the doc names (`src/security/`,
    `third_party/x86_64`) is in the repository exactly when some file below it is.
    """
    rel = rel.rstrip("/")
    return rel in index or any(p.startswith(rel + "/") for p in index)


def _in_repo(root: str, rel: str, index: set[str] | None) -> bool:
    """Would a fresh clone of this repository contain `rel`?"""
    if index is None:
        return os.path.exists(os.path.join(root, rel))
    return _tracked(index, rel)


class Report:
    def __init__(self, root: str, quiet: bool = False) -> None:
        self.root = root
        self.quiet = quiet
        self.failures: list[str] = []
        self.notes: list[str] = []

    def fail(self, doc: str, line: int, message: str, source: str) -> None:
        self.failures.append(f"{os.path.relpath(doc, self.root)}:{line}: {message} "
                             f"(checked against {source})")

    def fail_plain(self, message: str) -> None:
        self.failures.append(f"{message}")

    def note(self, message: str) -> None:
        self.notes.append(message)

    def emit(self) -> int:
        for line in self.failures:
            print(line)
        if not self.quiet:
            for line in self.notes:
                print(f"NOTE {line}")
        if self.failures:
            print(f"agents-doc gate: FAIL ({len(self.failures)} finding(s))")
            return 1
        print("agents-doc gate: OK")
        return 0


def line_of(text: str, match: re.Match) -> int:
    return text[: match.start()].count("\n") + 1


def flatten_with_lines(text: str) -> tuple[str, list[int]]:
    """`(flat, line_of_char)`: whitespace collapsed to single spaces.

    Prose in AGENTS.md wraps: the sentence about unverified metadata signatures ends a line
    with `signatures` and starts the next with `are still unverified`. A regex with literal
    spaces cannot see it, and `line_of` computed on the flat text would report a line that
    does not exist in the file — so every flat character carries the line it came from.
    """
    out: list[str] = []
    lines: list[int] = []
    line, pending_space = 1, False
    for ch in text:
        if ch == "\n":
            line += 1
        if ch.isspace():
            pending_space = bool(out)
            continue
        if pending_space:
            out.append(" ")
            lines.append(line)
            pending_space = False
        out.append(ch)
        lines.append(line)
    return "".join(out), lines


# ─── sources of truth ───────────────────────────────────────────────────────


def crate_version(root: str) -> str | None:
    """`[package].version` — explicitly that section: `Cargo.toml` also carries a
    dependency line with a version (`limine = "0.6"`), so 'the first version-looking
    line' is a trap this function must not fall into (mutant-tested in `--probe`)."""
    text = _read(os.path.join(root, "Cargo.toml"))
    pkg = re.search(r"^\[package\]\s*$(.*?)(?=^\[|\Z)", text, re.M | re.S)
    if not pkg:
        return None
    m = re.search(r"^version\s*=\s*\"([^\"]+)\"", pkg.group(1), re.M)
    return m.group(1) if m else None


def ci_commands(root: str) -> list[tuple[str, int]]:
    text = _read(os.path.join(root, CI_PATH))
    out = []
    for i, line in enumerate(text.splitlines(), 1):
        m = re.match(r"\s*(?:-\s*)?run:\s*(.+)$", line)
        if m and m.group(1).strip() not in ("|", ">"):
            out.append((normalize_cmd(m.group(1)), i))
    return out


def normalize_cmd(cmd: str) -> str:
    """Fold whitespace and strip inline comments ONLY.

    Deliberately no `python`→`python3` folding: the canonical interpreter is part of
    what the doc must get right (see the module docstring), and CI's own commands spell
    it `python3`.
    """
    return re.sub(r"\s+", " ", cmd.split("#", 1)[0]).strip()


def fenced_blocks(text: str) -> list[tuple[str, int]]:
    """`(body, first_line_number)` for every fenced block."""
    return [(m.group(1), line_of(text, m) + 1) for m in FENCE_BLOCK.finditer(text)]


def commands_block(text: str) -> tuple[list[tuple[str, int]], int] | None:
    lines = text.splitlines()
    start = next((i for i, l in enumerate(lines) if l.strip().startswith("## Commands")), None)
    if start is None:
        return None
    fence = next((i for i in range(start, len(lines)) if lines[i].startswith("```")), None)
    if fence is None:
        return None
    end = next((i for i in range(fence + 1, len(lines)) if lines[i].startswith("```")), len(lines))
    cmds = []
    for i in range(fence + 1, end):
        raw = lines[i]
        if not raw.strip() or raw.strip().startswith("#"):
            continue
        cmds.append((normalize_cmd(raw), i + 1))
    return cmds, fence + 1


def safety_list(root: str) -> list[tuple[str, int]]:
    """`[(path, line)]` from the `critical` array of `tools/check_safety.py`.

    The line matters in the reverse direction of the comparison: when the script enforces
    something the guide does not list, the finding has to point at the entry, not at the
    script as a whole.
    """
    text = _read(os.path.join(root, SAFETY_PATH))
    m = re.search(r"critical\s*=\s*\[(.*?)\]", text, re.S)
    if not m:
        return []
    out = []
    for item in m.group(1).split(","):
        item = item.strip()
        if not item:
            continue
        lit = re.search(r"['\"]([^'\"]+)['\"]", item)
        if lit:
            path, needle = lit.group(1).strip("/"), lit.group(1)
        else:
            seg = re.findall(r"['\"]([^'\"]+)['\"]|(\w+)", item)
            parts = [a or b for a, b in seg]
            if not parts:
                continue
            path, needle = "/".join(parts), parts[-1]
        off = text.find(needle, m.start())
        out.append((path, text[:off].count("\n") + 1 if off >= 0 else 0))
    return out


def invariant10(text: str) -> tuple[str, int]:
    """`(body, first_line)` of invariant 10, or `("", 0)` when it is not there.

    The span is what makes a finding point INSIDE the invariant: a path named in the text
    (`src/security/`) also appears elsewhere in the file, and searching the whole document
    reported the first hit — an unrelated line about the Limine loader.
    """
    m = re.search(r"^10\.\s\*\*Unsafe policy\*\*(.*?)(?=^## |\Z)", text, re.M | re.S)
    if not m:
        return "", 0
    return m.group(1), line_of(text, m)


def invariant10_list(text: str) -> list[str]:
    """The path list of invariant 10, exactly as that invariant states it.

    Bounded to the invariant (not to the end of the file — an unnumbered list has no
    `11.` to stop at) and cut at the clause that follows it, because the same sentence
    also names the *gate* (`tools/check_safety.py`), which is not one of the files the
    gate covers.
    """
    body, _first = invariant10(text)
    if not body:
        return []
    listing = re.split(r"needs a", body, maxsplit=1)[0]
    out = []
    for tok in re.findall(r"`([^`]+)`", listing):
        tok = tok.strip().rstrip("/")
        if tok.endswith("unsafe {") or tok.startswith("unsafe"):
            continue
        out.append(tok)
    return out


def entry_points(text: str) -> tuple[list[tuple[int, str, str, str]], int]:
    """`(line, role, path, rest)` for the declared canonical entry points."""
    lines = text.splitlines()
    start = next((i for i, l in enumerate(lines) if ENTRY_HEADER.search(l)), None)
    if start is None:
        return [], 0
    fence = next((i for i in range(start, len(lines)) if lines[i].startswith("```")), None)
    if fence is None:
        return [], 0
    end = next((i for i in range(fence + 1, len(lines)) if lines[i].startswith("```")), len(lines))
    out = []
    for i in range(fence + 1, end):
        m = ENTRY_LINE.match(lines[i])
        if m:
            out.append((i + 1, m.group(1), m.group(2), m.group(3)))
    return out, fence + 1


# ─── checks ─────────────────────────────────────────────────────────────────


def check_version(rep: Report, doc: str, text: str) -> None:
    want = crate_version(rep.root)
    if want is None:
        rep.fail_plain("Cargo.toml: no [package] version found (nothing to check the docs against)")
        return
    claims = list(VERSION_CLAIM.finditer(text))
    if not claims:
        rep.note(f"{os.path.relpath(doc, rep.root)}: no 'current … X.Y.Z' claim found; the gate "
                 f"cannot verify the version (Cargo.toml = {want}) — state it that way")
        return
    for m in claims:
        if m.group(1) != want:
            rep.fail(doc, line_of(text, m),
                     f"version claim '{m.group(0).strip()}' disagrees with the crate version",
                     f"Cargo.toml [package].version = {want}")


def check_lockfile(rep: Report, doc: str, text: str) -> None:
    version = crate_version(rep.root)
    lock = _read(os.path.join(rep.root, "Cargo.lock"))
    m = re.search(r'\[\[package\]\]\nname\s*=\s*"pagh"\nversion\s*=\s*"([^"]+)"', lock)
    if m is None:
        rep.fail_plain('Cargo.lock: no [[package]] "pagh" entry found')
        return
    if version and m.group(1) != version:
        line = next((i for i, l in enumerate(text.splitlines(), 1)
                     if "Cargo.lock" in l and "bump" in l.lower()), 1)
        rep.fail(doc, line, f"Cargo.lock pagh = {m.group(1)} but Cargo.toml = {version}",
                 "Cargo.toml vs Cargo.lock (AGENTS.md requires them to move together)")


def check_tags(rep: Report, doc: str, text: str) -> None:
    m = TAG_LIST_CLAIM.search(text)
    if not m:
        rep.note(f"{os.path.relpath(doc, rep.root)}: no 'tags A → B → …' claim to check")
        return
    listed = re.findall(r"\d+\.\d+\.\d+", m.group(1))
    if not GIT:
        rep.note(f"{os.path.relpath(doc, rep.root)}: git is not on PATH — the tag claim was "
                 f"not checked (and cannot be: tags live in the repository)")
        return
    shallow = subprocess.run([GIT, "rev-parse", "--is-shallow-repository"], cwd=rep.root,
                             capture_output=True, text=True)
    if shallow.stdout.strip() == "true":
        rep.note(f"{os.path.relpath(doc, rep.root)}: shallow clone (fetch-depth 1) — the tag "
                 f"claim was not checked; the CI job fetches full history with tags")
        return
    tags = subprocess.run([GIT, "tag", "--list"], cwd=rep.root,
                          capture_output=True, text=True).stdout.split()
    if not tags:
        rep.note("git tags unavailable (not a checkout?) — the tag claim was not checked")
        return
    version = crate_version(rep.root)
    line = line_of(text, m)
    for v in listed:
        if v not in tags:
            if version and v == version:
                rep.note(f"{v} is claimed as the current version and is not tagged yet "
                         f"(documented pre-tag bump window); tag the release commit")
            else:
                rep.fail(doc, line, f"tag list names '{v}', which is not a git tag",
                         f"git tag ({len(tags)} tags), e.g. {', '.join(sorted(tags)[-3:])}")
    if listed != sorted(listed, key=lambda s: [int(p) for p in s.split(".")]):
        rep.fail(doc, line, "tag list is not ascending", "git tag")


def check_commands(rep: Report, doc: str, text: str) -> None:
    ci = dict(ci_commands(rep.root))
    block = commands_block(text)
    if not ci:
        rep.fail_plain(f"{CI_PATH}: no run: steps found")
        return
    if block is None:
        rep.fail_plain(f"{os.path.relpath(doc, rep.root)}: no '## Commands' fenced block found")
        return
    doc_cmds, _ = block
    doc_set = dict(doc_cmds)
    for cmd, line in doc_set.items():
        if cmd not in ci:
            near = [c for c in ci if c.split()[0] == cmd.split()[0]]
            extra = f"; CI spells it {' / '.join(repr(c) for c in near)}" if near else ""
            rep.fail(doc, line, f"command '{cmd}' is documented as a gate but CI does not run it"
                                f"{extra}", f"the run: steps of {CI_PATH}")
    for cmd, line in ci.items():
        if cmd not in doc_set:
            rep.fail_plain(f"{CI_PATH}:{line}: CI runs '{cmd}' but the command block in "
                           f"{os.path.relpath(doc, rep.root)} does not list it")


def check_command_count(rep: Report, doc: str, text: str) -> None:
    m = COUNT_CLAIM.search(text)
    if not m:
        rep.note(f"{os.path.relpath(doc, rep.root)}: no 'Commands (all … must be green)' claim")
        return
    warned = NUMBER_WORDS.get(m.group(1).lower())
    block = commands_block(text)
    if warned is None or block is None:
        return
    actual = len(block[0])
    if warned != actual:
        rep.fail(doc, line_of(text, m),
                 f"the heading says 'all {m.group(1)}' but the block lists {actual} commands",
                 "the command block in the same file")


#: A copy-pasteable `python something.py` example (not a regex, not `python3`, not a path
#: like `./python/x.py`). Deliberately narrow: it must not match this gate's own strings.
BARE_PYTHON_CMD = re.compile(r"(?<![-\w./])python\s+[\w./-]*\.py")

#: Where the canon is scanned beyond AGENTS.md: the files a contributor copies commands
#: out of. `python FILE.py` needs an unversioned interpreter that ubuntu-latest lacks.
CANON_SCAN = (".github/workflows/ci.yml", "build.sh", "run.sh", "Makefile", "tools")

#: Windows-only launchers, exempt from the canon BY RULE: on Windows the interpreter is
#: `python` (or `py`) — python.org installs no `python3.exe` — and these files call it that
#: way in their runtime lines. A statement about the platform, so it needs no exceptions.
CANON_SKIP_SUFFIX = (".cmd", ".ps1")

#: Files whose examples are stale while the branch that rewrites them is still in flight.
#: Reported as NOTE (never a silent pass), and the entry becomes a FINDING once the file is
#: clean — so it cannot outlive the branch it describes (`ALLOWED_MISSING` discipline).
#: `tools/e2e.py` was the first entry: it carried a stale hint (`run: python tools/limine.py`)
#: until tools/e2e-verify-integrity f2bda5d spelled it `python3`, and the entry was removed
#: with that commit rather than left to rot. `--probe src-canon-pending-stale` exercises the
#: expiry path either way, so the mechanism is never untested dead code. The two entries
#: The entry is deliberately NOT used for the files that are already on main
#: (`tools/qemu_shot.py`, `tools/README.md`, four stale examples each, from 7784d83): a
#: tolerance declared in advance for a defect that is already merged would be an excuse, not
#: a schedule. Those are findings until the follow-up commit fixes the lines — the same
#: commit that drops the ALLOWED_MISSING entries — and until then the gate is right to be
#: red on a tree that ships copy-paste failures.
CANON_PENDING: dict[str, str] = {}


def canon_scan_files(root: str) -> list[str]:
    index = _git_index(root)
    if index is not None:
        files = sorted(index)
    else:  # probe scratch copy: no checkout, so walk instead
        files = []
        for rel in CANON_SCAN:
            full = os.path.join(root, rel)
            if os.path.isfile(full):
                files.append(rel)
            elif os.path.isdir(full):
                for dirpath, _dirs, names in os.walk(full):
                    files += [os.path.relpath(os.path.join(dirpath, n), root)
                              for n in sorted(names)]
    keep = tuple(f for f in CANON_SCAN if os.path.isdir(os.path.join(root, f)))
    return [f for f in files
            if f == ".github/workflows/ci.yml" or f.startswith(keep)
            or os.path.basename(f) in ("build.sh", "run.sh", "Makefile")]


def check_canon_python(rep: Report, doc: str, text: str) -> None:
    canon = "python3"
    offenders = []
    for body, first in fenced_blocks(text):
        for off, raw in enumerate(body.splitlines()):
            line = raw.split("#", 1)[0]
            if re.search(r"(?<![\w./-])python(?![\w.3-])", line):
                offenders.append((first + off, raw.strip()))
    for line, raw in offenders:
        rep.fail(doc, line, f"bare 'python' in a command block ('{raw}') — the repository "
                            f"canon is '{canon}'", "the `#!/usr/bin/env python3` shebangs "
                            "of tools/*.py and the run: steps of ci.yml")
    # `self_name` falls back to the repo-relative name, which matters in the probe's
    # scratch copy: there `__file__` still points at this checkout, and without the
    # fallback the copy of THIS file would be scanned and its own strings reported.
    skip = {os.path.relpath(doc, rep.root), self_name(rep.root)}
    for rel, why in CANON_PENDING.items():
        if not os.path.exists(os.path.join(rep.root, rel)):
            rep.note(f"{rel}: CANON_PENDING not verifiable in this tree (in flight?): {why}")
    for rel in canon_scan_files(rep.root):
        if rel in skip or rel.endswith(CANON_SKIP_SUFFIX):
            continue  # the guide's own blocks were checked above; this file's strings
                      # describe the check; Windows-only launchers are exempt by rule
        body = _read(os.path.join(rep.root, rel))
        head = (body.splitlines() or [""])[0]
        if rel.endswith(".py") and head.startswith("#!") and "python" in head \
                and "python3" not in head:
            rep.fail_plain(f"{rel}:1: shebang '{head}' — the repository canon is "
                           f"'#!/usr/bin/env python3' (a bare `python` may not exist on "
                           f"ubuntu-latest, where the tool is expected to run)"
                           f" (checked against the shebangs of tools/*.py)")
        hits = [(i, line) for i, line in enumerate(body.splitlines(), 1)
                if BARE_PYTHON_CMD.search(line)]
        if not hits:
            if rel in CANON_PENDING:
                rep.fail_plain(f"{rel}: CANON_PENDING says the file still has a stale "
                               f"`python` example, but it is clean now — remove the entry "
                               f"({CANON_PENDING[rel]})")
            continue
        for i, line in hits:
            detail = (f"{rel}:{i}: bare 'python' in a usage example ('{line.strip()}') — the "
                      f"repository canon is '{canon}'; this is a copy-paste failure on "
                      f"ubuntu-latest, where the unversioned interpreter may not exist "
                      f"(checked against the shebangs of tools/*.py and the run: steps of "
                      f"ci.yml)")
            if rel in CANON_PENDING:
                rep.note(f"{detail} [tolerated while in flight: {CANON_PENDING[rel]}]")
            else:
                rep.fail_plain(detail)


def check_entry_points(rep: Report, doc: str, text: str) -> None:
    entries, _ = entry_points(text)
    if not entries:
        rep.fail_plain(f"{os.path.relpath(doc, rep.root)}: no 'canonical entry points' "
                       f"declaration found — the E2E/tooling claims cannot be checked "
                       f"without it (see this script's docstring)")
        return
    roles = {role: (line, path, rest) for line, role, path, rest in entries}
    index = _git_index(rep.root)
    for line, role, path, rest in entries:
        full = os.path.join(rep.root, path)
        exists = bool(globmod.glob(full)) if any(c in path for c in "*?[") else os.path.exists(full)
        if not exists:
            if path in ALLOWED_MISSING and not _in_repo(rep.root, path, index):
                rep.note(f"entry point '{role}' -> '{path}' is in flight: "
                         f"{ALLOWED_MISSING[path]}")
            else:
                rep.fail(doc, line, f"entry point '{role}' names '{path}', which does not exist",
                         "the working tree (ALLOWED_MISSING takes a reason and self-expires)")
        if role.endswith("-legacy"):
            base = role[: -len("-legacy")]
            if base not in roles:
                rep.fail(doc, line, f"'{role}' has no canonical counterpart '{base}'",
                         "the declaration itself")
            elif roles[base][1] == path:
                rep.fail(doc, line, f"'{role}' and '{base}' name the same path ('{path}')",
                         "the declaration itself")
            if not any(w in rest.lower() for w in LEGACY_WORDS):
                rep.fail(doc, line, f"'{role}' is not marked as legacy/deprecated/windows-only: "
                                    f"'{rest.strip()}'", "the declaration's own marker words")
    rep.note(f"entry points come from the declaration above; the gate does not decide which "
             f"tool is current (see {self_name(rep.root)} docstring)")


def check_paths(rep: Report, doc: str, text: str) -> None:
    seen: set[str] = set()
    index = _git_index(rep.root)
    for i, line in enumerate(text.splitlines(), 1):
        for m in PATH_TOKEN.finditer(line):
            token = m.group(1).rstrip(".,;:)`")
            if token in seen or "<" in token or token.startswith("./"):
                continue
            seen.add(token)
            full = os.path.join(rep.root, token)
            if any(c in token for c in "*?["):
                if not globmod.glob(full):
                    rep.fail(doc, i, f"glob '{token}' matches no file", "the working tree")
                continue
            if token in ALLOWED_MISSING:
                if _in_repo(rep.root, token, index):
                    rep.fail(doc, i, f"'{token}' is now in the repository but is still listed "
                                     f"in ALLOWED_MISSING — remove the stale exception",
                             f"{self_name(rep.root)}::ALLOWED_MISSING")
                else:
                    rep.note(f"'{token}' is absent from the repository by exception: "
                             f"{ALLOWED_MISSING[token]}")
                continue
            if os.path.exists(full):
                if index is not None and not _tracked(index, token):
                    rep.fail(doc, i, f"'{token}' exists in this working tree but is not "
                                     f"tracked by git — a fresh clone (and CI) would not "
                                     f"have it", "git ls-files")
                continue
            if token in PATH_ALIASES:
                target = PATH_ALIASES[token]
                if not os.path.exists(os.path.join(rep.root, target)):
                    rep.fail(doc, i, f"'{token}' is declared as a shorthand for '{target}', "
                                     f"which does not exist either", "PATH_ALIASES + the tree")
                continue
            rep.fail(doc, i, f"referenced path '{token}' does not exist",
                     "the working tree (ALLOWED_MISSING takes a reason and self-expires)")


def check_absence(rep: Report, doc: str, text: str) -> None:
    """Absence claims are searched in the WHOLE document, so a claim moved from 'Known
    open gaps' into 'Hard invariants' (where it is easier to forget) is still caught.

    Matching happens on the flattened text and every hit is reported at the line it really
    came from; the finding appears when the contradiction is present, never the other way
    round (see `ABSENCE_CLAIMS`).
    """
    flat, line_at = flatten_with_lines(text)
    for entry in ABSENCE_CLAIMS:
        m = entry["claim"].search(flat)
        if not m:
            continue
        contradicted, found = entry["contradicted_by"](rep.root)
        if contradicted:
            rep.fail(doc, line_at[m.start()],
                     f"absence claim '{m.group(0).strip()}' (in {entry['where']}) is "
                     f"contradicted: {found}; {entry['fix']}", found)
    for item in NOT_CHECKED:
        rep.note(f"not machine-checked (judgement): {item}")


def _same_path(a: str, b: str) -> bool:
    """Component-aligned suffix match.

    The invariant writes the list as prose — `src/security/`, then `arch/x86_64/…`,
    `memory/…`, i.e. the `src/` prefix is elided after the first entry — while the gate
    spells every path in full. Comparing the *files each side names* (not the spelling)
    is what makes the check about the gate's behaviour instead of punctuation.
    """
    return a == b or a.endswith("/" + b) or b.endswith("/" + a)


def check_safety_list(rep: Report, doc: str, text: str) -> None:
    doc_paths = invariant10_list(text)
    src_paths = safety_list(rep.root)
    if not src_paths:
        rep.fail_plain(f"{SAFETY_PATH}: no `critical` array found")
        return
    if not doc_paths:
        rep.fail_plain(f"{os.path.relpath(doc, rep.root)}: invariant 10 lists no paths")
        return
    body, first = invariant10(text)
    for p in doc_paths:
        if not any(_same_path(p, q) for q, _line in src_paths):
            offset = next((off for off, l in enumerate(body.splitlines()) if f"`{p}`" in l),
                          None)
            message = (f"invariant 10 names '{p}', which {SAFETY_PATH} does not enforce")
            if offset is None:
                # No silent `, 1)`: a wrong line sends the reader to the wrong place.
                rep.fail_plain(f"{os.path.relpath(doc, rep.root)}: invariant 10 (line unknown): "
                               f"{message} (checked against {SAFETY_PATH}::critical "
                               f"({len(src_paths)} entries))")
            else:
                rep.fail(doc, first + offset, message,
                         f"{SAFETY_PATH}::critical ({len(src_paths)} entries)")
    for q, line in src_paths:
        if not any(_same_path(p, q) for p in doc_paths):
            where = f"{SAFETY_PATH}:{line}" if line else f"{SAFETY_PATH} (line unknown)"
            rep.fail_plain(f"{where}: enforces '{q}' but AGENTS.md invariant 10 does not "
                           f"list it (both directions matter: a doc describing a different "
                           f"gate is a false claim)")


def check_folder_docs(rep: Report, doc: str, text: str) -> None:
    """Every `src/<subsystem>/` that holds `.rs` files has a `README.md`."""
    m = FOLDER_DOC_CLAIM.search(text)
    if not m:
        rep.note(f"{os.path.relpath(doc, rep.root)}: no 'each folder documents itself' claim")
        return
    index = _git_index(rep.root)
    if index is not None:
        files = sorted(index)
    else:  # probe scratch copy: no checkout, so walk
        files = []
        for dirpath, _dirs, names in os.walk(os.path.join(rep.root, "src")):
            files += [os.path.relpath(os.path.join(dirpath, n), rep.root) for n in names]
    dirs: set[str] = set()
    for rel in files:
        if not rel.startswith("src/") or not rel.endswith(".rs"):
            continue
        parts = rel.split("/")[:-1]
        for i in range(2, len(parts) + 1):
            dirs.add("/".join(parts[:i]))
    for d in sorted(dirs):
        if f"{d}/README.md" in files:
            continue
        if len(d.split("/")) == 2:
            rep.fail(doc, line_of(text, m),
                     f"'{d}' holds code but has no README.md — the table promises every "
                     f"folder documents itself", "the tracked tree (src/<subsystem>/README.md)")
        else:
            rep.note(f"{d}: no README of its own — documented by its subsystem README; the "
                     f"claim's own scope is src/<subsystem>/")


def check_patterns(rep: Report, doc: str, text: str) -> None:
    """The "this file contains this" claims, one regex each.

    A claim whose sentence is no longer in the guide is reported as NOTE: the check
    verifies what the guide states, it does not invent requirements of its own.
    """
    for claim in PATTERN_CLAIMS:
        m = re.search(re.escape(claim["quote"]), text)
        if not m:
            rep.note(f"{os.path.relpath(doc, rep.root)}: no claim '{claim['quote']}' — "
                     f"'{claim['what']}' is not checked")
            continue
        body = _read(os.path.join(rep.root, claim["path"]))
        if not body:
            rep.fail(doc, line_of(text, m),
                     f"'{claim['path']}' is missing or empty, so '{claim['what']}' cannot "
                     f"hold", "the tracked tree")
            continue
        for pattern in claim["patterns"]:
            if not re.search(pattern, body, re.M):
                rep.fail(doc, line_of(text, m),
                         f"'{claim['path']}' does not match /{pattern}/ — {claim['what']}",
                         f"the text of {claim['path']}")


def check_honesty(rep: Report, _doc: str, _text: str) -> None:
    """The disclaimer that the gate reads the declaration must survive edits."""
    if DECLARATION_DISCLAIMER not in _read(os.path.abspath(__file__)):
        rep.fail_plain(f"{self_name(rep.root)}: the declaration disclaimer "
                       f"is gone — the gate must not look like it derives which tool is current")


def run_checks(root: str, quiet: bool = False) -> Report:
    rep = Report(root, quiet)
    check_honesty(rep, "", "")
    for rel in DOCS:
        doc = os.path.join(root, rel)
        text = _read(doc)
        if not text:
            rep.fail_plain(f"{rel}: missing or empty (the gate needs it to check anything)")
            continue
        check_version(rep, doc, text)
        check_lockfile(rep, doc, text)
        check_tags(rep, doc, text)
        check_commands(rep, doc, text)
        check_command_count(rep, doc, text)
        check_canon_python(rep, doc, text)
        check_entry_points(rep, doc, text)
        check_paths(rep, doc, text)
        check_absence(rep, doc, text)
        check_safety_list(rep, doc, text)
        check_folder_docs(rep, doc, text)
        check_patterns(rep, doc, text)
    return rep


# ─── negative probes: the gate must be able to fail ─────────────────────────
#
# Each probe mutates a temp COPY and requires at least one finding. Two of them mutate
# the SOURCE (`Cargo.toml`, `ci.yml`, `check_safety.py`) and leave the doc alone: that is
# the test for a self-confirming gate, which would otherwise read the document on both
# sides and always agree with itself.

#: `(name, what the probe breaks, the finding text it MUST produce)`. The third field is
#: what makes a probe evidence: 'some finding appeared' is not, because an unfaithful
#: scratch copy fails every probe by accident (see `copy_tree`).
PROBES: list[tuple[str, str, str]] = [
    ("doc-version", "version: change the claim in AGENTS.md",
     "disagrees with the crate version"),
    ("doc-lockfile", "lockfile: change Cargo.lock", "Cargo.lock pagh = 9.9.9"),
    ("doc-command-removed", "commands: delete a command from the documented block",
     "does not list it"),
    ("doc-command-added", "commands: add a command the docs promise but CI lacks",
     "documented as a gate but CI does not run it"),
    ("doc-command-spelling", "commands: python3 -> python (canon divergence, not PATH)",
     "documented as a gate but CI does not run it"),
    ("doc-count", "command-count: the heading count stops matching the block",
     "the heading says"),
    ("doc-glob", "paths: point a referenced glob at nothing", "matches no file"),
    ("absence-1-fire", "absence #1: procfs.rs lands and the gap claim stays",
     "procfs does not exist"),
    ("absence-1-clean", "absence #1 control: the claim is back, the evidence is NOT", ""),
    ("absence-2-fire", "absence #2: the trust chain lands on a claim wrapped across lines",
     "repository metadata signatures are still unverified"),
    ("absence-2-clean", "absence #2 control: claim back, no trust chain", ""),
    ("absence-3-fire", "absence #3: lx_tlsbig lands and the 'deterministically hangs' bullet stays",
     "embedded-tls deterministically hangs on large streams"),
    ("absence-3-clean", "absence #3 control: claim back, no harness", ""),
    ("doc-safety", "safety-list: a path in invariant 10 that check_safety.py does not enforce",
     "which tools/check_safety.py does not enforce"),
    ("doc-entry-point", "entry-points: canonical path that does not exist",
     "entry point 'kernel-e2e' names"),
    ("doc-entry-legacy", "entry-points: unmarked legacy companion",
     "is not marked as legacy"),
    ("src-version", "SOURCE: bump Cargo.toml, leave the doc",
     "disagrees with the crate version"),
    ("src-ci-step", "SOURCE: add a run: step to ci.yml, leave the doc", "does not list it"),
    ("src-safety", "SOURCE: add a path to check_safety.py::critical, leave the doc",
     "invariant 10 does not list it"),
    ("src-cargo-trap", "SOURCE: dependency version bumped, [package] untouched", ""),
    ("src-canon-example", "SOURCE: a stale `python tool.py` example in a Linux-facing file",
     "bare 'python' in a usage example"),
    ("src-canon-pending-stale", "the gate's own knob: a clean file declared as in flight",
     "CANON_PENDING says the file still has a stale"),
    ("src-shebang", "SOURCE: a tool shebang that spells the interpreter bare",
     "shebang '#!/usr/bin/env python'"),
    ("doc-exception-stale", "ALLOWED_MISSING: the in-flight path appears, the excuse must go",
     "remove the stale exception"),
    ("folder-doc-removed", "folder-docs: a subsystem README disappears",
     "has no README.md"),
    ("folder-doc-new", "folder-docs: a new subsystem folder arrives without one",
     "has no README.md"),
    ("no-git", "the gate's own knob: git is not on PATH (must degrade, not crash)", ""),
    ("src-untracked", "SOURCE: a checkout where the documented path is present but uncommitted",
     "not tracked by git"),
]
#: One probe per declarative claim: break exactly the pattern it looks for.
PROBES += [(f"src-pattern-{i}", f"SOURCE: break '{c['what']}'", "does not match /")
           for i, c in enumerate(PATTERN_CLAIMS)]


#: Probes that patch one of the gate's own knobs instead of a file. The knobs are read from
#: THIS module, not from the scratch tree, so a file edit in the copy could not change them;
#: the patch lives only for the duration of that run, and the scratch tree stays a faithful
#: copy of the repository.
MEMORY_PROBES: dict[str, dict[str, object]] = {
    "src-canon-pending-stale": {"CANON_PENDING": {"tools/limine.py": "probe: pretend it is "
                                                                     "in flight and stale"}},
    "no-git": {"GIT": ""},
}


def probe(root: str) -> int:
    ok = True
    for name, what, marker in PROBES:
        with tempfile.TemporaryDirectory() as tmp:
            copy_tree(root, tmp)
            try:
                apply_probe(tmp, name)
            except Exception as exc:  # a probe that cannot be applied proves nothing
                print(f"PROBE {name:20} ERROR   {exc}")
                ok = False
                continue
            patched = MEMORY_PROBES.get(name, {})
            saved = {k: globals()[k] for k in patched}
            globals().update(patched)
            try:
                rep = run_checks(tmp, quiet=True)
            except Exception as exc:  # a gate that dies on an odd tree is not a gate
                print(f"PROBE {name:20} ERROR   run_checks raised {exc!r}")
                ok = False
                continue
            finally:
                globals().update(saved)
            if not marker:  # self-asserting probe: it raised on any unexpected finding
                caught, detail = True, "-> self-asserting: no spurious finding appeared"
            else:
                hits = [f for f in rep.failures if marker in f]
                caught = bool(hits)
                detail = f"-> {hits[0]}" if caught else (
                    f"(wanted a finding containing {marker!r}; got "
                    f"{rep.failures[0] if rep.failures else 'no findings'})")
            ok &= caught
            print(f"PROBE {name:20} {'caught ' if caught else 'MISSED '} {what} {detail}")
    print("agents-doc gate probe:", "OK (every break was caught)" if ok else
          "FAILED (a break slipped through — the gate is not doing its job)")
    return 0 if ok else 1


def apply_probe(root: str, name: str) -> None:
    doc = os.path.join(root, "AGENTS.md")
    if name in MEMORY_PROBES:
        return  # nothing on disk: the probe patches the gate's own knob, see MEMORY_PROBES
    if name == "doc-version":
        _sub(doc, VERSION_CLAIM, lambda m: m.group(0).replace(m.group(1), "9.9.9"))
    elif name == "doc-lockfile":
        _sub(os.path.join(root, "Cargo.lock"),
             re.compile(r'(name = "pagh"\nversion = ")([^"]+)'), lambda m: m.group(1) + "9.9.9")
    elif name == "doc-command-removed":
        _sub(doc, re.compile(r"^python3 tools/host_tests\.py.*$", re.M), lambda m: "# gone")
    elif name == "doc-command-added":
        _sub(doc, re.compile(r"^python3 tools/build\.py build\b(?!.*--release).*$", re.M),
             lambda m: m.group(0) + "\npython3 tools/not_a_gate.py")
    elif name == "doc-command-spelling":
        _sub(doc, re.compile(r"^python3 tools/build\.py build --release", re.M),
             lambda m: m.group(0).replace("python3", "python", 1))
    elif name == "doc-count":
        _sub(doc, re.compile(r"\(all [a-z0-9]+ must be green"), lambda m: "(all nine must be green")
    elif name == "doc-glob":
        _sub(doc, re.compile(r"tools/e2e_\*\.ps1"), lambda m: "tools/e2e_*_gone.ps1")
    elif name.startswith("absence-"):
        # Every absence entry is probed in BOTH directions, because one direction is what
        # makes the class trustworthy: with the contradiction landed the gate must fire, and
        # with the claim back but the evidence absent it must stay silent. (A single probe on
        # entry #1 is how a mixed-up polarity went unnoticed.)
        _kind, idx_s, direction = name.split("-")
        idx = int(idx_s)
        _append(root, "AGENTS.md", "\n" + ABSENCE_PROBE_TEXT[idx - 1] + "\n")
        if direction == "fire":
            _land_contradiction(root, idx)
        else:
            rep = run_checks(root, quiet=True)
            fired = [f for f in rep.failures if "absence claim" in f]
            if fired:
                raise AssertionError(f"the claim fired with no contradiction present: "
                                     f"{fired[0]}")
            return
    elif name == "folder-doc-removed":
        os.remove(os.path.join(root, "src/net/README.md"))
    elif name == "folder-doc-new":
        _touch(root, "src/newsub/mod.rs")
    elif name.startswith("src-pattern-"):
        claim = PATTERN_CLAIMS[int(name.rsplit("-", 1)[1])]
        _sub(os.path.join(root, claim["path"]), re.compile(re.escape(claim["probe"][0])),
             lambda m: claim["probe"][1], count=0)
    elif name == "doc-safety":
        # A real path that check_safety.py::critical does not contain, so the only finding
        # is the safety-list one (an invented path would be caught by the paths check).
        _sub(doc, re.compile(r"(every `unsafe \{` in )"), lambda m: m.group(1) + "`tools/limine.py`, ")
    elif name == "doc-entry-point":
        _sub(doc, re.compile(r"(^kernel-e2e\s*->\s*)\S+", re.M),
             lambda m: m.group(1) + "tools/e2e_missing.py")
    elif name == "doc-entry-legacy":
        _sub(doc, re.compile(r"^(kernel-e2e-legacy\s*->\s*\S+)(.*)$", re.M),
             lambda m: m.group(1))
    elif name == "src-version":
        _sub(os.path.join(root, "Cargo.toml"),
             re.compile(r"(\[package\][\s\S]*?^version\s*=\s*\")[^\"]+", re.M),
             lambda m: m.group(1) + "9.9.9")
    elif name == "src-ci-step":
        _sub(os.path.join(root, CI_PATH), re.compile(r"(\n\s+run: python3 tools/host_tests\.py)"),
             lambda m: m.group(1) + "\n      - name: New gate\n        run: python3 tools/new_gate.py")
    elif name == "src-safety":
        _sub(os.path.join(root, SAFETY_PATH), re.compile(r"(critical = \[)",
             ), lambda m: m.group(1) + "\n    root/'src/net/x509.rs',")
    elif name == "src-cargo-trap":
        # The trap the version check must not fall into: the dependency version moves,
        # [package] does not — the doc must NOT be reported as stale.
        _sub(os.path.join(root, "Cargo.toml"), re.compile(r'(^limine = ")[^"]+', re.M),
             lambda m: m.group(1) + "9.9.9")
        rep = run_checks(root, quiet=True)
        if rep.failures:
            raise AssertionError("a dependency version bump was misread as a doc version drift: "
                                 f"{rep.failures[0]}")
        return
    elif name == "src-canon-example":
        with open(os.path.join(root, "tools/mini_repo.py"), "a", encoding="utf-8") as fh:
            fh.write("\n# Example: python tools/mini_repo.py serve 8000\n")
    elif name == "src-shebang":
        _sub(os.path.join(root, "tools/host_tests.py"), re.compile(r"^#!.*python3.*$", re.M),
             lambda m: "#!/usr/bin/env python")
    elif name == "doc-exception-stale":
        _touch(root, "docs/procfs.md")
    elif name == "src-untracked":
        # The scratch copy becomes a checkout with everything committed, then a documented
        # path appears WITHOUT being committed: exactly the tools/e2e.py situation.
        _git_init(root)
        _touch(root, "tools/unofficial.py")
        with open(doc, "a", encoding="utf-8") as fh:
            fh.write("\n- Helper: `tools/unofficial.py` (probe: present but uncommitted).\n")
        rep = run_checks(root, quiet=True)
        if not any("not tracked by git" in f for f in rep.failures):
            raise AssertionError("a present-but-uncommitted path went unreported: "
                                 + (rep.failures[0] if rep.failures else "no findings at all"))
        return
    else:
        raise AssertionError(f"unknown probe {name}")


def _sub(path: str, pattern: re.Pattern, repl, count: int = 1) -> None:
    """Apply a probe edit; `count=0` replaces every match.

    A pattern probe must remove ALL occurrences of what it breaks: the vendored patch uses
    `certificate_received` in several places, and replacing one left the claim true — a
    probe that cannot fail (`src-pattern-0` was caught doing exactly that).
    """
    text = _read(path)
    new, n = pattern.subn(repl, text, count=count)
    if n == 0:
        raise AssertionError(f"probe pattern {pattern.pattern!r} not found in {path}")
    with open(path, "w", encoding="utf-8") as fh:
        fh.write(new)


#: The claim text each probe puts back, `ABSENCE_CLAIMS` order. Entry #2 is written across
#: a line break on purpose — that wrapping is what a regex with literal spaces cannot see.
ABSENCE_PROBE_TEXT = (
    "- procfs does not exist (only emulated `/proc/self/exe` readlink).",
    "- repository metadata signatures\n  are still unverified.",
    "- embedded-tls deterministically hangs on large streams.",
)


def _land_contradiction(root: str, idx: int) -> None:
    """The evidence that makes absence claim `idx` (1-based) false."""
    if idx == 1:
        _touch(root, "src/vfs/procfs.rs")
    elif idx == 2:
        _touch(root, "src/pkg/openpgp.rs")
        _append(root, "src/pkg/apt.rs", "\n// openpgp::verify wires the trust chain\n")
    elif idx == 3:
        _append(root, "Cargo.toml", "\nlx_tlsbig = []\n")
        _append(root, "src/selftest_lx.rs", "// run_tls_big_check lives here\n")
    else:
        raise AssertionError(f"no contradiction defined for absence #{idx}")


def _append(root: str, rel: str, text: str) -> None:
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "a", encoding="utf-8") as fh:
        fh.write(text)


def _git_init(root: str) -> None:
    """Make a scratch copy a checkout with everything committed (probe support)."""
    opts = ["-c", "user.email=probe@example.invalid", "-c", "user.name=probe",
            "-c", "commit.gpgsign=false"]
    for args in (["init", "-q"], ["add", "-A"], ["commit", "-q", "-m", "probe scratch"]):
        out = subprocess.run(["git", *opts, *args], cwd=root, capture_output=True, text=True)
        if out.returncode != 0:
            raise AssertionError(f"probe git {' '.join(args)} failed: {out.stderr.strip()}")


def _touch(root: str, rel: str) -> None:
    path = os.path.join(root, rel)
    os.makedirs(os.path.dirname(path), exist_ok=True)
    with open(path, "w", encoding="utf-8") as fh:
        fh.write("// probe: pretend the feature landed\n")


#: Tracked content no check reads, copied as empty placeholders: `vendor/` alone is 140 MB
#: of the 145 MB tree and would make every probe a large copy. Existence — the only thing
#: the path checks ask about — stays exact.
PLACEHOLDER_DIRS = ("vendor/", "prebuilt/")


def copy_tree(root: str, dest: str) -> None:
    """A faithful copy of the *tracked* tree.

    An incomplete copy is worse than no probe: a missing path fails every mutation for the
    wrong reason, and the probe would 'prove' what it never tested. (The first version of
    this function copied a handful of top-level names and did exactly that.)
    """
    # Files a check READS (not merely asks about) are copied in full even under
    # PLACEHOLDER_DIRS: a 0-byte `connection.rs` would make the pattern claim fail in the
    # scratch copy for a reason that has nothing to do with the probe (`src-cargo-trap`
    # caught exactly that). Derived from the table, so it cannot drift from it.
    read_full = {claim["path"] for claim in PATTERN_CLAIMS}
    index = _git_index(root)
    if index is None:  # not a checkout: at least bring the names the checks read
        index = {"AGENTS.md", "Cargo.toml", "Cargo.lock", ".github/workflows/ci.yml",
                 "tools/check_safety.py", "build.sh", "run.sh", "run.cmd", "Makefile",
                 "src/vfs/procfs.rs", "docs/procfs.md", "tools/e2e.py",
                 "src/pkg/apt.rs", "src/pkg/openpgp.rs", "src/selftest_lx.rs"}
    for rel in sorted(index):
        src = os.path.join(root, rel)
        dst = os.path.join(dest, rel)
        os.makedirs(os.path.dirname(dst), exist_ok=True)
        if not os.path.exists(src):
            continue
        if (rel.startswith(PLACEHOLDER_DIRS) and rel not in read_full) or os.path.islink(src):
            if os.path.isdir(src) and not os.path.islink(src):
                continue
            try:
                with open(dst, "wb"):
                    pass  # placeholder: the checks only ask whether it exists
            except OSError:
                pass
            continue
        shutil.copy2(src, dst)


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    ap.add_argument("--root", default=os.path.dirname(os.path.dirname(os.path.abspath(__file__))))
    ap.add_argument("--probe", action="store_true",
                    help="also break each class in a temp copy and require the gate to fail")
    ap.add_argument("--quiet", action="store_true")
    args = ap.parse_args()
    rc = run_checks(args.root, args.quiet).emit()
    if args.probe:
        rc |= probe(args.root)
    return 1 if rc else 0


if __name__ == "__main__":
    sys.exit(main())
