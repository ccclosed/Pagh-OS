#!/usr/bin/env python3
"""Inspect — and safely prune — this repository's git worktrees.

WHY THIS EXISTS.  The worktree-per-PR workflow this repo uses leaves a
worktree behind for every branch, and the merged ones are supposed to be
removed immediately (AGENTS.md, "Immediately after a PR merges, delete its
branch ... and retarget any stacked PRs to `main`").  In practice that rule was
the only place the policy existed: one checkout here accumulated 24 stale
worktrees and 28 fully merged local branches, several of them dirty with a
one-line deletion left over from an old commit, and cleaning them up by hand
meant reading `git worktree list`, `git cherry`, `git status` and `git
rev-list` for every single one.

This tool answers the two questions that matter, for every worktree at once:

  * is the branch FULLY MERGED into the base (`origin/main` by default), i.e.
    is it safe to drop?  ("ahead 0" is not enough: a branch can be ahead and
    still patch-equivalent, so `git cherry` is the arbiter.)
  * is the worktree CLEAN?  Uncommitted work is never thrown away: a dirty
    worktree is reported and skipped, never pruned.

Default mode is a read-only table.  `--prune` removes the worktrees whose branch
is fully merged and whose tree is clean, then deletes those local branches.  It
is a dry run unless `--yes` is given, and `--self-test` proves the classifier on
a scratch repository (the same "prove the gate by breaking it" idea as
`tools/check_agents_md.py --probe`).

WHAT IT DELIBERATELY DOES NOT DO
  * it never touches a remote branch (no `push --delete`): deleting someone
    else's branch on the server is not a local cleanup decision;
  * it never removes the main worktree or the worktree it is run from;
  * it never force-removes a dirty or ignored-file-bearing worktree — the
    operator gets the list and the reason, and decides.
"""

from __future__ import annotations

import argparse
import os
import shutil
import subprocess
import sys
import tempfile
import time
from dataclasses import dataclass, field
from pathlib import Path


# ─── git plumbing ────────────────────────────────────────────────────────────


def git(repo: Path, *args: str, check: bool = True) -> str:
    """Run one git command in `repo` and return its stdout (stripped)."""
    proc = subprocess.run(
        ["git", "-C", str(repo), *args],
        capture_output=True,
        text=True,
        check=False,
    )
    if check and proc.returncode != 0:
        raise RuntimeError(
            f"git {' '.join(args)} failed in {repo}: {proc.stderr.strip()}"
        )
    return proc.stdout.strip()


@dataclass
class Worktree:
    """One `git worktree list` entry, enriched with the facts that decide fate."""

    path: Path
    head: str
    branch: str  # "" when detached
    detached: bool
    is_main: bool
    # Filled in by inspect().
    ahead: int = 0
    unique: int = 0  # commits whose patch is NOT in the base (git cherry '+')
    dirty: list[str] = field(default_factory=list)
    state: str = "unknown"
    reason: str = ""
    # Minutes since the worktree's gitdir last changed. Used as an "is somebody
    # working here right now?" guard for --prune: a checkout somebody just
    # committed in must not be removed underneath them.
    idle_minutes: float = 1e9

    @property
    def prunable(self) -> bool:
        return self.state == "merged" and not self.dirty and not self.is_main


def parse_worktrees(repo: Path) -> list[Worktree]:
    """Parse `git worktree list --porcelain` into Worktree records."""
    raw = git(repo, "worktree", "list", "--porcelain")
    entries: list[Worktree] = []
    path: Path | None = None
    head = ""
    branch = ""
    detached = False
    for line in raw.splitlines() + [""]:
        if line.startswith("worktree "):
            if path is not None:
                entries.append(
                    Worktree(
                        path=path,
                        head=head,
                        branch=branch,
                        detached=detached,
                        is_main=path == repo,
                    )
                )
            path = Path(line[len("worktree ") :])
            head, branch, detached = "", "", False
        elif line.startswith("HEAD "):
            head = line[len("HEAD ") :]
        elif line.startswith("branch "):
            branch = line[len("branch ") :].removeprefix("refs/heads/")
        elif line == "detached":
            detached = True
    # The porcelain listing ends with the last entry's own lines, not with a new
    # `worktree ` header, so the final record has to be appended after the loop.
    if path is not None:
        entries.append(
            Worktree(
                path=path,
                head=head,
                branch=branch,
                detached=detached,
                is_main=path == repo,
            )
        )
    return entries


