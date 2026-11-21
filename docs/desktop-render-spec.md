# Pagh Desktop: CPU rendering performance spec (v1)

Status: implementation spec for agents, written 2026-11-16. Since then the tree moved
past it: wave A (PAT / write-combined `/dev/fb0`) and the core of waves B and C —
damage-only present, cached static layer, the precomposed shadow ring, client-side widget
damage, PDP/1 `ShmSurface` (types 14/15/16, `welcome.flags` bit 2) — are implemented in
`dev` 2.24.0. The steps below are the **plan**, not a checklist of what shipped: line
references and `/tmp/cpu-ui-refs/…` describe the tree at the time of writing, and the
crate READMEs (`pagh-desktop`, `pagh-ui`, `pagh-files`) state the current contract.
Scope: make the existing userspace
`pagh-desktop` compositor smooth at 1280×800 **without a GPU**, then leave a clean
path for shared-memory surfaces. Based on the patterns used by SerenityOS
WindowServer, LVGL and Redox Orbital (sources at the end).

## 0. Goals / non-goals

Goals:

- idle desktop must not redraw/present full frames;
- dragging/resizing a window at 1280×800 should be bounded by damage area, not
  by screen size;
- client pixels must not travel through AF_UNIX once ShmSurface is in use;
- framebuffer writes must be write-combined, not uncached per-pixel MMIO;
- wallpaper, panel, window decorations/shadows and glyphs are cached;
- all existing tests/screenshots/e2e stay green; fallback paths preserved.

Non-goals for this wave: GPU/DRM/KMS, OpenGL, hardware cursor, Wayland, video,
multi-GPU, compositor clustering. The CPU path must be good enough on its own.

## 1. Reference patterns (verified in source)

- SerenityOS WindowServer: `Compositor.cpp` keeps front/back/temp bitmaps, a
  `m_dirty_screen_rects` list, occlusion culling, cached wallpaper
  (`Compositor.cpp:168`, `:185`, `:312`, `:333`), and `compose()` returns early
  when `m_invalidated_any` is false (`:181`). `Painter::fill_rect` uses
  `fast_u32_fill` (`Painter.cpp:70`, `:101`), and `blit` only copies the requested
  source rectangle (`Painter.cpp:1050`).
- LVGL: `src/core/lv_refr.c` joins invalid areas (`lv_refr_join_area`), refreshes
  only invalid areas (`refr_invalid_areas`), clips to the layer, and flushes only
  the changed display area; `src/draw/lv_draw.c` splits drawing into clipped
  tasks/layers.
- Redox Orbital: `src/window.rs:318` `handle_window_sync` copies only the damage
  list into the backbuffer; title bitmaps are cached; comments describe the
  double-buffer/live-buffer split.
- Our current path: `/dev/fb0` is mapped `NO_CACHE | WRITE_THROUGH` in
  `src/arch/x86_64/linux/mem_sys.rs:599`; every compositor pixel write goes to
  uncached MMIO. PDP/1 `commit` uploads the full frame over AF_UNIX
  (`rust-apps/pagh-desktop/PROTOCOL.md` §5); `damage` is only a hint (§9).
  `pagh-desktop` already has a damage/full_render switch and a compositor frame
  buffer, so the work is to make damage and shared memory first-class.

  > **Status note (2026-11-21, when the spec was merged into the tree):** this paragraph
  > describes `dev` *before* waves A–C. Today PAT makes `/dev/fb0` leaves
  > write-combined (`src/memory/pat.rs`), the compositor presents damage-only, and
  > `ShmSurface` is the ordinary PDP/1 path — see `pagh-desktop/PROTOCOL.md` (ShmSurface)
  > and the crate READMEs. The sections below are kept as the plan that produced them.

## 2. Workstream A — kernel: write-combined `/dev/fb0`

