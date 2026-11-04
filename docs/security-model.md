# Security model: process credentials and filesystem permissions

**Status:** implemented in the `security-model` milestone. Defaults to root, so the
existing userland (shell, apt, CPython) keeps running unchanged; the new behaviour is
exercised by the in-QEMU selftest suite and host property P56.

This page is the contract. It states what is enforced, where, and what is deliberately
not modelled. Do not read it as a claim of sandboxing or multi-user security.

## 1. Credentials

`src/arch/x86_64/linux/cred.rs` (pure, `core`-only, host property P56) defines:

```rust
pub struct Creds { uid, gid, euid, egid, suid, sgid: u32 }
pub enum Access { Read, Write, Execute }
```

* `Creds::ROOT` is the identity of every kernel-spawned process.
* Credentials are stored in `CompatState::creds` (`src/task/compat.rs`), not in the VFS
  or the scheduler. `fork`/`clone` inherit the value; `execve` preserves it (setuid
  binaries are not modelled, so exec never elevates or drops ids). A pagh-native task
  with no `CompatState` reports `Creds::ROOT` to the syscall layer.
* Accessors used by syscalls: `compat::current_creds`, `set_current_creds`, `creds_of`.

### `may`: the POSIX matrix

`Creds::may(mode, (owner_uid, owner_gid), access)`:

1. effective ids select the class: `euid == owner_uid` → owner bits,
   else `egid == owner_gid` → group bits, else other bits;
2. a class grants the access iff its three bits contain the request;
3. **root** (`euid == 0`) bypasses read and write, but not execute: execute is granted
   iff at least one of the nine permission bits is an execute bit (`mode & 0o111 != 0`).

Only the low nine permission bits participate; type and setuid/setgid/sticky bits are
masked out. There are no supplementary groups and no `fsuid`: `euid`/`egid` are the only
identity a check sees.

### `set*id` transitions

`setuid`/`setgid`, `setreuid`/`setregid`, `setresuid`/`setresgid` are implemented in the
same pure module and follow the Linux ownership rules:

| call | root (`euid == 0`) | otherwise |
|---|---|---|
| `setuid(x)` | all three UIDs := x | x must be real or saved; `euid := x`, `suid := x`, real unchanged |
| `setgid(x)` | all three GIDs := x | x must be real or saved; same shape |
| `setreuid(r, e)` | whichever is not `-1` | `r ∈ {uid, suid}`, `e ∈ {uid, euid, suid}`; saved follows the new effective id when a transition was requested |
| `setresuid(r, e, s)` | whichever is not `-1` | every specified value ∈ `{uid, euid, suid}` |
| `setre[ug]id(-1, -1)` | no-op | no-op |

`setres[ug]id` never invents an identity for an unprivileged caller; `setuid`/`setgid`
never change the real id on the unprivileged path. A refused transition returns `EPERM`.
`getresuid`/`getresgid` validate all three user pointers before writing any of them.

One deliberate deviation from Linux: on the unprivileged `setuid`/`setgid` path pagh
also sets the saved id to the new effective id, so a process that drops root cannot
re-acquire it through the saved id. That is stricter than Linux (which leaves the saved
id alone) and is the rule this milestone asked for; it is not a compatibility promise
for setuid-root programs, which pagh does not support anyway.

## 2. Filesystem metadata

`VfsNode` (`src/vfs/mod.rs`) gained four **metadata-only** methods:

| method | default | ext2 |
|---|---|---|
| `mode() -> u32` | `0o755` dir / `0o644` file | `i_mode & 0o7777` |
| `owner() -> (u32, u32)` | `(0, 0)` | `(i_uid, i_gid)` |
| `set_mode(u32)` | `NotSupported` | journaled `i_mode` write, type bits preserved |
| `set_owner(u32, u32)` | `NotSupported` | journaled `i_uid`/`i_gid` write; values > `u16::MAX` are `EINVAL` |

The VFS never checks a credential. Boot provisioning, the package installer and the
shell call these methods directly, exactly as before. A `MountNode` forwards all four.

## 3. Enforcement points (syscall layer only)

All checks live in `src/arch/x86_64/linux/`; the VFS remains credential-free.

