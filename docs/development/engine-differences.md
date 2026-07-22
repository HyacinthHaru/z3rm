# Engine Differences from Zed

z3rm is a hard fork of Zed editor. This document catalogs the fundamental architectural differences.

## Guiding Philosophy

| Aspect | Zed | z3rm |
|--------|-----|------|
| Primary identity | Code editor | Terminal multiplexer |
| Architecture | Monolith + extensions | Client-server (mux) |
| Transport | Local process | gRPC (remote-capable) |
| Terminal | PTY-only (embed) | First-class grid + scrollback |
| Extension runtime | None (WASM planned) | QuickJS |
| Persistence | Filesystem | Broken sessions |
| Undo/redo | Buffer-level | State-level snapshots |

## Crate Map Comparison

### Removed (not applicable to mux)

| Zed Crate | Reason |
|-----------|--------|
| `collab`, `collab_ui`, `rpc` | Zed-only: cloud collaboration, remote editing |
| `language_tools` | LSP-specific UI (inline errors, hover popups) |
| `project`, `project_panel` | File/project browser (Zed workspace model) |
| `search` | File search, project search |
| `vim` | Vim mode (not in-scope) |
| `zed` | Zed main binary (replaced by `z3rm`) |

### Added (new to z3rm)

| z3rm Crate | Purpose |
|------------|---------|
| `z3rm_mux` | Mux server (session, grid, pty, shadow) |
| `z3rm_mux_proto` | gRPC protobuf definitions |
| `z3rm_client` | gRPC client stub |
| `z3rm_terminal` | Terminal emulation (vt100, grid, scrollback) |
| `z3rm_terminal_view` | GPUI terminal renderer |
| `z3rm_shadow` | Snapshot version tree (WAL, GC, branching) |
| `z3rm_extension_host` | QuickJS extension host |
| `z3rm_extension_api` | `@z3rm/*` built-in extension APIs |
| `z3rm_commands` | Core command registry |
| `z3rm_chrome` | GPUI chrome (tabs, panes, status bar) |
| `z3rm_ipc` | Extension-host IPC |

### Modified (Zed columns adapted)

| Zed Crate | z3rm Modification |
|-----------|-------------------|
| `gpui` | Retained as-is (cherry-picked upstream) |
| `editor` | Pruned to read-only viewer + diff |
| `workspace` | Pruned to session/workspace model |
| `settings` | Pruned to mux-specific settings |
| `theme` | Retained as-is (theme system) |
| `ui` | Retained (UI component library) |
| `terminal` | Moved to `z3rm_terminal` + `z3rm_terminal_view` |
| `terminal_view` | Moved to `z3rm_terminal_view` |
| `picker` | Retained (command palette) |

## Key Fork Differences

### Session Model (z3rm) vs File Model (Zed)

Zed operates on files in a project. z3rm operates on sessions: a session is a set of panes, each with a terminal PTY or editor view.

```
Zed:
  Project → Files → Editor tabs (file-backed)

z3rm:
  Session → Grid → Panes (terminal/editor), persisted via snapshots
```

### Extension Runtime

Zed has no extension runtime. Extensions are planned via WASM components (willow). z3rm uses QuickJS via `rquickjs` (see ADR-0004).

### Transport

Zed is local-only. z3rm is designed for remote: gRPC mux protocol (see ADR-0003, mux-protocol.md).

### Snapshots vs Git Integration

Zed uses Git for undo reconstruction. z3rm uses shadow snapshot version tree (see ADR-0006, shadow-snapshot-architecture.md).

## Upstream Cherry-Pick Strategy

See ADR-0001. In practice:

- **GPUI:** Cherry-pick on each GPUI point release. SDK is stable upstream; code is decoupled from other crates.
- **Theme:** Cherry-pick rarely (stable). Manual update if breaking change.
- **UI components:** Cherry-pick selectively (picker, command palette). Test each cherry-pick.
- **Editor:** Cherry-pick only for rendering fixes relevant to read-only viewer. Most editor changes are irrelevant (multi-cursor, inline completions, etc.).
- **Workspace:** Cherry-pick never (diverged completely).
- **Terminal:** Cherry-pick selectively (vt100 parser fixes). Grid model is diverged.