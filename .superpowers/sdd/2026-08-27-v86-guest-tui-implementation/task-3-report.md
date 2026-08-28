# Task 3 report — guest landing TUI and v86 filesystem package

## Scope

Implemented the bounded Task 3 deliverable without touching the Task 2 mux files or
`crates/fs/src/wasm_fs.rs`:

- `crates/z3rm_guest_tui/Cargo.toml`
- `crates/z3rm_guest_tui/src/main.rs`
- root workspace membership in `Cargo.toml`
- `crates/z3rm_web/src/local_server.rs`
- `website/wasm/z3rm_demo/build-guest-fs.sh`
- generated content-addressed files and `fs.json` under `website/public/v86/fs`

The existing source asset `website/public/media/z3rm-terminal-grid.png` is staged
by the packaging script as `/mnt/z3rm-terminal-grid.png`.

## Deliverable

- Added the no-dependency `z3rm-tui` binary. Its terminal I/O uses libc
  `termios`, `read(2)`, and `write(2)`; the bundled PNG is loaded with libc
  `open(2)`, `read(2)`, and `close(2)`. It enters raw mode, alternate-screen
  mode, SGR mouse tracking, and hidden-cursor mode, then restores the saved
  terminal state through an RAII guard. Restoration and error-reporting failures
  are handled and reported explicitly.
- Added deterministic layout and SGR mouse parsing/action tests. Wheel buttons 64
  and 65 move a clamped page offset; left-clicks map to download/copy rectangles.
- The landing page includes a colored z3rm/product header, guest/GPUI/mux/serial
  architecture explanation, a scrollable page, a visible OSC 8
  `z3rm-download:/` `Download server` link, and a `Copy install command` button.
- The Kitty image command is generated from the bytes loaded at
  `/mnt/z3rm-terminal-grid.png` and uses exactly
  `a=T,f=100,i=1,c=56,r=12,q=2`.
- Download clicks emit the OSC 8 hyperlink and BEL-terminated OSC 9
  `z3rm-download;/` action consumed by the mux terminal-media scanner. Copy
  clicks emit BEL-terminated OSC 9 `z3rm-copy;<base64>` plus OSC 52 `c;<base64>`
  with ST termination; the typed action and clipboard paths remain distinct.
- Added the `/mnt/z3rm` wrapper: `a`, `attach`, and `landing` exec
  `/mnt/z3rm-tui`; any other command prints usage and exits 2.
- The first web session pane now requests `/mnt/z3rm-tui` with empty args/env.
- Packaging builds both static i686-musl binaries, stages the TUI, mux server,
  wrapper, start script, and PNG, exports `PATH=/mnt:$PATH` from `start-mux.sh`,
  removes stale `.bin` chunks, regenerates `fs.json`, and writes current hashes.

## TDD and verification

The initial focused command before the crate existed produced the expected red
state:

```text
$ cargo test -p z3rm_guest_tui
error: package ID specification `z3rm_guest_tui` did not match any packages
```

After adding the tests and implementation, then adding the multi-click/action
protocol and terminal-size regressions:

```text
$ cargo test -p z3rm_guest_tui
cargo test: 7 passed (1 suite, 0.00s)
```

The requested guest build succeeded:

```text
$ RUSTFLAGS="-C linker=rust-lld -C strip=symbols -C panic=abort" cargo build -p z3rm_guest_tui --target i686-unknown-linux-musl --release
[exit 0; Cargo emitted only existing workspace dependency-patch warnings]
```

The resulting binary is:

```text
target/i686-unknown-linux-musl/release/z3rm-tui: ELF 32-bit LSB executable, Intel i386, version 1 (GNU/Linux), statically linked, stripped
```

The first packaging attempt exposed and corrected an incorrect relative media
path. The requested command then succeeded:

```text
$ sh website/wasm/z3rm_demo/build-guest-fs.sh
Creating file tree ...
Creating json ...
guest fs packaged into ../../public/v86/fs
```

After the final Task 2 follow-up (`d0e634bb27`) and the final TUI action/terminal-size
fixes, the post-package index check found five entries and validated every
referenced chunk's size and SHA-256 prefix:

```text
entries 5 index_size 3577944 checks [('start-mux.sh', True, True, '18df1693.bin'), ('mux_server', True, True, '1be1d510.bin'), ('z3rm', True, True, 'd866e2c2.bin'), ('z3rm-terminal-grid.png', True, True, 'c19922ad.bin'), ('z3rm-tui', True, True, '3d5df8dd.bin')]
bin_files ['18df1693.bin', '1be1d510.bin', '3d5df8dd.bin', 'c19922ad.bin', 'd866e2c2.bin']
all_valid True
sum_entry_sizes 3577944 index_size 3577944 matches True
```

The mux chunk is `1be1d510.bin` (3,091,140 bytes); the TUI chunk is
`3d5df8dd.bin` (441,308 bytes). The wrapper, PNG, and start-script chunks
remain unchanged. Both target binaries and packaged ELF chunks were rechecked
as static i386 stripped ELFs, and the packaged PNG remains a valid 45,100-byte
PNG.

Running the packaged wrapper with an unknown command returned the required
status and usage text:

```text
status=2 stderr=usage: /mnt/z3rm {a|attach|landing}
```

```text
target/i686-unknown-linux-musl/release/z3rm-tui:    ELF 32-bit LSB executable, Intel i386, version 1 (GNU/Linux), statically linked, stripped
target/i686-unknown-linux-musl/release/z3rm-server: ELF 32-bit LSB executable, Intel i386, version 1 (GNU/Linux), statically linked, stripped
website/public/v86/fs/1be1d510.bin:                ELF 32-bit LSB executable, Intel i386, version 1 (GNU/Linux), statically linked, stripped
website/public/v86/fs/3d5df8dd.bin:                 ELF 32-bit LSB executable, Intel i386, version 1 (GNU/Linux), statically linked, stripped
website/public/v86/fs/c19922ad.bin:                 PNG image data, 1440 x 640, 8-bit/color RGBA, non-interlaced
```

No commit or push was performed here; the parent session owns the final commit
boundary after the Task 2 commit.
