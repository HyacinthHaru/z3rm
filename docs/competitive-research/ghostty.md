# Ghostty Competitive Research

## Architecture Overview

Ghostty is a GPU-accelerated terminal emulator written in Zig. Key architectural pillars: **per-surface thread separation** (read/parse/render), **compile-time SIMD VT parsing**, **library-first design** (libghostty C ABI), **glyph atlas + textured-quad compositing**.

### Per-Surface Read/Parse/Render Thread Separation
- **Three threads per terminal surface**: (1) **Read thread** reads raw bytes from PTY into ring buffer. (2) **Parse thread** consumes ring buffer, runs VT parser, mutates terminal state (grid). (3) **Render thread** reads grid state, renders via GPU. Threads communicate via atomics + lightweight synchronization. z3rm's mux-server (Plan 10) should adopt **per-pane thread separation**: PTY read thread (reads bytes), VT parse thread (mutates grid), notification thread (notifies clients). Terminal-view (GPUI) render runs on GPUI render thread.
- **No mutex on hot paths**: Grid has double-buffer or copy-on-write snapshot. Parse thread mutates; render reads snapshot. z3rm grid should use **snapshot copy** for rendering: grid thread maintains canonical grid; render thread reads immutable snapshot of last-frame grid.

### Compile-Time SIMD VT Parser
- **SIMD-accelerated VT parsing at compile time**: Ghostty generates optimized parser tables using Zig comptime (compile-time codegen). Parser processes bytes in SIMD-width chunks (e.g., 16 bytes/iteration for SSE). z3rm's vt100 crate should use **compile-time-generated parser tables** (Rust const generics or `build.rs` codegen) + **SIMD byte scanning** (std::simd or portable-simd feature gate).
- **State machine at speed**: VT parser is a state machine (C0 control, CSI, OSC, DCS). Ghostty's parser runs at >1GB/s throughput. z3rm vt100 parser should benchmark with criterion.criterion; target ≥500MB/s for competitive parsing speed.

### Library-First Design (libghostty C ABI)
- **libghostty C ABI**: Core terminal emulation logic exported as C ABI (`libghostty.h`, `libghostty_api.h`). Ghostty itself is a thin CI/UI shell around the library. This enables: embedding Ghostty core in other apps, language bindings (Python, Rust, C++), headless testing. z3rm's terminal-view crate should expose **Rust-native API** (no C ABI needed since z3rm is Rust-only) but maintain **library-first separation** so terminal-view is usable standalone (headless testing, embedding in other Rust apps).
- **Headless mode**: libghostty supports headless mode (no GUI, pure terminal emulation). z3rm terminal-view should support **headless mode** for testing + server-side terminal emulation (mux-server panes run terminal-view headless).

### Glyph Atlas + Textured-Quad Compositing
- **Glyph atlas**: Single GPU texture packing all glyphs. Ghostty uses a custom glyph cache (not fontdb/cosmic-text directly—lighter). z3rm should use **cosmic-text glyph atlas** (already dependency).
- **Textured-quad compositing**: Each glyph cell = one textured quad. GPU draws quads with atlas UVs. Dirty-region tracking: only changed quads re-uploaded per frame. z3rm terminal-view (GPUI) should use **scaled quads with glyph atlas** per cell; track dirty grid cells; only re-render damaged quads.

### Advanced Features
- **Kitty graphics protocol**: Full inline image support (see Kitty research).
- **iTerm2 image protocol**: Inline image support.
- **Advanced shaders**: Custom fragment shaders for visuals (blur, transparency). z3rm GPUI supports shaders.
- **`ghostty +list-themes` / `ghostty +show-config`**: CLI for introspecting config. z3rm should have CLI config introspection (Plan 27: CLI control interface).

## Lessons for z3rm

| Ghostty Pattern | z3rm Adaptation |
|-----------------|-----------------|
| Per-surface read/parse/render threads | **Per-pane threads** (Plan 10): PTY-read, VT-parse, client-notify; render on GPUI thread |
| Compile-time SIMD VT parser | **Comptime-generated parser + SIMD byte scanning** (vt100 crate); benchmark ≥500MB/s |
| Library-first (C ABI) | **Library-first separation** (terminal-view crate): standalone, embeddable, headless-testable |
| Headless mode | **Headless terminal-view** for mux-server panes + tests |
| Glyph atlas + textured quads | **cosmic-text atlas + GPUI quads** (terminal-view) |
| Dirty-region tracking | **Grid dirty rects** (terminal-view) |
| Custom shaders | **GPUI fragment shaders** for terminal visuals |
| CLI introspection | **CLI config commands** (Plan 27) |

## Key Source Files (Ghostty)
- `src/Surface.zig` — Per-surface thread separation, read/parse/render
- `src/Parser.zig` — Compile-time SIMD VT parser
- `src/Terminal.zig` — Terminal emulator, grid, scrollback
- `src/lib/App.zig` — libghostty C ABI entry points
- `src/lib/font/load.zig` — Glyph atlas + font loading
- `src/Renderer.zig` — Textured-quad GPU compositing
- `src/simdisms.zig` — SIMD byte scanning primitives

## Competitive Positioning Note
Ghostty validates **library-first terminal core** as a design. z3rm's terminal-view crate should be usable in three contexts: (1) GPUI render (z3rm UI), (2) headless (mux-server panes), (3) standalone (tests, embeddings). The vt100 parser should match Ghostty's throughput via compile-time parser table codegen + SIMD byte scanning.