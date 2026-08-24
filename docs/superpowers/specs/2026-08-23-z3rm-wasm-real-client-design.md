# Z3rm WebAssembly Client — Real GUI Port Design

Status: draft for maintainer review
Date: 2026-08-23

## Problem

The current `website/wasm/z3rm_demo` is a hand-built lookalike: a GPUI canvas
that renders hard-coded pane transcripts through `z3rm_web::WebTerminal`. It
is not the real z3rm interface. The requirement is that the browser demo be
the actual z3rm GUI — same workspace chrome, same panes — connected to a mux
server whose PTYs are backed by an in-browser Linux with a real shell.

## What already works on wasm32 (verified by cargo check)

- `gpui` + `gpui_web` (WebPlatform, wgpu rendering, keyboard/mouse)
- `gpui_wgpu`, `scheduler`
- `ui`, `theme` (compile clean)
- `z3rm_web` → vendored `alacritty_web` core (same rev as native)
- `mux_protocol` (wire types shared with native)

## What does not compile on wasm32 today, and why

| crate | blocker | resolution |
|---|---|---|
| `workspace` | `errno`/`async_fs` via fs/worktree deps | cfg-gate fs-backed worktree behind a trait; browser impl over OPFS |
| `terminal_view` | pulls `terminal` → `polling` (PTY loop) | swap `Terminal` for a wasm `TerminalSource` feeding the existing alacritty grid |
| `sidebar` | 21 errors, fs/tree deps | depends on the workspace gate above |
| `title_bar`/`tab_switcher` | untested; likely fine once workspace gates land | verify after |

## Architecture

```
browser
├── gpui_web (WebPlatform) ── renders the REAL workspace tree:
│     title_bar + sidebar + tab_switcher + status_bar + terminal_view panes
├── mux_client (in-process channel, not WebSocket)
├── mux_server (compiled to wasm)
│     ├── layout/lifecycle/grid_sync/dec2026: pure logic, compiles as-is
│     └── pane.rs: portable_pty::PtySystem trait → NEW WasmPtySystem
└── WasmPtySystem backend = in-wasm Linux userland
      option A: v86 (x86 Linux in wasm) — real busybox/shell, heavy (~2 MB gz)
      option B: Rust shelled userland (wash-style) — small, not a real kernel
      decision below
```

### Key seams

1. **PtySystem trait** (`pane.rs:739` already uses `dyn PtySystem`). Implement
   `WasmPtySystem` whose master side is a channel pair and whose child runs
   inside the embedded Linux. No changes to pane lifecycle logic.
2. **Workspace fs**: gate `Worktree` disk IO behind an existing-ish trait;
   browser build uses an in-memory/OPFS worktree. Editor buffers stay in RAM.
3. **Rendering parity**: because we render the real workspace entities through
   the same gpui element trees, chrome looks identical to PC. Theme comes from
   the default assets, loaded via gpui's asset source (fetch-based on web).

## Embedded Linux decision

**Choose v86** for the first increment despite size:

- it is a real Linux: busybox + real `/proc`, real process semantics, so
  `z3rm new/split-window/send-keys/capture-pane` behave exactly like native;
- the alternative (Rust toy shell) recreates "另起炉灶" — a fake — which this
  design exists to eliminate.

Size mitigation: v86 loads lazily after first paint; the demo URL keeps its
static fallback poster while the ~2 MB image streams. A `?lite=1` escape
hatch keeps CI fast.

## Milestones

1. **M1 — workspace compiles for wasm32**: cfg-gate fs/process deps; `cargo
   check -p workspace --target wasm32` green. No UI change.
2. **M2 — real chrome in the iframe**: open the actual workspace root view
   (title bar + sidebar + empty editor surface) under gpui_web; demo binary
   becomes a thin bootstrap that builds the same window the native app builds.
3. **M3 — WasmPtySystem**: implement the PtySystem trait against a channel
   backend; unit-test pane lifecycle against it natively.
4. **M4 — v86 Linux attached**: boot a minimal Linux image in a worker,
   bridge its serial to WasmPtySystem masters; `z3rm attach` shows a real
   shell running inside the browser.
5. **M5 — site integration**: replace `wasm/z3rm_demo` bootstrap with the
   real client; keep lazy-load and fallback poster; deploy via Pages.

## Non-goals

- Native desktop features: auto-update, telemetry, crash reporting, remote
  server dialing (the mux server IS local here), credentials providers.
- GPU-accelerated anything beyond what wgpu's GL backend gives on WebGL2.

## Risks

- v86 performance on low-end devices (mitigation: lazy load + lite mode).
- workspace crate has deep fs tentacles; M1 may uncover more gates than
  listed (timebox: if >40 call sites, introduce `BrowserFs` adapter first).
- gpui_web is single-threaded; mux_server event loop must be
  `background_spawn`-free or use worker threads via wasm_bindgen rayon only
  where wgpu allows.

## Verification per milestone

- M1/M2: `cargo check --target wasm32-unknown-unknown -p <crate>` plus the
  existing e2e suite staying green.
- M3: native unit tests of pane lifecycle against WasmPtySystem.
- M4: e2e types `ls` into the demo, asserts output appears in the real grid.
- M5: Pages deploy + Axe suite unchanged.

## References and inspiration

- Mayx, [在浏览器中运行 Linux 的各种方法](https://mabbs.github.io/2025/12/01/linux.html) —
  survey and implementation notes for browser-hosted Linux approaches.
- Mayx, [WASM Linux Terminal](https://mabbs.github.io/linux/) — terminal-first
  browser experience that inspired z3rm's v86-backed web demo and full-viewport
  terminal presentation.

These are design references rather than copied implementations; z3rm keeps its
own server-authoritative mux protocol, GPUI renderer, WasmPty seam, and v86
serial/9p bridge.