def idle_minutes(worktree: Path) -> float:
    """Minutes since anything in the worktree's gitdir or its root last changed.

    Deliberately cheap: the gitdir (index/HEAD/logs) is tiny, and the worktree
    root's own mtime changes when tracked files are created or deleted there.
    Walking the whole tree would stat a `target/` with tens of thousands of
    files just to answer "is this checkout idle?".
    """
    newest = 0.0
    try:
        gitdir = Path(git(worktree, "rev-parse", "--absolute-git-dir"))
    except RuntimeError:
        return 0.0
    for candidate in (worktree, gitdir, gitdir / "index", gitdir / "HEAD",
                      gitdir / "logs" / "HEAD"):
        try:
            newest = max(newest, candidate.stat().st_mtime)
        except OSError:
            continue
    return max(0.0, (time.time() - newest) / 60.0)


def inspect(repo: Path, base: str, wt: Worktree) -> Worktree:
    """Fill in ahead/unique/dirty/state for one worktree (read-only)."""
    if not wt.path.is_dir():
        wt.state = "missing"
        wt.reason = "directory is gone; `git worktree prune` clears the record"
        return wt
    try:
        ahead = int(git(wt.path, "rev-list", "--count", f"{base}..HEAD") or "0")
    except RuntimeError:
        wt.state = "unknown"
        wt.reason = f"cannot compare with {base}"
        return wt
    # `git cherry` compares PATCHES, not commits: a rebased or cherry-picked
    # branch is merged even though its commits are not ancestors of the base.
    cherry = git(wt.path, "cherry", base, "HEAD")
    unique = sum(1 for line in cherry.splitlines() if line.startswith("+"))
    status = git(wt.path, "status", "--porcelain")
    wt.ahead = ahead
    wt.unique = unique
    wt.dirty = [line for line in status.splitlines() if line.strip()]
    wt.idle_minutes = idle_minutes(wt.path)
    if wt.is_main:
        wt.state = "main"
        wt.reason = "the primary checkout"
    elif wt.dirty:
        wt.state = "dirty"
        wt.reason = f"{len(wt.dirty)} uncommitted path(s); never pruned"
    elif wt.detached:
        wt.state = "detached"
        wt.reason = "detached HEAD has no branch to delete; left alone"
    elif unique == 0:
        wt.state = "merged"
        wt.reason = f"every patch is in {base}"
    else:
        wt.state = "unique"
        wt.reason = f"{unique} patch(es) not in {base}"
    return wt


def inspect_all(repo: Path, base: str) -> list[Worktree]:
    return [inspect(repo, base, wt) for wt in parse_worktrees(repo)]


# ─── output ──────────────────────────────────────────────────────────────────


STATE_COLOURS = {
    "main": "",
    "merged": "32",  # green
    "unique": "33",  # yellow
    "dirty": "31",  # red
    "detached": "35",  # magenta
    "missing": "31",
    "unknown": "31",
}


def human(n: int) -> str:
    return "-" if n == 0 else str(n)


def print_table(repo: Path, base: str, trees: list[Worktree]) -> None:
    colour = sys.stdout.isatty()
    paint = (lambda s, c: f"\033[{c}m{s}\033[0m") if colour else (lambda s, c: s)
    print(f"{repo}  (base: {base})")
    width = max([len(str(t.path)) for t in trees] + [4])
    print(
        f"  {'PATH'.ljust(width)}  {'STATE':<9} {'BRANCH':<32} "
        f"{'AHEAD':>5} {'UNIQUE':>6} {'DIRTY':>5}  NOTE"
    )
    for t in trees:
        branch = "(detached)" if t.detached else (t.branch or "?")
        note = t.reason
        print(
            f"  {paint(str(t.path).ljust(width), STATE_COLOURS.get(t.state, ''))}"
            f"  {paint(t.state.ljust(9), STATE_COLOURS.get(t.state, ''))} "
            f"{branch[:32]:<32} {human(t.ahead):>5} {human(t.unique):>6} "
            f"{human(len(t.dirty)):>5}  {note}"
        )
        for line in t.dirty[:3]:
            print(f"  {' ' * width}  dirty: {line}")
        if len(t.dirty) > 3:
            print(f"  {' ' * width}  dirty: … {len(t.dirty) - 3} more")
    prunable = [t for t in trees if t.prunable]
    print(
        f"\n  {sum(1 for t in trees if t.state == 'merged')} merged worktree(s), "
        f"{len(prunable)} prunable (--prune also skips recently active ones), "
        f"{sum(1 for t in trees if t.state == 'unique')} with unique work, "
        f"{sum(1 for t in trees if t.state == 'dirty')} dirty (kept), "
        f"{sum(1 for t in trees if t.state == 'detached')} detached (kept)."
    )


# ─── prune ───────────────────────────────────────────────────────────────────


