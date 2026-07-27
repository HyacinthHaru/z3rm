# Mux Design

Architecture of the mux client (`MuxDomain`) and server (`mux_server`).

## MuxDomain — Client Core (§9)

```rust
pub struct MuxDomain {
    inner: Arc<parking_lot::RwLock<DomainInner>>,
    window_id: String,        // §3.3 multi-window support
}
struct DomainInner {
    next_request_id: AtomicU64,
    pending_requests: HashMap<u64, Sender<Response>>,  // oneshot per RPC
    subscribers: Vec<async_channel::Sender<Notification>>, // §9 broadcast
    write_tx: mpsc::Sender<Vec<u8>>,                    // framed outbound
}
```

**I/O Architecture**: Dedicated `mux-io` OS thread runs `io_and_router_loop()` — polls write queue → writes frame; reads next frame → decodes `Envelope` → routes response to `pending_requests` or broadcasts notification to `subscribers`. Single writer serializes all outbound frames.

### MuxTransport Enum (§3.2)

```rust
pub enum MuxTransport {
    Local(interprocess::local_socket::Stream),  // Unix socket / named pipe
    #[cfg(feature = "ssh")] Ssh(ssh::SshSession),
}
```

`connect_local()` → Unix socket at `$XDG_RUNTIME_DIR/z3rm/mux.sock` (or `/tmp/z3rm/mux.sock`). Windows: `\\.\pipe\z3rm-mux`.

### Request/Response (§9)

```rust
async fn send_request(&self, req: RequestBody) -> Result<Response> {
    let id = next_request_id.fetch_add(1);
    pending.insert(id, tx);
    write_tx.send(frame(Request { request_id: id, body: req }))?;
    rx.recv()?  // blocks I/O thread until response
}
```

`request_id` correlation via protobuf fields. Timeout: blocked until socket closes.

### Notifications (§9)

Server → client push types:

| Notification | Payload | Semantics |
|--------------|---------|-----------|
| `PaneDirty` | `pane_id` | At-most-once (coalesced); client pulls via `FetchGridUpdate` |
| `PaneAdded` | `PaneInfo` | At-least-once (idempotent) |
| `PaneRemoved` | `pane_id`, `exit_code?` | At-least-once |
| `PaneFocused` | `pane_id` | At-least-once |
| `TabTitleChanged` | `tab_id`, `title` | At-least-once |
| `SessionLayoutChanged` | `LayoutTree` | At-least-once (layout split/close/resize) |
| `WindowAdded/Removed` | `window_id` | §3.3 multi-window |
| `ShellIntegrationChanged` | `cwd` | OSC 7/133 |
| `PaneZoomed` | `pane_id`, `zoomed` | Zoom toggle |

No ACK; UDP transport (§16.6) adds its own reliability layer.

## Grid Sync — Pull Model (§3.3)

**Server** (`GridDiffRing`, 64-entry `VecDeque`):

```
Pane.generation: AtomicU64 (monotonic, incremented per grid mutation)
  → VT output → diff_from_dirty() → GridDiffRing.push(gen, diff)
  → PaneDirty notification → client calls fetch_grid_update(pane, since_gen)
```

**Handler** (`handle_fetch_grid_update`):

| Condition | Response |
|-----------|----------|
| `since_gen == 0` | `FullGridSnapshot` (initial attach) |
| `since_gen >= current` | `NoChange(current)` |
| `since_gen < oldest_in_ring` | `FullGridSnapshot` (cache invalidated) |
| else | Merged `GridDiff` (row-level dedup) |

Diff merging: later `RowChange` for same row overwrites earlier.

## Attach/Detach with Snapshot Reconcile (§3.10, §15.4)

1. Client sends `AttachRequest{session_id, mode, window_id, identity}`.
2. Server `handle_attach()`:
   - Register client `outbound_tx` as subscriber for all session panes.
   - Build `SessionSnapshot { tabs, layout, panes_with_generation }`.
   - Return `AttachResponse{snapshot}`.
3. Client constructs `MuxPaneView` for each pane, subscribes to `PaneDirty`, calls `fetch_grid_update(pane, 0)` for initial full grid.
4. Subsequent `PaneDirty` → incremental pull.

**Detach**: `DetachRequest{}` → server removes client subscriber; session persists if other clients attached or `keep_alive`.

## mux_server — Daemon Core (§3.1)

```rust
struct Server {
    listener: LocalSocketListener,
    sessions: Arc<RwLock<Vec<Session>>>,
    db: Arc<Mutex<Connection>>,            // SQLite persistence
    clipboard: Arc<ServerClipboard>,
    shutdown: Arc<AtomicBool>,
}
```

