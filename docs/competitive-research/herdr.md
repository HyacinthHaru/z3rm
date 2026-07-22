# Herdr Competitive Research

## Architecture Overview

Herdr is an emerging terminal multiplexer (Rust) with a distinctive architecture: **server owns all PTYs and streams rendered frames** (not raw PTY bytes) to clients, **Rust FFI binds Ghostty's Zig VT core** for terminal emulation, **PTY fd passing enables live session handoff**, and **explicit state/runtime separation**.

### Server-Owns-PTYs + Rendered-Frame Streaming
- **Server renders, client displays**: Herdr's server owns PTYs AND runs the terminal emulator. Server renders terminal state into a frame representation (grid cells, styles, image placements) and streams frames to clients. Clients never see raw PTY bytes. z3rm's mux-server (Plan 10) streams annotated grid cells, not raw bytes. This means clients are stateless renderers.
- **Bandwidth tradeoff**: Streaming rendered frames (grid diffs) vs. raw PTY bytes (let client emulate). Herdr picks rendered-frame: simpler clients, richer rendering control, less client compute. z3rm shares this model — mux-server owns terminal-view state; mux-client receives grid updates (Plan 10+13).
- **Latency tradeoff**: Server-side rendering adds one hop of latency (PTY→server→client vs. PTY→client direct). Herdr mitigates with rendering pipeline optimization. z3rm same tradeoff; mitigated by local rendering for local PTYs (LocalDomain short-circuits: client renders local PTYs directly).

### Rust FFI Binding Ghostty's Zig VT Core
- **FFI to libghostty**: Herdr uses Ghostty's libghostty C ABI (Zig core) via Rust FFI. Terminal emulation logic in Zig (fast, SIMD-optimal); integration in Rust via `unsafe` FFI. z3rm's terminal-view is pure Rust (vt100 crate) — no need for Zig FFI. However, the lesson is **pick proven-fast core** rather than reimplementing. z3rm should evaluate: is Rust vt100 crate fast enough, or should we consider FFI to a proven C/Zig core?
- **Unsafe boundary discipline**: All FFI wrapped in safe Rust API; no leaks across boundary; careful lifetime management. z3rm (if ever adding FFI) should follow this discipline.

### PTY fd Passing for Live Handoff
- **fd passing over Unix sockets**: Server can pass PTY file descriptors to clients or to other servers via `SCM_RIGHTS` ancillary data on Unix domain sockets. Enables: (1) **live session handoff** — migrate a running PTY from server A to server B without killing the child process. (2) **local fast path** — client can receive PTY fd directly for local sessions, bypassing server rendering pipeline. z3rm's mux-server (Plan 10) should support `SCM_RIGHTS` fd passing: local sessions can hand PTY fd directly to terminal-view in mux-client process, reducing latency.
- **Live migration**: When mux-server A wants to hand session to mux-server B (load balancing, A maintenance), A passes PTY fd to B via Unix socket. Child process keeps running. B resumes terminal emulation. z3rm should design for **session migration**: mux-server holds canonical state; PTY fd can move between server instances.

### Explicit State/Runtime Separation
- **State as serializable data; runtime as executor**: Herdr cleanly separates terminal state (grid, scrollback, cursor, styles — serializable) from runtime (PTY fd, event loop, channels — not serializable). Snapshotting = serialize state, drop runtime. Resume = deserialize state, re-create runtime (spawn PTY, reopen channels). z3rm should adopt **state/runtime split**:
  - Terminal state (TerminalViewState): grid, cursor, scrollback, styles — serializable, checksummable (for shadow snapshot, Plan 13).
  - Terminal runtime (TerminalView): PTY fd, event channels, render handles — runtime-only.
  - Shadow snapshot (Plan 13) serializes state; resume re-creates runtime.
- **Clean shadow snapshot**: State can be cloned cheaply (copy-on-write for grid). Runtime is single-owner. z3rm shadow snapshot = clone state, share runtime (PTY stays in mux-server).

## Lessons for z3rm

| Herdr Pattern | z3rm Adaptation |
|---------------|-----------------|
| Server owns PTYs + streams rendered frames | **mux-server owns terminal-view state; streams grid diffs to mux-client** (Plan 10+13) |
| Pre-rendered frame streaming (vs raw bytes) | **Grid-cell streaming** for remote; local-domain short-circuits to direct PTY + local rendering |
| Local fast-path for local sessions | **PTY fd passing to mux-client** via SCM_RIGHTS for local sessions (low latency) |
| PTY fd passing for live handoff | **Session migration**: PTY fd moves between mux-server instances via SCM_RIGHTS (Plan 10) |
| FFI to proven-fast core | **Evaluate**: pure-Rust vt100 vs FFI to proven core. z3rm defaults to pure-Rust; benchmark target ≥500MB/s |
| Explicit state/runtime split | **TerminalViewState (serializable)** + **TerminalView (runtime)** separation |
| Snapshot state, recreate runtime | **Shadow snapshot** (Plan 13): serialize state, re-create runtime on resume |
| Copy-on-write grid for cheap snapshot | **CoW grid** in TerminalViewState for cheap shadow snapshot |

## Key Patterns Observed
- **State/runtime split** is essential for: shadow snapshots, session migration, headless testing, replay/debug.
- **fd passing** is the mechanism for low-latency local sessions + live session migration between server instances.
- **Rendered-frame streaming** is simpler for clients at cost of one server-side render hop; mitigated for local sessions via fd passing (client renders directly).

## Competitive Positioning Note
Herdr demonstrates **state/runtime separation + fd passing** as core multiplexer primitives. z3rm should adopt both: TerminalViewState (serializable) vs. TerminalView (runtime); mux-server supports SCM_RIGHTS for local fast-path and session migration. The server-owns-render model (vs. raw-byte streaming) aligns with z3rm's mux-server→mux-client grid-diff streaming (Plan 10+13).