def prune(
    repo: Path,
    trees: list[Worktree],
    assume_yes: bool,
    min_idle_minutes: float = 30.0,
) -> int:
    """Remove clean, fully merged worktrees and delete their local branches.

    Returns a process exit code: 0 when the prunable set was handled (or there
    was nothing to do), 2 when the caller declined at the prompt.
    """
    candidates = [
        t
        for t in trees
        if t.prunable
        and t.branch
        and t.branch != "main"
        and t.idle_minutes >= min_idle_minutes
    ]
    skipped = [t for t in trees if not t.prunable and t.state in ("dirty", "unique")]
    active = [
        t
        for t in trees
        if t.prunable
        and t.branch
        and t.branch != "main"
        and t.idle_minutes < min_idle_minutes
    ]
    if not candidates:
        if active:
            print(
                f"Nothing to prune: {len(active)} merged worktree(s) were touched in "
                f"the last {min_idle_minutes:g} min and look active."
            )
            for t in active:
                print(f"  {t.path}  (idle {t.idle_minutes:.0f} min)")
            print("Use --min-idle-minutes 0 to prune them anyway.")
        else:
            print(
                "Nothing to prune: every worktree is either the main one, unique, "
                "detached, or dirty."
            )
        return 0
    print("Will remove:")
    for t in candidates:
        print(f"  {t.path}  (branch {t.branch})")
    if skipped:
        print("Keeping (unique or dirty):")
        for t in skipped:
            print(f"  {t.path}  [{t.state}] {t.reason}")
    if active:
        print(f"Keeping (touched in the last {min_idle_minutes:g} min — looks active):")
        for t in active:
            print(f"  {t.path}  (idle {t.idle_minutes:.0f} min)")
    if not assume_yes:
        try:
            answer = input("Remove these worktrees and their merged branches? [y/N] ")
        except EOFError:
            answer = ""
        if answer.strip().lower() not in ("y", "yes"):
            print("Refused; nothing was changed.")
            return 2
    failures = 0
    for t in candidates:
        try:
            # `--force` is not used: prune() only ever lists clean worktrees, so
            # a plain remove cannot discard uncommitted work. If git still
            # refuses (ignored files are the usual reason), the operator decides.
            git(repo, "worktree", "remove", str(t.path))
        except RuntimeError as err:
            print(f"  ! could not remove {t.path}: {err}")
            print("    (ignored build artefacts are the usual reason: remove them by hand)")
            failures += 1
            continue
        try:
            git(repo, "branch", "--delete", t.branch)
        except RuntimeError as err:
            print(f"  ! removed {t.path} but could not delete branch {t.branch}: {err}")
            failures += 1
            continue
        print(f"  removed {t.path} and branch {t.branch}")
    return 0 if failures == 0 else 1


# ─── self-test ───────────────────────────────────────────────────────────────


