# WezTerm Competitive Research

## Architecture Overview

WezTerm is a GPU-accelerated terminal emulator + multiplexer written in Rust. Key architectural pillars: **Domain trait abstraction** (local/remote share one interface), **notification bus** decoupling PTY I/O from rendering, **output coalescing + DEC-2026 sync**, **GPU glyph atlas + dirty-region tracking**.

### Domain Trait Abstraction (Local/Remote Unification)
- **`Domain` trait**: Single trait implemented by `LocalDomain` (local PTY), `SshDomain` (SSH), `TlsDomain` (TLS), `UnixDomain` (Unix socket), `WslDomain` (WSL). All multiplexing logic operates on `Arc<dyn Domain>`. z3rm's mux-protocol (Plan 9) + mux-server (Plan 10) should adopt: `MuxDomain` trait with `LocalDomain`, `RemoteDomain`, `TlsDomain` implementations sharing one mux-client/mux-server codepath.
- **Domain configuration via config file**: Domains declared in config (TOML); instantiated at startup. z3rm's config (Plan 16) should declare domains declaratively.

### Notification Bus Decouples PTY I/O from Rendering
- **`MuxNotification` bus**: PTY thread emits `MuxNotification::PtyOutput`, `MuxNotification::TitleChanged`, etc. Render thread subscribes. Zero coupling between PTY I/O and render loop. z3rm's mux-server (Plan 10) should use MPSC notification bus: PTY thread → `MuxNotification` → grid thread → client notification.
- **Cross-thread via `crossbeam-channel`**: Bounded channels with backpressure. z3rm should use `crossbeam-channel` bounded MPSC for all inter-thread notification.

### Output Coalescing + DEC-2026 Synchronized Output
- **Output coalescing**: PTY output batched per-frame (default 10ms) → reduces render calls. z3rm's PTY thread should coalesce PTY bytes per frame (target 60fps = 16ms budget).
- **DEC-2026 (Synchronized Output)**: `ESC [ ? 2026 h` / `ESC [ ? 2026 l` — terminal buffers output until matching disable, then flushes atomically. Prevents tearing during full-screen redraws (vim, htop). z3rm's terminal-view (vt100 parser) must implement DEC-2026; grid thread must honor synchronized-output flag.

### GPU Glyph Atlas + Dirty-Region Tracking
- **Glyph atlas**: Single GPU texture atlas packing all used glyphs (via `glyph_brush` / `cosmic-text`). Reused across frames. z3rm's terminal-view (GPUI) should use `cosmic-text` glyph atlas.
- **Dirty-region tracking**: Only damaged grid cells re-rendered. Grid tracks `damage_region: Rect` per frame. z3rm's grid (terminal-view) should track dirty rects; GPUI render only damaged quads.
- **Ligature/shaping via `cosmic-text` + `harfbuzz`**: Full shaping, ligatures, emoji, RTL. z3rm terminal-view uses `cosmic-text` (already dependency).

### Configuration: Lua + TOML
- **Lua config (`wezterm.lua`)**: Full Lua runtime for dynamic config (key bindings, dynamic domains, event callbacks). z3rm Plan 16 (settings) + Plan 14 (QuickJS) should adopt: TOML for static config, QuickJS for dynamic/scriptable config.

### Multiplexer: Tab/Pane/Window Model
- **Mux tab/pane/window**: Similar to tmux but GPU-rendered. Tabs = windows; panes = splits. z3rm workspace (Plan 15) mirrors this: workspace → tabs → panes.

## Lessons for z3rm

| WezTerm Pattern | z3rm Adaptation |
|-----------------|-----------------|
| Domain trait (local/remote unified) | **MuxDomain trait** (Plan 9/10): LocalDomain, RemoteDomain, TlsDomain |
| Notification bus (MPSC) | **MuxNotification bus** (Plan 10): PTY→Grid→Client |
| Output coalescing (10ms) | **PTY output coalescing per frame** (Plan 10: 16ms budget) |
| DEC-2026 sync output | **DEC-2026 in vt100 parser** (terminal-view crate) |
| GPU glyph atlas (cosmic-text) | **cosmic-text glyph atlas** (terminal-view + GPUI) |
| Dirty-region tracking | **Grid damage rects** (terminal-view grid) |
| Lua config + TOML | **TOML static + QuickJS dynamic** (Plan 14+16) |
| Lua keybindings/events | **QuickJS keymap/events** (Plan 17) |

## Key Source Files (WezTerm)
- `src/mux/domain.rs` — Domain trait + implementations
- `src/mux/notification.rs` — MuxNotification bus
- `src/mux/tab.rs` / `pane.rs` / `window.rs` — Mux hierarchy
- `src/term/terminal.rs` — Terminal emulator, DEC-2026, grid
- `src/gpu/atlas.rs` — Glyph atlas
- `src/config/configuration.rs` — Lua + TOML config