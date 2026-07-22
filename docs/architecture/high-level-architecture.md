# High-Level Architecture

z3rm is a terminal multiplexer built on GPUI (GPU-accelerated UI framework) with a client-server architecture. The mux server (`z3rm_mux`) manages terminal state, sessions, and grid layout. The client (`z3rm` binary) renders GPUI windows and communicates via gRPC.

```
┌─────────────────────────────────────────────────────────────────────┐
│                        z3rm Client (GPUI)                           │
│  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐              │
│  │  z3rm_chrome │  │ z3rm_editor  │  │ z3rm_terminal│              │
│  │   (tabs,     │  │  (editor,    │  │   (grid,     │              │
│  │   panes,     │  │   buffer,    │  │   cursor,    │              │
│  │   status)    │  │   syntax)    │  │   scrollback)│              │
│  └──────┬───────┘  └──────┬───────┘  └──────┬───────┘              │
│         │                 │                 │                       │
│         └─────────────────┼─────────────────┘                       │
│                           ▼                                         │
│              ┌────────────────────────┐                            │
│              │    z3rm_client (gRPC)  │                            │
│              └───────────┬────────────┘                            │
└───────────────────────────┼────────────────────────────────────────┘
                            │ gRPC (mux.proto)
                            ▼
┌─────────────────────────────────────────────────────────────────────┐
│                      z3rm_mux (mux server)                          │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐  ┌────────────┐  │
│  │  SessionMgr │  │   GridMgr   │  │   PtyMgr    │  │ ShadowMgr  │  │
│  │  (sessions, │  │  (grid,     │  │  (pty,      │  │ (snapshots,│  │
│  │   workspaces)│  │   panes,    │  │   processes)│  │  WAL, GC)  │  │
│  │             │  │   splits)   │  │             │  │            │  │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘  └─────┬──────┘  │
│         │                │                │               │         │
│         └────────────────┼────────────────┼───────────────┘         │
│                          ▼                ▼                         │
│                 ┌─────────────────────────────────┐                │
│                 │        z3rm_terminal (pty)      │                │
│                 │  (vt100 parser, grid, scrollback)│                │
│                 └─────────────────────────────────┘                │
└─────────────────────────────────────────────────────────────────────┘
```

## Crate Topology

```
z3rm/
├── z3rm_client/          # gRPC client stub + connection management
├── z3rm_mux/             # Mux server (session, grid, pty, shadow)
├── z3rm_terminal/        # Terminal emulation (vt100, grid, scrollback)
├── z3rm_shadow/          # Snapshot version tree (WAL, GC, branching)
├── z3rm_workspace/       # Workspace model (projects, files, settings)
├── z3rm_chrome/          # GPUI chrome (tabs, panes, status, command palette)
├── z3rm_editor/          # GPUI editor component (buffer, syntax, cursor)
├── z3rm_terminal_view/   # GPUI terminal view (grid renderer, input)
├── z3rm_extension_host/  # QuickJS extension host process
├── z3rm_extension_api/   # @z3rm/* built-in extension APIs
├── z3rm_commands/        # Core command registry (works without extensions)
├── z3rm_config/          # Configuration (TOML, schema, migration)
├── z3rm_ipc/             # Extension-host IPC (message passing)
├── z3rm_mux_proto/       # gRPC protobuf definitions
└── z3rm/                 # Main binary (entry point, GPUI app)
```

## Communication Flows

### Client → Server (gRPC)
- `CreateSession`, `AttachSession`, `DetachSession`
- `ResizeGrid`, `SplitPane`, `ClosePane`, `FocusPane`
- `WritePty` (stdin), `ReadPty` (stdout stream)
- `SnapshotCreate`, `SnapshotRestore`, `SnapshotBranch`

### Server → Client (gRPC streaming)
- `PtyOutput` stream (terminal output)
- `GridUpdate` stream (pane splits, focus, titles)
- `SessionEvent` stream (created, destroyed, renamed)
- `SnapshotEvent` stream (created, gc'd, branched)

### Extension Host ↔ Client (IPC)
- `ExtensionHost` spawns `z3rm_extension_host` subprocess
- JSON-RPC 2.0 over stdio
- `@z3rm/*` APIs exposed as imported modules

## Threading Model

- **Client (GPUI):** Single-threaded event loop (GPUI requirement). All GPUI work on main thread. gRPC client runs on dedicated tokio runtime.
- **Mux Server:** Multi-threaded tokio runtime. Single-writer `ShadowManager` actor. `PtyManager` per-pty tasks. `GridManager` single-threaded actor.
- **Extension Host:** Single-threaded QuickJS per extension. Host process multi-threaded (one thread per extension runtime).