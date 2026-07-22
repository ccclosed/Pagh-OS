# Features synchronized from the GUS experiment

The GUS comparison contained no hidden Python, Lua, upstream rustc or enhanced nano implementation. The actual GUS-only functional changes were ported back without branding:

- idempotent first-boot provisioning under `/mnt`;
- release metadata, user home and mini-Rust example;
- parent-directory Limine loader discovery;
- network apt enabled by default (already synchronized);
- persistent nano+ configuration and the previously promised editor improvements.

GUS names, prompt branding, banner and GPL distribution files were intentionally not copied into the MIT pagh kernel.
