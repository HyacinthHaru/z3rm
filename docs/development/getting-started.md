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

### GUI client (z3rm)

```sh
cargo run -p z3rm
```

### CLI

The CLI provides tmux-compatible commands for multiplexer control.

```sh
cargo run -p cli -- ls                 # List sessions
cargo run -p cli -- new -s mysession   # Create named session
cargo run -p cli -- kill-session -t id # Kill session
cargo run -p cli -- split-window -d h  # Split horizontally
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