Owner: kernel agent. Files: `src/arch/x86_64/linux/mem_sys.rs`,
`src/memory/vmm.rs`, `src/drivers/fb_abi.rs`, tests/docs.

### A1. PAT setup

1. Detect PAT via CPUID leaf 1, EDX bit 16. If absent, log once and keep the
   current `NO_CACHE|WRITE_THROUGH` mapping (fallback).
2. Program IA32_PAT (MSR 0x277) once during CPU init, preserving other entries:
   - entry 0 = WB (0x06), 1 = WT (0x04), 2 = UC- (0x07), 3 = UC (0x00),
     4 = WC (0x01), 5 = WC (0x01), 6/7 = UC- (0x07).
   - Write full 64-bit MSR; do not change CR0.CD.
3. Expose helper `memory::pat::wc_supported()` and `wc_pte_flags()`; pure flags
   are host-testable.

### A2. `/dev/fb0` mapping

Replace the fbdev PTE flags:

```text
PRESENT | WRITABLE | USER_ACCESSIBLE | MMIO_BIT | PAT_BIT(bit 7)
| NO_EXECUTE
```

with **no** `NO_CACHE`/`WRITE_THROUGH` when WC is supported. PAT bit is bit 7 of
a 4 KiB PTE (`PageTableFlags::BIT_7` or raw `1<<7`); `MMIO_BIT` (bit 52) remains
the software marker so `munmap`/`exit`/`fork` never return framebuffer frames to
PMM.

- `mprotect` on the fb region must preserve WC/MMIO flags; do not force
  `NO_CACHE|WRITE_THROUGH` back (see current `mem_sys.rs:1068`).
- `fork` copies only PTEs; the child keeps WC/MMIO; no COW.
- `mmap` remains `MAP_SHARED`-only; offset/length bounds unchanged.
- If PAT is absent, use exact current flags and log `fb0: PAT missing, uncached`.
- Do not change HHDM/kernel MMIO mappings globally; only user fb0 leaves.

### A3. Tests / evidence

- Host unit: PAT entry encoding, PTE flag composition, fallback composition.
- In-QEMU selftest: open `/dev/fb0`, mmap, verify `leaf_flags` contains PAT/WC
  (or fallback), write a 4 MiB pattern timed with `scheduler::ticks()`; check
  scanout pixels. QEMU may ignore cache attributes; correctness only there.
- Real-hardware evidence is required for the bandwidth claim; record ticks for
  `fill 4 MiB` and `copy 4 MiB` before/after if possible.
- No changes to `PROT_READ`/`PROT_WRITE` validation, `takeover`, or fork/exit.

## 3. Workstream B — compositor: back buffer, damage, caches, opaque fast path

Owner: server agent. Files: `rust-apps/pagh-desktop/src/{server,compositor,backend}.rs`,
`PROTOCOL.md`, tests/screenshots.

### B1. Double buffer and damage-only present

- Keep one compositor **back buffer** (the format/stride of the scanout). All
  window/panel/wp drawing goes there via normal cached memory.
- `Damage` = normalized screen rects. Operations:
  - `add(rect)` clip to screen; merge overlapping/touching rects; cap count
    (e.g. 64); if merged area > 70% of screen, collapse to one full-screen rect.
  - `is_empty()` -> `present()` returns immediately; do not touch `/dev/fb0`.
- `present(damage)`:
  - for each rect, per row copy `width*4` bytes from back buffer to the mapped
    scanout (`copy_from_slice`); no per-pixel `set_pixel` in the hot path;
  - 16/24/32 bpp handled by the backend with row conversion only for bpp<32;
  - `full_render`/`--e2e` may force full present; normal path must use damage.
- Mark damage from: window commit, move/resize, focus/title change, panel clock
  tick, cursor move, menu open/close, wallpaper/theme change.

### B2. Cached layers

Cache as `Surface`/`Bitmap` (RGBA or native) and reuse until invalidated:

