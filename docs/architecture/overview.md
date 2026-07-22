# z3rm Architecture Overview

z3rm is a terminal multiplexer forked from the Zed editor, built around a **server-canonical terminal state** architecture with a **GPUI-based GUI client** and a **mux_server daemon** communicating over a framed binary protocol over Unix sockets (or SSH tunnels).

## High-Level Components

```
┌─────────────────────────────────────────────────────────────────┐
│                        z3rm (GUI Client)                        │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐              │
│  │   GPUI      │  │  MuxDomain  │  │ Extensions  │              │
│  │  Renderer   │◄─┤  (Client)   │◄─┤  (QuickJS)  │              │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘              │
└─────────┼────────────────┼────────────────┼─────────────────────┘
          │                │                │
          │  Unix Socket / │                │
          │  SSH Tunnel    │                │
          ▼                ▼                ▼
┌─────────────────────────────────────────────────────────────────┐
│                      mux_server (Daemon)                        │
│  ┌─────────────┐  ┌─────────────┐  ┌─────────────┐              │
│  │   Session   │  │   Pane/     │  │  Shadow     │              │
│  │   Manager   │  │   Grid Sync │  │  Snapshot   │              │
│  └──────┬──────┘  └──────┬──────┘  └──────┬──────┘              │
│         │                │                │                     │
│         ▼                ▼                ▼                     │
│  ┌─────────────────────────────────────────────────┐           │
│  │              alacritty_terminal + PTY           │           │
│  └─────────────────────────────────────────────────┘           │
└─────────────────────────────────────────────────────────────────┘
```

## Core Architectural Principles

### 1. Server-Canonical Terminal State (§3.1)
- **Single source of truth**: `mux_server` owns all PTY file descriptors, `alacritty_terminal` instances, scrollback buffers, and grid state.
- **Clients are thin viewers**: The GPUI client renders grid diffs pushed by the server; it never manipulates PTY state directly.
- **Crash isolation**: Client crashes don't affect running sessions; server restart recovers state from shadow snapshots.

### 2. Single Data Path — Framed Binary Protocol (§9)
- **Transport**: Unix domain socket (`$XDG_RUNTIME_DIR/z3rm/mux.sock`) or Windows named pipe (`\\.\pipe\z3rm-mux`).
- **Framing**: Length-prefixed protobuf envelopes (`mux_protocol::Envelope`).
- **Message types**:
  - **Request/Response**: RPC with `request_id` correlation (§9).
  - **Notifications**: Server→client push (PaneDirty, PaneAdded, PaneRemoved, PaneFocused, SessionLayoutChanged, etc.) (§9).
  - **Grid Sync**: Pull-based diff with generation counter + 64-entry ring buffer (§3.3).
  - **PTY Byte Stream**: Optional raw PTY bytes for in-place render path (§3.1).

### 3. Process Architecture

| Process | Role | Crate |
|---------|------|-------|
| `z3rm` (GUI) | GPUI application, window management, extension host | `z3rm` |
| `z3rm-server` (daemon) | PTY management, terminal emulation, session persistence | `mux_server` |
| `z3rm` CLI | Command dispatch (attach, kill, status) | `cli` |

**Daemon lifecycle** (§16.1):
1. GUI client calls `ensure_daemon_running()` → spawns `z3rm-server` if socket absent.
2. Server binds socket, initializes SQLite (sessions/layouts), starts PTY event loop.
3. Client connects via `MuxDomain::connect_local()` → `AttachRequest` → receives `SessionSnapshot`.
4. On client disconnect: server retains session; `DetachRequest` or socket close triggers cleanup only when last client detaches (or `keep_alive` timeout).

### 4. Data Flow Summary

```
User Input (GPUI) ──► MuxDomain.send_input() ──► Unix Socket ──► mux_server
                                                              │
                                                              ▼
                                                  PTY write → alacritty_terminal
                                                              │
                                                              ▼
                                              Grid diff (generation++) → DiffRing
                                                              │
                                                              ▼
                              PaneDirty notification ◄─── Push to attached clients
                                                              │
                                                              ▼
                              Client: FetchGridUpdateRequest(gen) ──► GridDiff/FullSnapshot
                                                              │
                                                              ▼
                                              GPUI renders grid via TerminalElement
```

### 5. Extension Host (QuickJS)
- Runs on a **dedicated OS thread** per extension (§5.2).
- Resource limits: CPU 50 ms/s (fuel), Memory 64 MB, IO 100 ops/s (token bucket).
- Extensions return **VDOM (JSON)** → `vdom_bridge` maps to GPUI elements.
- 6 built-in extensions: status-bar, tab-bar, command-palette, which-key, session-manager, layout-manager.

### 6. Shadow Snapshot Engine (§4)
- **Version tree** (not DAG) per file path, keyed by monotonic `SeqNo`.
- **WAL-first write path**: content → blob store (content-addressed, Zstd) → WAL entry → version tree advance.
- **Bounded delta chain**: `D_MAX = 16` rope-level deltas before forced full snapshot materialization.
- **Crash-safe decline**: WAL records intent → write file → watcher re-observes → confirm.

### 7. Transport Resilience (§16.6)
- Optional UDP-based reliable transport (`transport_resilient`) with AES-256-GCM, stateless roaming, RTT estimation (RFC 6298), heartbeat (3s/40s), MTU 1280 fragmentation.
- Used for high-latency / lossy links; primary path remains Unix socket.

## Key Spec Sections Referenced

| Spec Section | Topic |
|--------------|-------|
| §3.1 | Server-canonical terminal model |
| §3.2 | Transport (Unix socket / SSH) |
| §3.3 | Grid sync (generation counter, diff ring) |
| §3.10 | Protocol versioning, session lifecycle |
| §4 | Shadow snapshot engine (WAL, version tree, blob store) |
| §5.2 | QuickJS resource limits & thread isolation |
| §5.3 | Extension API traits (MuxApi, CommandApi, KeymapApi, SettingsApi, TerminalApi) |
| §5.4 | VDOM JSON → GPUI bridge |
| §9 | Framed binary protocol, request/response, notifications |
| §16.1 | Daemon auto-start, socket path, default session |
| §16.6 | UDP resilient transport, extension sync |
| §16.9 | Scrollback buffer, sync scroll |
| §16.12 | Logging, daemon connection monitoring |