### Session (§3.2)
```rust
struct Session {
    id, name, cwd, created_timestamp,
    tabs: HashMap<String, Tab>,
    layout: LayoutTree,
    focused_pane: Option<String>, focused_tab: Option<String>,
    attached_clients: Arc<RwLock<Vec<AttachedClient{outbound_tx, role, window_id}>>>,
    panes: Arc<RwLock<HashMap<String, Arc<Pane>>>>,
    sync_scrollback: Arc<RwLock<SyncScrollbackState>>,
    connected_windows: Arc<RwLock<Vec<String>>>,
    snapshot_watch: Option<Arc<SnapshotWatch>>,
}
```

### Pane (§3.1)
```rust
struct Pane {
    id, session_id, tab_id,
    term: Arc<Mutex<Term>>,          // alacritty terminal emulator
    pty: Arc<Mutex<Box<dyn MasterPty>>>, // PTY master fd
    generation: AtomicU64,           // grid sync counter
    grid_diff_ring: GridDiffRing,    // 64-entry ring
    scrollback: ScrollbackBuffer,    // §16.9
    title, cwd, size: Arc<RwLock<...>>,
    subscribers: Arc<Mutex<Vec<UnboundedSender<Notification>>>>,
}
```

**PTY Read Loop**: `pty.read()` → bytes → `term.process()` (VT parsing) → `damage()` → `diff_from_dirty()` → `GridDiffRing.push(gen++)` → `PaneDirty` to subscribers. Adaptive coalescing (5ms default window).

### Persistence
SQLite stores sessions, tabs, layout tree (`SplitNode` × `PaneLeaf`), pane metadata. On startup: `load_sessions_from_db()` → restore sessions, re-spawn PTYs. Layout: recursive horizontal/vertical `SplitNode` with ratios.

### Server Settings — `server.json` (§16.11)

The mux_server daemon reads a small JSON config so operators can tune
scrollback capacity and the idle-shutdown timer without recompiling.

**Sources** (highest wins at load time):

1. Environment — `Z3RM_SCROLLBACK_LINES`, `Z3RM_KEEP_ALIVE_SECONDS`.
2. File — `$Z3RM_SERVER_SETTINGS` (explicit override) or the default
   `$XDG_CONFIG_HOME/z3rm/server.json` (or `~/.config/z3rm/server.json`).

**Schema** (`crates/mux_server/src/server_settings.rs`); a copy-to-`server.json` sample lives at `crates/mux_server/server.example.json`:

```json
{
  "keep_alive_seconds": 0,
  "scrollback_lines": 10000,
  "max_scroll_history_lines": 10000
}
```

| Key | Type | Default | Notes |
|---|---|---|---|
| `keep_alive_seconds` | `u64` | `0` (disabled) | Idle daemon shutdown delay; `0` = keep alive forever. Read once at boot and on the next idle cycle when the file changes. |
| `scrollback_lines` | `u64` (≤ `100_000`) | `10_000` | Backlog rows per pane. Capped at 100k. Existing live panes shrink (FIFO drop) on shrink; new panes honor it immediately. |
| `max_scroll_history_lines` | `u64` (≤ `100_000`) | — | Alias for `scrollback_lines` (matches `terminal.max_scroll_history_lines` from the client settings schema). `scrollback_lines` wins when both are set. |

**Hot reload**: a background task polls the resolved file path every 2s
(`ServerSettings::spawn_hot_reload`). On file change it parses the JSON,
swaps the value into the live `Arc<ServerSettings>` atomics, and applies the
new scrollback capacity to every pane currently in `sessions` via
`Pane::set_scrollback_capacity`. `keep_alive_seconds` is re-read on the
next idle cycle.

**Wiring**: new panes receive their capacity from
`ServerSettings::scrollback_lines()` (which already incorporates env + JSON)
threaded through `handle_connection` → `handle_spawn_pane` /
`handle_split_pane` into `Pane::spawn_with_session`. `Pane::spawn` (used by
unit tests) falls back to the env-only `default_scrollback_lines()` since no
live settings handle is in scope.

## Protocol Summary (mux_protocol §9, §3.10)

| Category | Messages |
|----------|----------|
| Session | `CreateSession`, `ListSessions`, `KillSession`, `RenameSession` |
| Attach | `Attach`→`SessionSnapshot`, `Detach` |
| Window | `NewWindow`, `WindowAdded`, `WindowRemoved` |
| Pane | `SpawnPane`, `SplitPane`, `ClosePane`, `FocusPane`, `ResizePane`, `SetPaneTitle`, `ZoomPane` |
| Input | `SendInput`, `Paste` |
| Grid Sync | `FetchGridUpdate`→`FullGridSnapshot`/`GridDiff`/`NoChange` |
| Scrollback | `FetchScrollback`, `SearchScrollback`, `SyncScrollbackNotification` |
| Clipboard | `SetClipboard`, `GetClipboard`, `ClipboardChanged` |
| Shell | `ShellIntegration` (OSC 7/133), `PaneZoomed` |
| File RPC | `ReadFile`, `ListDir`, `StatFile` |
| Extensions | `ExtensionChromeUpdate`, `InstallExtension` |

All wrapped in `Envelope{version, payload}` with length-prefixed framing (§9).