- wallpaper (full screen; invalidate on size/wallpaper change);
- panel background + icons (invalidate on theme/clock/app list change; clock text
  gets its own small damage rect);
- per-window titlebar/decoration + shadow (invalidate on title/focus/size/theme;
  shadow is a 9-slice or precomputed bitmap, not recomputed per frame);
- glyph atlas for UI text (cache glyph bitmaps by (char, size, weight));
- menu/popup backgrounds (invalidate on text/selection change).

Never call `fill_rect`/alpha blending for a cached layer on every frame.

### B3. Opaque fast path / blit

- XRGB8888 window surfaces are opaque: copy rows directly; no alpha math.
- Alpha surfaces (shadows, menus, flyouts) blend only inside their damage rect;
  clip against the window/panel rect before per-pixel work.
- Use word-at-a-time fill/copy (`chunks_exact_mut(4)`, `copy_from_slice`) and
  avoid per-pixel branches; no allocation in the frame loop.
- Optional later: 2D `copy` for horizontal runs; do not add SIMD in this wave
  unless measured.

### B4. Frame scheduler

- Compose only when there is damage or a pending animation; coalesce multiple
  commits within one loop iteration.
- Cap at 60 Hz (`next_frame = now + 16ms`); no periodic 10 FPS full repaint.
- If idle and no damage for N frames, block on the socket/input poll instead of
  polling at 10 FPS.
- Cursor movement may be a separate small damage rect/overlay; don't recompose
  the whole screen for a mouse move.

### B5. ShmSurface protocol (PDP/1 extension)

Add without breaking old clients:

```
type 14 SHM_OFFER  c->s:
  u32 window_id, u32 width, u32 height, u32 format, u32 stride, u32 flags
  ancillary: exactly one SCM_RIGHTS fd (memfd, size >= stride*height)
type 15 SHM_READY  s->c:
  u32 window_id, u32 generation
type 16 SHM_RELEASE s->c / c->s:
  u32 window_id, u32 generation
type 5 COMMIT (shm form):
  u32 window_id, u32 generation, u32 damage_count, rect[damage_count]
  // no pixel bytes; flags bit 0 in header = SHM
type 9 DAMAGE:
  u32 window_id, u32 generation, u32 count, rect[count]
```

- `welcome.flags` bit 2 = ShmSurface supported.
- `create` remains; a client may create then send SHM_OFFER instead of frame
  commits. Server maps the fd `MAP_SHARED|PROT_READ` and validates
  format/stride/size. It does **not** scan out client memory directly; it copies
  damaged rects into its back buffer (clients are untrusted).
- On `configure` resize: client sends a new SHM_OFFER with a new generation; old
  fd/generation is released after the next commit. Server drops frames with a
  stale generation.
- Fallback: old clients keep full-frame `commit`; server supports both.
- Security: fd is received with SCM_RIGHTS (kernel support merged); reject
  wrong/multiple fds, short memfd, writable client? Client buffer read-only to
  server; no direct scanout mapping; bounds check all rects.

### B6. Tests / acceptance

- Golden screenshots remain byte-identical for current scenarios.
- Unit: damage merge/normalize/clamp/collapse; present rect coverage; cache
  invalidation; opaque vs alpha pixel equality.
- E2E: idle 2 s -> zero `present` calls/bytes; drag window -> present bytes
  approximately damage area, not full screen; move cursor -> small rect.
- Shm E2E: client writes pattern into memfd, sends damage, server screenshot has
  pattern; no pixel bytes on the wire; resize/reoffer works; old frame-client
  still works.
- Measure: `presents`, `bytes_presented`, `compose_ms`, `frame_ms` counters exposed
  in logs/selftest; no per-frame heap allocations (use counters).

## 4. Workstream C — clients: shared-memory surfaces and dirty tracking

Owner: clients agent. Files: `rust-apps/pagh-ui/**`, `pagh-files/**`,
`pagh-terminal/**`, their protocol code/tests.

