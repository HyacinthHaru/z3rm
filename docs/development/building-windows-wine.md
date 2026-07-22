# Building for Windows (Wine / CI)

## Wine for Compilation Checks Only

Wine can compile Rust code for Windows targets, but **ConPTY cannot be tested via Wine**. Windows Pseudo Console (ConPTY) is the terminal backend used by z3rm's `mux_server` and `terminal_view` on Windows. Wine implements POSIX APIs, not the Windows Console API.

**Use Wine only for:**
- `cargo check` / `cargo build` cross-compilation validation
- Syntax and type checking of Windows-specific code paths
- Verifying that `#[cfg(target_os = "windows")]` blocks compile

**Do NOT use Wine for:**
- Running `z3rm-server` or `z3rm` GUI
- Testing terminal rendering, multiplexer attach/detach, or PTY I/O
- Integration or behavioral tests that require ConPTY

## Cross-Compilation Setup

From a Linux host, install the MinGW-w64 cross-compiler:

```sh
# Fedora
sudo dnf install mingw64-gcc mingw64-winpthreads-static
# Debian/Ubuntu
sudo apt install gcc-mingw-w64-x86-64
# Arch
sudo pacman -S mingw-w64-gcc
```

Build for Windows:

```sh
cargo build --target x86_64-pc-windows-gnu -p mux_server
cargo build --target x86_64-pc-windows-gnu -p cli
```

For MSVC targets (`x86_64-pc-windows-msvc`) you need a real Windows build machine.

## Real Windows CI Runner

Behavioral testing requires a native Windows runner (GitHub Actions `windows-latest`, self-hosted VM, or physical machine).

**Required tooling:**
- Visual Studio Build Tools 2022+ with "Desktop development with C++" workload
- Windows 10 SDK (version 10.0.20348.0 or later)
- CMake
- Rust via rustup (`x86_64-pc-windows-msvc` target)

**CI configuration:** See `.github/workflows/` for existing Windows pipeline examples.

## Known Issues

- **Long paths:** Enable long path support in Git (`git config --system core.longpaths true`) and the Windows registry
- **Vulkan:** z3rm uses Vulkan on Windows. Update GPU drivers if the GUI fails to initialize with `NoSupportedDeviceFound`
- **Custom RUSTFLAGS:** The project's `.cargo/config.toml` sets required flags. Setting `RUSTFLAGS` as an env var overrides these and breaks the build