| operation | rule | errno |
|---|---|---|
| `open`/`openat` existing node | read for `O_RDONLY`; write for `O_WRONLY`/`O_RDWR`; both for `O_RDWR`; write additionally for `O_TRUNC` (checked before truncation) | `EACCES` |
| `open`/`openat` `O_CREAT` | write **and** execute on the parent directory | `EACCES` |
| `execve` | execute on the target; root still needs one x bit | `EACCES` |
| `unlink`/`rmdir`/`mkdir` | write+execute on the parent directory | `EACCES` |
| `rename`/`renameat`/`renameat2` | write+execute on the source **and** destination parents, both checked before any mutation | `EACCES` |
| `chmod`/`fchmod` | root or inode owner | `EPERM` |
| `chown`/`fchown` | root only; `(uid_t)-1` keeps the current id | `EPERM` |
| `kill` | sender euid is root, or equals the target's real or effective uid; a group/broadcast send delivers to the permitted subset | `EPERM` if none permitted |
| `tgkill` | same rule for the addressed thread | `EPERM` |

`chmod` writes the ext2 inode and also records the historical per-process stat override,
because `stat_mode_for` still prefers that override and the existing userland expects it
for untouched files. Synthetic nodes cannot persist metadata: their `set_mode` returns
`NotSupported` (the override still makes `stat` report the mode), and `chown` returns
`EPERM` instead of pretending to have stored an owner.

The package installer now applies each tar member's `mode` to the created ext2 inode.
Without that, every apt-installed program would be `0644`, and the execute check would
reject it even for root.

## 4. Not implemented (honest gaps)

* **No login/accounts.** The shell and all kernel-spawned processes are root; the model
  constrains only a process that calls the `set*id` family. There is no `/etc/passwd`
  authentication, no session, no `su`/`sudo`.
* **No setuid/setgid binaries or privilege elevation on exec.** `execve` preserves the
  credential record; an executable's mode does not change ids.
* **No supplementary groups, ACLs, `fsuid`/`fsgid`, capabilities, seccomp, LSM, audit or
  namespaces.** Owner/group/other + root bypass is the whole model.
* **No sticky-bit or directory-owner checks** on unlink/rename beyond the parent
  write+execute rule.
* **No `access(2)` enforcement** — it remains an existence probe that reports success.
* **`stat` reports the historical defaults** (`0755` files / `0700` dirs) unless the
  process recorded an override; `st_uid`/`st_gid` are not yet rendered. Permission
  decisions always use the real `VfsNode::mode`/`owner`.
* **SMEP only.** `CR4.SMEP` is set when CPUID leaf 7 EBX bit 7 reports support. SMAP
  (would require `stac`/`clac` around every user-memory access) and KPTI (would require a
  separate kernel page-table hierarchy) are not implemented.
* **`link(2)`/`symlink(2)` do not exist** in this syscall set, so the parent-directory
  rule is documented for the VFS operations the installer uses, not enforced through a
  missing syscall.
* **Images populated before this change** (including a local `disk.img`) store
  apt-installed programs as `0644`, because the old installer ignored the tar mode.
  `execve` of such a file now returns `EACCES` until the package is reinstalled (the
  installer now applies the archive mode) or the file is `chmod +x`. Kernel-internal
  launchers (`run_linux_binary`, the shell's `lxrun`, the `lx_selftest` harnesses) are
  not affected: they bypass the syscall check, exactly like the package installer
  bypasses the VFS checks.

## 5. Tests

* **Host property P56** (`host-tests/src/properties/p56.rs`): `may` against an
  independent oracle over arbitrary ids/modes, the exact root execute law, exhaustive
  class selection on a small id universe, and every `set*id` transition's ownership
  invariant. Runs via `python3 tools/host_tests.py`.
* **In-QEMU `src/test.rs`** (`security_tests`, five routines): the permission matrix,
  setuid transitions, chmod/chown rules plus ext2 mode/owner write-back, the kill
  permission rule, and a get/set*id syscall round-trip against a synthetic
  `CompatState` (root drops to 1000/100, then `setuid(0)`/`setgid(0)` are `EPERM`).
  They allocate no PMM frames net of the suite and the ext2 test uses the
  heap-backed mock and removes its file.
* `python3 tools/e2e.py selftest` prints `SELFTEST SUMMARY: 80 routines, 0 failed
  checks, 0 skipped` on this branch.

## 6. Review checklist for new syscalls

1. Add the number to `abi::nr`, the `dispatch_supported` arm and `SUPPORTED_SYSCALLS`
   (and its independent `EXPECTED` literal). The maintenance contract is in
   `src/arch/x86_64/linux/abi.rs`.
2. If it touches a path, decide which access it needs and call the same helpers
   (`check_node_access`, `check_dir_mutation`); do not add a credential check inside a
   `VfsNode` implementation.
3. If it changes credentials, go through a `Creds` transition method — never mutate the
   six ids field by field in the handler.
4. Add the pure decision to P56 (or a sibling property) and a deterministic in-QEMU
   routine when kernel state is involved.