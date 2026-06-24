---
name: Bug report
about: Report a problem in the pagh kernel
title: "[bug] "
labels: bug
assignees: ''
---

## Description

<!-- A clear and concise description of the bug. -->

## Steps to reproduce

1.
2.
3.

## Expected behavior

<!-- What you expected to happen. -->

## Actual behavior

<!-- What actually happened. -->

## Environment

- Build mode: <!-- debug / release -->
- Nightly version: <!-- output of `rustc +nightly --version` -->
- QEMU version: <!-- output of `qemu-system-x86_64 --version` -->
- Host OS:

## Logs / output

<!--
Paste relevant serial output and/or a snippet of qemu_debug.log.
For a panic, include the stack trace (the kernel prints it by walking the RBP chain).
Use code fences (```) to keep formatting.
-->

```
(paste here)
```

## Additional context

<!-- Anything else: framebuffer screenshot, which subsystem (memory/fs/net/shell/...), etc. -->
