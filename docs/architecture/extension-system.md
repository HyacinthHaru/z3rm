# Extension System

QuickJS-based extension runtime with **dedicated OS thread isolation**, **resource limits**, **VDOM→GPUI bridge**, and **capability-based permissions**.

## Architecture

```
z3rm (GPUI Main Thread)               QuickJS Thread (per extension)
┌─────────────────────────────┐       ┌─────────────────────────┐
│ ExtensionHost               │       │ Runtime + Context       │
│ • ExtensionApi (5 traits)   │──►──│ CPU: 50ms/s fuel        │
│ • VDOM Bridge (JSON→GPUI)   │       │ Mem: 64MB limit         │
│ • CapabilityGranter         │       │ IO: 100 ops/s (bucket)  │
└─────────────────────────────┘       └─────────────────────────┘
```

## QuickJS Runtime (`quickjs_runtime`)

```rust
pub struct QuickJsRuntime {
    runtime: Runtime,                 // rquickjs, single-threaded
    fuel_tracker: CpuFuelTracker,     // interrupt_handler, CPU budget
    io_bucket: IoTokenBucket,         // rate=100/s, capacity=200
    memory_limit: usize,              // 64MB default
}
```

### Resource Limits (per extension)

| Resource | Limit | Enforcement |
|----------|-------|-------------|
| **CPU** | 50 ms/s | `set_interrupt_handler()` — elapsed CPU check, throws on overrun |
| **Memory** | 64 MB | `set_memory_limit()` — alloc failure throws `RangeError` |
| **IO** | 100 ops/s | Token bucket (rate 100/s, burst 200) — blocks or rejects |

**Thread isolation**: `ExtensionRunner::run()` spawns `std::thread` for each extension. No GPUI access from JS thread — all UI via VDOM JSON returns.

## Extension Manifest (`extension.toml`)

```toml
[extension]
name = "z3rm-status-bar"
version = "0.1.0"
[runtime]
side = "client"          # "client" | "server" | "both"
[capabilities]
terminal = true          # pane events
mux = true               # session/pane management
workspace = true         # file tree, search
[resources]
memory_limit_mb = 64
cpu_budget_ms = 50
io_rate_limit = 100
```

### Runtime Side

| Value | Meaning |
|-------|---------|
| `client` | Runs in GPUI process (z3rm) |
| `server` | Runs in mux_server daemon (future) |
| `both` | Both processes |

## Extension API Traits (`extension_host/src/api.rs`)

Five `Send + Sync` traits, aggregated in `ExtensionApi`:

```rust
pub trait MuxApi: Send + Sync {
    async fn list_sessions(&self) -> Result<Vec<SessionInfo>>;
    async fn create_session(&self, name: &str, cwd: &Path) -> Result<String>;
    async fn spawn_pane(&self, ...) -> Result<String>;
    async fn split_pane(&self, pane: &str, dir: SplitDirection) -> Result<String>;
    async fn close_pane(&self, pane: &str) -> Result<()>;
    async fn focus_pane(&self, pane: &str) -> Result<()>;
    async fn resize_pane(&self, pane: &str, cols: u32, rows: u32) -> Result<()>;
    async fn send_input(&self, pane: &str, bytes: &[u8]) -> Result<()>;
    async fn get_grid(&self, pane: &str) -> Result<FullGridSnapshot>;
}

pub trait CommandApi: Send + Sync {
    fn register_command(&self, id: CommandId, title: &str, cb: Box<dyn Fn() + Send>);
    async fn execute_command(&self, id: CommandId) -> Result<()>;
}

pub trait KeymapApi: Send + Sync {
    fn bind_key(&self, sequence: KeySequence, cmd: CommandId) -> Result<()>;
    fn unbind_key(&self, sequence: KeySequence) -> Result<()>;
}

pub trait SettingsApi: Send + Sync {
    fn get(&self, key: &str) -> Result<Option<serde_json::Value>>;
    fn set(&self, key: &str, value: serde_json::Value) -> Result<()>;
}

pub trait TerminalApi: Send + Sync {
    fn subscribe_pane_events(&self, pane: &str, cb: Box<dyn Fn(PaneEvent) + Send>);
}
```

Aggregate facade: `ExtensionApi { mux, command, keymap, settings, terminal }` — passed to extension on activation.

## VDOM Bridge (§5.4)

Extensions return Virtual DOM (JSON) from `render()`:

```json
{
  "type": "div", "props": {"id": "status-bar"},
  "style": {"gap": "4px"},
  "children": ["text", {"type": "button", "props": {"onclick": "cmd:refresh"}}]
}
```

**Pipeline**: `JS render()` → `serde_json::Value` → `parse_vdom()` → `VDomNode{type, props, style, children}` → `vdom_to_element()` → `gpui::AnyElement`.

| VDOM Type | GPUI Element |
|-----------|--------------|
| `div` | `div().flex()` container |
| `span` | Inline text wrapper |
| `button` | `div().on_click()` — `props.onclick = "cmd:<id>"` |
| Text child | `SharedString` label |

**Display-list pattern**: full VDOM each frame; `vdom_to_element()` diffs previous tree structurally; only changed subtrees re-render.

## Permission Model (`capability_granter.rs`)

```rust
pub struct CapabilityGranter {
    granted: Vec<ExtensionCapability>,
    manifest: Arc<ExtensionManifest>,
}
```

**Flow**:
1. Extension declares capabilities in `extension.toml`.
2. User grants at install/first-use → persisted in settings.
3. At FFI call time: `grant_exec(cmd, args)` validates manifest + user grant.
4. **No ambient authority**: every privileged operation checked against both.

Capability types: `ProcessExec(command, args_pattern)`, `DownloadFile(url_pattern)`, `NpmInstallPackage(pattern)`. Args support wildcard `*` and trailing `**`.

## Built-in Extensions (6)

| Extension | Capabilities | Purpose |
|-----------|--------------|---------|
| `z3rm-status-bar` | `terminal`, `mux`, `workspace` | Status line (session, pane, cwd) |
| `z3rm-tab-bar` | `workspace` | Tab strip with drag-reorder |
| `z3rm-command-palette` | `workspace`, `mux` | Ctrl+Shift+P fuzzy palette |
| `z3rm-which-key` | `workspace` | Keybinding hint popup |
| `z3rm-session-manager` | `mux` | Session list/create/attach UI |
| `z3rm-layout-manager` | `workspace`, `mux` | Layout save/restore, split presets |

## Lifecycle

1. Parse `extension.toml` → `ExtensionManifest`.
2. `CapabilityGranter` initialized with persisted user grants.
3. `std::thread::spawn(ExtensionRunner::run(manifest, api))`.
4. `QuickJsRuntime::new(limits)` → `Runtime` + `Context`.
5. Load `main.js` → `eval` → `exports.activate(api)`.
6. Event loop: `PaneDirty` → `TerminalApi` callback, command invoke → `CommandApi` callback, render tick → `render()` → VDOM → GPUI.

**Error handling**: JS exceptions caught → logged → unhealthy. Auto-reload (debounced 200ms), 3× max then disabled. Resource limits throw catchable JS errors.

## Settings Persistence

Per-extension key-value store at `$XDG_CONFIG_HOME/z3rm/extensions/<name>/settings.json`. File-backed, debounced writes via `ExtensionSettings` struct. Accessed through `SettingsApi`.

## Future: Server-Side Extensions

`runtime.side = "server"` or `"both"` → extension runs in mux_server. Same QuickJS, different `ExtensionApi` impl. Communicates via `ExtensionChromeUpdate` protobuf (VDOM push over mux protocol). Not fully implemented; manifest field reserved.