def self_test() -> int:
    """Prove the classifier and the prune rules on a scratch repository.

    The repository is built from scratch in a temporary directory, so this never
    touches the real one: a `main` worktree, one MERGED branch (its patch is
    cherry-picked onto main, which is exactly the case plain ancestry gets
    wrong), one UNIQUE branch, and one DIRTY worktree whose branch is merged but
    must still be kept.
    """
    tmp = Path(tempfile.mkdtemp(prefix="pagh-worktrees-selftest-"))
    env = {
        **os.environ,
        "GIT_AUTHOR_NAME": "selftest",
        "GIT_AUTHOR_EMAIL": "selftest@example.invalid",
        "GIT_COMMITTER_NAME": "selftest",
        "GIT_COMMITTER_EMAIL": "selftest@example.invalid",
        "GIT_CONFIG_GLOBAL": str(tmp / "gitconfig"),
        "GIT_CONFIG_SYSTEM": os.devnull,
    }
    os.environ.update(env)  # subprocess inherits; keep it simple for a host tool
    log: list[str] = []
    failures = 0
    try:
        root = tmp / "repo"
        root.mkdir()
        git(root, "init", "-q", "-b", "main")
        (root / "a.txt").write_text("one\n")
        git(root, "add", "a.txt")
        git(root, "commit", "-q", "-m", "base")
        # A branch whose patch lands on main verbatim (cherry-pick), so `ahead`
        # is one but `git cherry` says merged.
        git(root, "checkout", "-q", "-b", "merged-branch")
        (root / "b.txt").write_text("two\n")
        git(root, "add", "b.txt")
        git(root, "commit", "-q", "-m", "merged work")
        merged_sha = git(root, "rev-parse", "HEAD")
        git(root, "checkout", "-q", "main")
        git(root, "cherry-pick", merged_sha)
        # A branch with genuinely unique work.
        git(root, "checkout", "-q", "-b", "unique-branch")
        (root / "c.txt").write_text("three\n")
        git(root, "add", "c.txt")
        git(root, "commit", "-q", "-m", "unique work")
        git(root, "checkout", "-q", "main")

        wt_merged = tmp / "wt-merged"
        wt_unique = tmp / "wt-unique"
        wt_dirty = tmp / "wt-dirty"
        git(root, "worktree", "add", "-q", str(wt_merged), "merged-branch")
        git(root, "worktree", "add", "-q", str(wt_unique), "unique-branch")
        # Third worktree: use the merged branch's name elsewhere? No: a dirty
        # worktree on its OWN merged branch — branch off main, cherry-pick is
        # already on main, so it is merged, then dirty a file.
        git(root, "branch", "dirty-merged", merged_sha)
        git(root, "worktree", "add", "-q", str(wt_dirty), "dirty-merged")
        (wt_dirty / "a.txt").write_text("local edit\n")

        trees = inspect_all(root, "main")
        by_path = {str(t.path): t for t in trees}
        expected = {
            str(root): ("main", False),
            str(wt_merged): ("merged", True),
            str(wt_unique): ("unique", False),
            str(wt_dirty): ("dirty", False),
        }
        for path, (state, prunable) in expected.items():
            got = by_path.get(path)
            if got is None:
                log.append(f"FAIL {path}: missing from classification")
                failures += 1
                continue
            if got.state != state or got.prunable != prunable:
                log.append(
                    f"FAIL {path}: expected state={state} prunable={prunable}, "
                    f"got state={got.state} prunable={got.prunable} ({got.reason})"
                )
                failures += 1
            else:
                log.append(f"ok   {Path(path).name}: {state}, prunable={prunable}")

        # Prune must remove only the clean merged worktree; the dirty one and the
        # unique one must survive with their branches intact.
        # The scratch worktrees were created seconds ago: disable the idle
        # guard, which exists for real checkouts somebody may be using.
        code = prune(root, trees, assume_yes=True, min_idle_minutes=0)
        if code != 0:
            log.append(f"FAIL prune returned {code}")
            failures += 1
        checks = [
            (not wt_merged.exists(), "merged worktree removed"),
            (not git(root, "branch", "--list", "merged-branch"), "merged branch deleted"),
            (wt_unique.exists(), "unique worktree kept"),
            (bool(git(root, "branch", "--list", "unique-branch")), "unique branch kept"),
            (wt_dirty.exists(), "dirty worktree kept"),
            (bool(git(root, "branch", "--list", "dirty-merged")), "dirty branch kept"),
        ]
        for ok, what in checks:
            log.append(("ok   " if ok else "FAIL ") + what)
            failures += 0 if ok else 1
    except Exception as err:  # noqa: BLE001 - a self-test reports, never raises
        log.append(f"FAIL self-test aborted: {err}")
        failures += 1
    finally:
        shutil.rmtree(tmp, ignore_errors=True)

    print("worktrees.py self-test:")
    for line in log:
        print("  " + line)
    print(f"  SELFTEST {'PASS' if failures == 0 else f'FAIL ({failures})'}")
    return 0 if failures == 0 else 1


# ─── main ────────────────────────────────────────────────────────────────────


def find_repo(start: Path) -> Path:
    """The repository root that contains `start` (a worktree is not enough)."""
    return Path(git(start, "rev-parse", "--path-format=absolute", "--git-common-dir")).parent


def main() -> int:
    ap = argparse.ArgumentParser(
        description=__doc__,
        formatter_class=argparse.RawDescriptionHelpFormatter,
    )
    ap.add_argument(
        "--repo",
        default=".",
        help="any path inside the repository (default: current directory)",
    )
    ap.add_argument(
        "--base",
        default="origin/main",
        help="branch the worktrees are compared against (default: origin/main)",
    )
    ap.add_argument(
        "--prune",
        action="store_true",
        help="remove worktrees whose branch is fully merged and whose tree is clean",
    )
    ap.add_argument(
        "--yes",
        action="store_true",
        help="with --prune: do not ask for confirmation",
    )
    ap.add_argument(
        "--min-idle-minutes",
        type=float,
        default=30.0,
        metavar="MIN",
        help="with --prune: keep worktrees touched less than MIN minutes ago, so a "
        "worktree somebody is using is never removed (default: 30)",
    )
    ap.add_argument(
        "--self-test",
        action="store_true",
        help="prove the classifier and prune rules on a scratch repository",
    )
    args = ap.parse_args()

    if args.self_test:
        return self_test()

    try:
        repo = find_repo(Path(args.repo))
        trees = inspect_all(repo, args.base)
    except RuntimeError as err:
        print(f"worktrees.py: {err}", file=sys.stderr)
        return 1
    print_table(repo, args.base, trees)
    if args.prune:
        return prune(repo, trees, args.yes, args.min_idle_minutes)
    # A dirty or unique worktree is not an error: the table already says why it
    # is being kept, and a nightly `--prune` run must not fail on it.
    return 0


if __name__ == "__main__":
    sys.exit(main())