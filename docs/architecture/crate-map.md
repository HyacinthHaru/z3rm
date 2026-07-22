# Crate Map

Complete inventory of crates in the z3rm workspace, grouped by architectural layer.

## Entry Point

| Crate | Type | Description |
|-------|------|-------------|
| `z3rm` | Binary + Library | GPUI entry, daemon spawn/connect, window setup, extension host integration. |

## Mux Layer

| Crate | Type | Key Types | Depends On |
|-------|------|-----------|------------|
| `mux_protocol` | Library | `Envelope`, `Request`, `Response`, `Notification`, `SessionSnapshot`, `GridDiff` | `prost`, `prost-types` |
| `mux` | Library | `MuxDomain`, `MuxTransport` (Local/Ssh), `connect_local()`, grid sync pull | `mux_protocol`, `interprocess`, `tokio` |
| `mux_server` | Binary+Library | `Server`, `Session`, `Pane`, `GridDiffRing`, `ScrollbackBuffer`, `LayoutTree` | `mux`, `mux_protocol`, `alacritty_terminal`, `portable_pty`, `shadow_snapshot`, `sqlez` |

## Shadow Snapshot, Extensions, Transport

| Crate | Type | Key Types |
|-------|------|-----------|
| `shadow_snapshot` | Library | `ShadowSnapshotEngine`, `VersionTree`, `Wal`, `BlobStore`, `DeltaReplay`, `DeclineProtocol`, `QuotaManager` |
| `quickjs_runtime` | Library | `QuickJsRuntime`, `CpuFuelTracker`, `IoTokenBucket` (50ms CPU, 64MB mem, 100 IO/s) |
| `extension_host` | Library | `ExtensionApi`, `VDomNode`, `CapabilityGranter`, `ExtensionSettings` |
| `extension` | Library | Manifest types, `extension.toml` parsing, capability definitions (`ProcessExecCapability`, etc.) |
| `transport_resilient` | Library | `UdpResilientTransport`, `UdpClient`, `UdpServer`, AES-256-GCM, RTT estimator, MTU=1280 |

## Migration & Macros

| Crate | Type | Purpose |
|-------|------|---------|
| `z3rm_macros` | Proc-macro | `#[z3rm_migration]`, `count_todos` binary |
| `z3rm_macros_types` | Library | Shared types for macro expansion |

## Retained Zed Crates (Consolidated)

| Group | Crates |
|-------|--------|
| **Core GPUI** | `gpui` (GPU UI), `gpui_platform`, `gpui_tokio` |
| **Terminal & Text** | `terminal`, `terminal_view`, `alacritty_terminal`, `rope` |
| **Editor & Language** | `editor`, `language`, `languages`, `language_extension` |
| **Workspace & Project** | `workspace` (Pane, PaneGroup, persistence), `project`, `file_finder`, `recent_projects` |
| **Settings** | `settings`, `settings_ui`, `settings_profile_selector` |
| **UI Components** | `command_palette`, `sidebar`, `title_bar`, `tab_switcher`, `notifications`, `file_finder` |
| **Theming** | `theme`, `theme_settings`, `theme_extension` |
| **Utilities** | `fs`, `db`, `cli`, `clipboard`, `auto_update`, `zlog`/`ztracing`, `collections`, `remote`, `paths` |
| **Infrastructure** | `prost`/`prost-build` (protobuf), `sqlez` (SQLite), `interprocess` (sockets), `portable_pty` |

## Dependency Graph

```
z3rm ──┬─ gpui, editor, workspace, terminal, terminal_view
       ├─ mux ──► mux_protocol ──► interprocess (socket)
       ├─ quickjs_runtime + extension_host
       ├─ transport_resilient (optional UDP)
       └─ settings, theme, command_palette, sidebar, ...

mux_server ──┬─ mux, mux_protocol
              ├─ alacritty_terminal, portable_pty
              ├─ shadow_snapshot (cwd watch)
              ├─ sqlez (SQLite persistence)
              └─ interprocess (socket listener)
```

## Feature Flags

| Crate | Features |
|-------|----------|
| `mux` | `ssh` (remote server connection) |
| `z3rm` | `tracy`, `track-project-leak`, `test-support`, `visual-tests` |
| `extension_host` | `z3rm-migration`, `test-support` |
| `gpui` | `input-latency-histogram`, `leak-detection`, `test-support` |