# Kitty Competitive Research

## Architecture Overview

Kitty is a GPU-accelerated terminal emulator written in Python+C. Key innovations: **Kitty graphics protocol** (inline images), **capability-scoped permission model**, **versioned JSON wire protocol**, **pluggable overlay layouts**, **transmit-image-once/place-many**.

### Kitty Graphics Protocol (Inline Images)
- **Inline image protocol**: `ESC ] 1337 ; File=... \` (iTerm2-compatible) + Kitty-extended `ESC _ G` control sequence with base64 chunked transfer. Images placed at cursor position in grid; stored in scrollback; re-rendered on scroll. z3rm terminal-view (vt100) must implement **Kitty graphics protocol** (Plan 29: kitty-graphics). Images stored in grid cells as image refs (not raw bytes); GPU texture cache holds uploaded image textures.
- **Image placement spec**: Kitty supports per-cell image placement with arbitrary offset, width/height, cell anchoring. Images can span multiple rows/cols. z3rm terminal-view grid needs **image cell type** alongside text cell type; image cells reference GPU texture + placement rect.
- **Transmit-once, place-many**: Image transmitted once (gets an image ID), then placed multiple times via `ESC _ G ; a=T,f=100,i=<ID>`. Refcount shared; placement cheap. z3rm should implement this: **image cache keyed by ID**; placement emits textured quad referencing cached texture.

### Capability-Scoped Permission Model
- **Permission prompts via OSC**: Kitty prompts user for sensitive operations (reading clipboard via OSC 52, remote-control commands). User grants/denies per-request or per-session or persistently. z3rm extensions (Plan 14) + permission system (Plan 33) should adopt **capability-grant matrix**: per-extension capability declarations; per-capability grant (single-use, session-persist, always). Grant UI integrated with terminal overlay.
- **Remote control protocol capability**: Kitty remote control (TCP/socket) can be scoped to local-only, password-protected, or disabled. z3rm mux-client→mux-server protocol should support **per-action permission prompts** for destructive operations (kill session, send Ctrl+C).

### Versioned JSON Wire Protocol
- **Kitty remote control protocol**: Kitty exposes JSON wire protocol over Unix socket / TCP. Each message has `version` field. `kitten @ ls`, `kitten @ launch`, `kitten @ close-window` — scriptable CLI. z3rm's CLI control interface (Plan 27) should adopt **JSON wire protocol** with version field per request. Supports both interactive (CLI) and programmatic (HTTP) interfaces.
- **Scriptable via `kitten @`**: Every kitty action scriptable via `kitten @ <command>`. z3rm's `z3rm` CLI (Plan 27) should expose every session/window/pane action scriptable.

### Pluggable Overlay Layouts
- **Overlays**: Kitty supports overlay windows (popups, search, command palette) with custom layouts. `kitten @ launch --type=overlay` spawns overlay. Overlay layouts plugin-like. z3rm's extension system (Plan 14) should support **overlay panes**: extensions can spawn overlay windows (popups, pickers, modals) with declarative layout.

### Advanced Features
- **Tabs/windows/OS windows**: Multiple tabs → windows → OS windows (separate native windows). z3rm workspace model supports tabs + panes; multiple OS windows via Plan 32 (multiple-windows).
- **Shaders**: Custom background shaders. z3rm GPUI supports custom shaders.
- **Ligatures + Unicode 14**: Full shaping via custom font code. z3rm uses cosmic-text + harfbuzz.
- **Synchronized output (DEC-2026)**: Supported. z3rm terminal-view must implement.
- **kitten utility framework**: `kitten` CLI for sub-commands (icat, ssh, hyperlinked_grep, etc.). z3rm CLI (Plan 27) should support subcommand + plugin architecture.

## Lessons for z3rm

| Kitty Pattern | z3rm Adaptation |
|-----------------|-----------------|
| Kitty graphics protocol (inline images) | **Kitty graphics protocol** in terminal-view vt100 (Plan 29) |
| Transmit-once/place-many image cache | **Image cache keyed by ID**; textured-quad placement in GPUI |
| Capability-scoped permissions | **Capability grant matrix** (Plan 33): per-extension, per-capability grant |
| Permission prompts via overlay | **Grant UI overlay** for sensitive ops (OSC 52 clipboard, kill-session) |
| Versioned JSON wire protocol | **JSON wire protocol** with version field (Plan 27: CLI control interface) |
| Scriptable via `kitten @` CLI | **Every action scriptable** via `z3rm` CLI subcommands |
| Pluggable overlay layouts | **Extension overlay panes** (popups, pickers, modals) with declarative layout |
| Multi-OS-window support | **Multiple OS windows** (Plan 32) |
| Synchronized output (DEC-2026) | Implement in terminal-view vt100 |

## Key Source Files (Kitty)
- `kitty/child.py` / `kitty/child-monitor.c` — PTY + VT parsing (C extension for speed)
- `kitty/graphics.py` — Kitty graphics protocol parser, image cache
- `kitty/rc/base.py` — Remote control JSON protocol (versioned)
- `kitty/tabs.py` — Tab/window management
- `kitty/layout.py` — Pluggable layouts (stack, grid, vertical, horizontal)
- `kitty/permissions.py` — Capability permission prompts
- `kitty/options/definition.py` — Config option declaration

## Competitive Positioning Note
Kitty validates **rich image support + capability permissions**. z3rm terminal-view should be image-protocol complete (Kitty graphics + iTerm2 fallback), and z3rm extension/permission system should adopt Kitty's capability-grant matrix. Scriptable JSON wire protocol (Plan 27) mirrors Kitty's `kitten @` success—every action scriptable.