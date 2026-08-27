# Task 2 report — standalone terminal-media scanner

## Scope

Standalone `TerminalMediaScanner` in `crates/mux_server/src/terminal_media.rs`
(§16.13), plus its module declaration in `crates/mux_server/src/mux_server.rs`.
No Pane/connection wiring in this task.

## Deliverable

- `crates/mux_server/src/terminal_media.rs` (new file)
  - Public API: `TerminalMediaScanner::feed(&mut self, bytes: &[u8]) -> ScanOutput`
    with `ScanOutput { grid_bytes, media, actions }`.
  - Public constant `MAX_CONTROL_SEQUENCE_BYTES: usize = 4 * 1024 * 1024`.
  - Scans arbitrary PTY batches, retaining state across `feed` calls (a
    sequence split mid-way is reassembled).
  - Strips Kitty `ESC _ G … ST` from the grid, parses `a,f,i,c,r,m,q,d` keys,
    base64-decodes payload, reassembles `m=1 … m=0` continuations per image id,
    and emits one final `PaneMedia` per image (format/position/data/delete
    flags `final_chunk`/`delete`), plus delete (`a=d`/`d=1`) media.
  - OSC 8 hyperlinks: preserved byte-for-byte in `grid_bytes`, no action
    (a rendered `z3rm-download:` link never triggers a download).
  - OSC 9 `z3rm-download:` / `z3rm-copy:` (BEL- or ST-terminated): consumed
    from the grid, emitted as typed `PaneAction` DOWNLOAD / COPY (copy value
    base64-decoded). Ordinary OSC 9 stays in the grid.
  - OSC 52: kept in `grid_bytes` for alacritty's `ClipboardStore` path and
    additionally emitted as a typed COPY `PaneAction` after base64 decoding.
  - All control accumulation bounded at 4 MiB; overflow/log malformed input and
    recover so later ordinary text is emitted.
  - No `unwrap()`; no silent fallible-error discard.
- `crates/mux_server/src/mux_server.rs`: `pub mod terminal_media;` added.
- 10 unit tests in `terminal_media.rs` covering ordinary surrounding text,
  Kitty transmit/display, `m=1` then `m=0` continuation, empty/delete/final
  flags, OSC 9 actions plus OSC 8 / OSC 52 handling, feed-boundary splits, and
  overflow recovery.

## Verification

- `timeout 180 cargo test -p mux_server terminal_media` → **10 passed; 0 failed**
  (0.04s; only the bounded parser target).

## Fix round during implementation

Initial run after assembling the scanner surfaced 5 test failures, all from
terminator handling: OSC 8/52 sequences lost their trailing `ESC \` ST byte,
OSC 9 action values kept the trailing BEL, and overflow residue leaked into
`grid_bytes`.

- ESC that begins `ST` (`ESC \`) is no longer pushed into the buffer before
  the backslash is seen — the terminator bytes are re-added when a preserved
  sequence is flushed, and excluded when only the payload/action value is
  extracted (`payload_end` trims a trailing BEL or ST).
- Kitty `ESC _ G` params/data pop the ESC on a possible ST start, aborting the
  APC cleanly when the next byte is not `\`.
- OscPassthrough/ApcPassthrough escape states keep ST bytes in the grid and
  re-dispatch aborted ESCs from escape state.
- After an overflow (`drop_overflow`) the rest of the current batch is skipped
  (`break`), so residue of the oversized sequence is not emitted; the next
  `feed` resumes at ground state and later text passes through.

## Status

DONE — commit `8e859302d4` pushed to `origin/main` (base `50b601b503`, no
force).