### C1. Shared memory surface

- On create, allocate `memfd_create("pagh-ui-<pid>-<window>", 0)`; `ftruncate`
  to `stride*height`; `mmap(MAP_SHARED, PROT_READ|PROT_WRITE)`.
- Send `SHM_OFFER` with the fd via `sendmsg`/SCM_RIGHTS; wait for `SHM_READY`.
- Render UI directly into the mapping; no `Vec<u8>` frame copy.
- On configure resize: allocate new memfd/mapping, send a new generation,
  release old after ack; never resize an in-use mapping.
- If `welcome.flags` lacks bit 2 or SCM fails: fall back to the current full
  `commit` path (must remain tested).

### C2. Dirty rectangles

- Widgets return `Rect` damage; compose into a per-window `Damage` list, merge
  like the server, clip to surface; only send changed rects in `DAMAGE`/`COMMIT`.
- Full redraw allowed on: first frame, configure/resize, theme change, expose,
  popup open/close (or use popup damage).
- Cursor blink: damage only the cursor cell, not the whole terminal.
- Scrolling terminal/files: damage only the scrolled region if possible.
- Avoid allocations in the draw/commit loop; reuse buffers.

### C3. Tests

- Host unit: damage merge/clip; surface allocation/stride; fallback when SCM
  unsupported.
- E2E with real `pagh-desktop`: Files window, open menu, type, resize; screenshot
  equals expected; protocol trace has no pixel payload for shm commits.
- Benchmark: 600 frames of a moving window, report average compose/commit time;
  no regression vs frame-upload path when fallback is used.

## 5. Ordering, ownership, merge rules

1. Kernel WC (A) first: no protocol changes; can merge independently.
2. Server back buffer/damage/caches/opaque fast path (B1–B4) second: old clients
   keep working; reduces CPU immediately.
3. ShmSurface protocol + server (B5) and clients (C1–C2) after B1–B4, in
   lockstep: server PR and client PR target `dev`; server must keep frame-upload
   fallback until clients land.
4. Do not mix kernel/src edits with rust-apps edits in the same PR.
5. Each PR: base `dev`, PR-only, no version bump unless lead asks.

## 6. Acceptance metrics (release gate)

On real hardware, 1280×800, 60 Hz panel:

- idle desktop: 0 presents/s after settling; CPU < 2%;
- drag a 640×400 window: present bytes/frame <= 2× damaged area; frame time
  p95 < 16 ms; no full-screen memcpy;
- typing in terminal: only cursor/text rect damage; < 1 ms compose CPU;
- ShmSurface: 0 frame bytes on control socket; client commit is metadata only;
- old frame-upload clients still produce identical screenshots;
- no regression in `selftest`, `tools/e2e.py desktop`, host tests, safety/agents.

QEMU/TCG numbers are directional only; record them but do not use as the gate.

## 7. References

- SerenityOS: `Userland/Services/WindowServer/Compositor.cpp` (dirty rects,
  occlusion, cached wallpaper, early-out), `Userland/Libraries/LibGfx/Painter.cpp`
  (fast fill/blit).
  <https://github.com/SerenityOS/serenity>
- LVGL: `src/core/lv_refr.c` (invalid area join/clip/flush),
  `src/draw/lv_draw.c` (layers/tasks).
  <https://github.com/lvgl/lvgl>
- Redox Orbital: `src/window.rs` (damage/backbuffer, cached title bitmaps).
  <https://github.com/redox-os/orbital>
- Local copies: `/tmp/cpu-ui-refs/{serenity,lvgl,orbital}`.
- Our code: `src/arch/x86_64/linux/mem_sys.rs:544-615`,
  `rust-apps/pagh-desktop/PROTOCOL.md`, `rust-apps/pagh-desktop/src/server.rs`,
  `rust-apps/pagh-desktop/src/compositor.rs`.