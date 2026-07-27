# Getting Started with z3rm Development

## Prerequisites

- **Rust toolchain:** Install via [rustup](https://rustup.rs/). The project uses edition 2024.
- **System dependencies (Linux):** `wayland-devel`, `libxkbcommon`, `vulkan-loader`, `fontconfig`, `pkg-config`. Run `script/linux` for the full list.
- **System dependencies (macOS):** Xcode Command Line Tools.
- **System dependencies (Windows):** Visual Studio Build Tools with MSVC + Windows SDK. See building-windows-wine.md.

## Clone and Build

```sh
git clone <repository-url>
cd z3rm

# Debug build (default)
cargo build

# Release build
cargo build --release

# Build only the daemon
cargo build -p mux_server

# Build only the CLI
cargo build -p cli
```

## Test

```sh
cargo test --workspace              # All tests
cargo test -p mux                   # Mux crate tests
cargo test -p mux_server            # Server tests
cargo test -p terminal_view         # Terminal rendering tests
```

## Run

### Daemon (z3rm-server)

The headless mux server owns all PTYs and terminal state. It auto-starts on client connect.

```sh
cargo run -p mux_server
```

### Server configuration (z3rm-server)

The daemon reads scrollback capacity and its idle-shutdown timer from a
small JSON file so you don't need env vars in the launcher:

```sh
mkdir -p "${XDG_CONFIG_HOME:-$HOME/.config}/z3rm"
cat > "${XDG_CONFIG_HOME:-$HOME/.config}/z3rm/server.json" <<'EOF'
{
  "keep_alive_seconds": 0,
  "scrollback_lines": 10000
}
EOF
```

| Key | Default | Affects |
|---|---|---|
| `keep_alive_seconds` | `0` (off) | Idle daemon shutdown delay. `0` = keep alive forever. |
| `scrollback_lines` | `10000` | Backlog rows per pane (capped at 100_000). |
| `max_scroll_history_lines` | — | Alias for `scrollback_lines`. |

Override the file path with `Z3RM_SERVER_SETTINGS`, or set
`Z3RM_SCROLLBACK_LINES` / `Z3RM_KEEP_ALIVE_SECONDS` to skip the file. Values
reload live every couple of seconds (drop-in edit `server.json` and the running
daemon applies the new scrollback to existing panes; new panes pick it up at
spawn). See `docs/architecture/mux-design.md`.

### GUI client (z3rm)

```sh
cargo run -p z3rm
```

### CLI

The packaged `bin/z3rm` is the `cli` wrapper. Mux subcommands (`ls`, `new`,
`attach`, `send-keys`, `capture-pane`, …) are forwarded to the real `z3rm`
binary (`libexec/z3rm` in packages, or `target/debug/z3rm` in development).
Prefer running the main binary during development so you exercise the same
parser the daemon GUI uses:

```sh
cargo run -p z3rm -- ls
cargo run -p z3rm -- new -s mysession
cargo run -p z3rm -- kill-session -t mysession
cargo run -p z3rm -- split-window -t mysession -h
cargo run -p z3rm -- send-keys -t mysession Enter
cargo run -p z3rm -- capture-pane -t mysession -p -e

# Packaged/wrapper path (forwards the same argv after detecting mux verbs):
cargo run -p cli -- ls
```

## Project Structure

Key crates in the z3rm foundation:

| Crate | Role |
|---|---|
| `crates/z3rm` | GUI client entry point |
| `crates/mux_server` | Headless daemon (PTY + alacritty + layout) |
| `crates/mux` | Client-side MuxDomain + transport |
| `crates/mux_protocol` | Protobuf wire protocol |
| `crates/cli` | Tmux-compatible CLI |
| `crates/terminal_view` | GPUI terminal rendering |
| `crates/shadow_snapshot` | Versioned filesystem snapshots |
| `crates/quickjs_runtime` | QuickJS extension engine |
| `crates/transport_resilient` | UDP resilient transport |
| `crates/z3rm_macros` | `#[z3rm_todo]` proc macro |