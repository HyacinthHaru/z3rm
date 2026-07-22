# Building on Linux

## System Dependencies

```sh
# Fedora
sudo dnf install wayland-devel libxkbcommon-devel vulkan-loader-devel \
  fontconfig-devel freetype-devel expat-devel libxcb-devel \
  mesa-libGL-devel pkgconfig

# Ubuntu/Debian
sudo apt install libwayland-dev libxkbcommon-dev libvulkan-dev \
  libfontconfig-dev libfreetype-dev libexpat-dev libxcb-composite0-dev \
  libgl1-mesa-dev pkg-config

# Arch
sudo pacman -S wayland libxkbcommon vulkan-headers vulkan-icd-loader \
  fontconfig freetype2 expat libxcb mesa pkg-config
```

## Build

```sh
git clone <repo>
cd z3rm
cargo build -p mux_server -p z3rm
```

## Run

```sh
# Start daemon
./target/debug/z3rm-server

# Launch GUI (separate terminal)
./target/debug/z3rm
```

## Common Issues

- **Missing wayland headers**: Ensure `wayland-devel` (Fedora) or `libwayland-dev` (Ubuntu) is installed.
- **Vulkan not found**: Install `vulkan-loader-devel` / `libvulkan-dev` and ensure a Vulkan-capable GPU driver is present.
- **`openssl-sys` build failure**: Install `openssl-devel` (Fedora) or `libssl-dev` (Ubuntu).
- **`libxkbcommon` not found**: Ensure `libxkbcommon-devel` / `libxkbcommon-dev` is installed.