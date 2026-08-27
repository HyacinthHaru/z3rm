# v86 Guest TUI and Terminal Media Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Run a guest-only z3rm landing TUI through the real in-guest mux_server, with Kitty graphics, mouse/scroll, download/copy actions, and observable loading progress.

**Architecture:** `mux_server` remains authoritative for PTY state, alacritty grid, terminal modes, and scrollback. Kitty media is parsed at the guest server boundary and delivered as typed mux notifications; the GPUI client renders the notifications without parsing raw PTY bytes. The guest image is an HTTP-backed v86 9p tree containing the static i686 binaries, the landing image, and a `z3rm` command wrapper. The browser loading surface measures actual response bytes and speed before the Trunk module starts.

**Tech Stack:** Rust 2024, prost, alacritty_terminal, portable-pty, tokio, v86 9p, GPUI, WebAssembly, plain browser Fetch/Streams, ANSI/Kitty/OSC terminal protocols.

**Spec:** `docs/superpowers/specs/2026-08-27-v86-guest-tui-design.md`

## Global Constraints

- `mux_server` owns PTYs, terminal parsing, grid, scrollback, and generation; the client only renders authoritative updates.
- The client and guest use the same length-delimited protobuf frames; no second protocol or shared-memory path.
- The guest target is `i686-unknown-linux-musl`; `cargo build --no-default-features --features guest` must not pull QuickJS, SQLite, TLS, or the wasm-only server.
- Kitty APC, OSC 8, and OSC 52 parsing must never corrupt ordinary grid bytes; malformed control sequences are bounded and logged.
- Do not use `unwrap()` or silently discard fallible results; every ignored result is logged or converted into an error.
- Every completed task gets a focused check and its own commit pushed to `origin/main`.
- The TUI landing page exists only in the guest; the Astro landing page is not reintroduced.

---

### Task 1: Add typed terminal-media protocol messages

**Files:**
- Modify: `crates/mux_protocol/proto/mux.proto:657-679`
- Modify: `crates/mux_protocol/src/mux_protocol.rs:19-20`
- Test: `crates/mux_protocol/tests/round_trip.rs`

**Interfaces:**
- Produces `Notification.pane_media` carrying `PaneMedia`.
- Produces `Notification.pane_action` carrying `PaneAction` for browser actions.
- `PaneMedia` fields: `pane_id: string`, `sequence: uint64`, `image_id: uint32`, `format: uint32`, `row: int32`, `column: uint32`, `columns: uint32`, `rows: uint32`, `data: bytes`, `final_chunk: bool`, `delete: bool`.
- `PaneAction` fields: `pane_id: string`, `sequence: uint64`, `kind: enum`, `value: string`, with `DOWNLOAD=1` and `COPY=2`.

- [ ] **Step 1: Write failing protobuf round-trip tests**

Add tests that construct `Notification { event: PaneMedia(...) }` and
`Notification { event: PaneAction(...) }`, encode with `mux_protocol::frame`,
decode with `mux_protocol::unframe`, and assert every field survives exactly,
including empty data, a split chunk, and the final/delete flags.

- [ ] **Step 2: Run the focused protocol tests**

Run: `cargo test -p mux_protocol --test round_trip`
Expected: the new tests fail because the notification variants do not exist.

- [ ] **Step 3: Add the protobuf definitions and bump the minor version**

Add the two oneof entries and messages without renumbering existing fields.
Increment `PROTOCOL_VERSION.minor` from `5` to `6`; major remains `1` because
these are additive notification variants.

- [ ] **Step 4: Run the focused protocol tests**

Run: `cargo test -p mux_protocol --test round_trip`
Expected: PASS, including all existing frame-hardening tests.

- [ ] **Step 5: Commit and push**

```bash
git add crates/mux_protocol
git commit -m "mux_protocol: add terminal media and browser action notifications"
git push origin HEAD:main
```

---

### Task 2: Parse Kitty APC and terminal actions in mux_server

**Files:**
- Create: `crates/mux_server/src/terminal_media.rs`
- Modify: `crates/mux_server/src/pane.rs:33-123,730-920`
- Modify: `crates/mux_server/src/connection.rs:1450-1710`
- Modify: `crates/mux_server/Cargo.toml`
- Test: `crates/mux_server/src/terminal_media.rs` unit tests

**Interfaces:**
- `TerminalMediaScanner::feed(&mut self, bytes: &[u8]) -> ScanOutput`.
- `ScanOutput { grid_bytes: Vec<u8>, media: Vec<PaneMedia>, actions: Vec<PaneAction> }`.
- Scanner state is bounded by `MAX_CONTROL_SEQUENCE_BYTES = 4 * 1024 * 1024` and resets after overflow.
- `Pane` exposes a `set_media_hook(Box<dyn Fn(Vec<PaneMedia>) + Send + Sync>)` and an action hook with the same lifetime/ordering guarantees as the existing clipboard hook.
- `connection.rs` broadcasts each media/action event to attached clients using the existing notification fan-out and never sends the raw Kitty payload through the grid parser.

- [ ] **Step 1: Write parser tests first**

Test the following observable contracts:

```rust
#[test]
fn kitty_transmit_and_display_emits_media_and_keeps_text() {
    let mut scanner = TerminalMediaScanner::new();
    let output = scanner.feed(b"before\x1b_Ga=T,f=100,i=7,c=2,r=1,q=2;SGVsbG8=\x1b\\after");
    assert_eq!(output.grid_bytes, b"beforeafter");
    assert_eq!(output.media[0].image_id, 7);
    assert_eq!(output.media[0].columns, 2);
    assert!(output.media[0].final_chunk);
}

#[test]
fn kitty_continuation_chunks_are_reassembled_in_order() { /* assert one final media payload */ }

#[test]
fn download_and_copy_actions_are_bounded_and_decoded() { /* OSC action assertions */ }

#[test]
fn unterminated_control_sequence_is_bounded_and_does_not_drop_future_text() { /* overflow/reset */ }
```

The continuation test uses `m=1` followed by `m=0`; the action test uses the
explicit `z3rm-download:` OSC 8 URI and OSC 52 payload. The tests must assert
that normal bytes surrounding each sequence stay in `grid_bytes`.

- [ ] **Step 2: Run the focused parser tests and observe failure**

Run: `cargo test -p mux_server terminal_media`
Expected: FAIL because `terminal_media` and its scanner do not exist.

- [ ] **Step 3: Implement the bounded scanner**

Recognize `ESC _ G` through `ESC \` for Kitty APC, parse only the supported
keys (`a`, `f`, `i`, `c`, `r`, `m`, `q`, `d`), base64-decode payloads, retain
continuation state by image id, and emit `PaneMedia`. Recognize OSC 8 and
emit no action for ordinary URIs; recognize `z3rm-download:` as `DOWNLOAD`.
Recognize OSC 52 as `COPY` after base64 decoding. Preserve all unsupported or
malformed sequences as dropped control bytes, log the parse error, and resume
at the next ground-state byte.

- [ ] **Step 4: Wire scanner output into Pane and connection fan-out**

Run the scanner before `Term::advance`. Advance the terminal only with
`grid_bytes`; queue media/actions under the same commit/generation fence as
PTY output. Add hooks beside the existing clipboard hook, register them from
`handle_spawn_pane`, and broadcast typed notifications to every attached
client. Ensure media sequence numbers are monotonic per pane and that pane
removal clears pending media.

- [ ] **Step 5: Run focused tests and compile both server configurations**

Run:

```bash
cargo test -p mux_server terminal_media
cargo check -p mux_server
RUSTFLAGS="-C linker=rust-lld" cargo check -p mux_server --target i686-unknown-linux-musl --no-default-features --features guest
```

Expected: all focused tests pass and all commands report zero errors.

- [ ] **Step 6: Commit and push**

```bash
git add crates/mux_server
git commit -m "mux_server: parse Kitty graphics and terminal actions server-side"
git push origin HEAD:main
```

---

### Task 3: Build and package the guest TUI landing program

**Files:**
- Create: `crates/z3rm_guest_tui/Cargo.toml`
- Create: `crates/z3rm_guest_tui/src/main.rs`
- Modify: root `Cargo.toml` workspace members
- Modify: `crates/z3rm_web/src/local_server.rs:50-100`
- Modify: `website/wasm/z3rm_demo/build-guest-fs.sh`
- Add: `website/public/media/z3rm-terminal-grid.png` to the guest 9p stage
- Test: `crates/z3rm_guest_tui/src/main.rs` parser/geometry tests

**Interfaces:**
- Binary name: `z3rm-tui`.
- `z3rm-tui` takes no required arguments; it reads `/mnt/z3rm-terminal-grid.png` and defaults to `/` for content links.
- Guest command wrapper `/mnt/z3rm` accepts `a`, `attach`, and `landing` by execing `/mnt/z3rm-tui`; unknown commands print usage and return status `2`.
- `local_server::ensure_pane_in_session` sends `ShellCommand { program: "/mnt/z3rm-tui", args: [], env: [] }` for the first pane.

- [ ] **Step 1: Write failing TUI geometry/action tests**

Test that a 120×32 terminal has deterministic button rectangles, wheel input
changes the page offset within `[0, page_height - viewport_height]`, an SGR
left-click inside the download rectangle emits the `z3rm-download:` hyperlink
and an OSC action, and a click outside all controls does not emit an action.

- [ ] **Step 2: Run focused tests and observe failure**

Run: `cargo test -p z3rm_guest_tui`
Expected: FAIL because the crate does not exist.

- [ ] **Step 3: Implement the no-dependency TUI**

Use libc termios and `read(2)`/`write(2)` only. On startup, save termios,
enter raw mode, switch to the alternate screen, enable SGR mouse tracking and
hide the cursor. Draw:

- a colored z3rm header and product statement;
- the real guest/GPUI/mux architecture explanation;
- a Kitty `a=T,f=100,i=1,c=56,r=12,q=2` image command using the bundled PNG;
- scrollable sections with a visible `Download server` OSC 8 link;
- a `Copy install command` button which emits OSC 52 for
  `cargo install z3rm`;
- a quit hint and page indicator.

Decode SGR mouse sequences, map buttons 64/65 to wheel offsets, map left-click
on controls to the download/copy escape sequences, redraw only after state
changes, and restore the original termios/alternate screen on every exit path.

- [ ] **Step 4: Make the first mux pane launch the TUI and package all guest files**

Set the initial `SpawnPaneRequest.command` to `/mnt/z3rm-tui`. Add the
`z3rm-tui`, `z3rm` wrapper, `z3rm-terminal-grid.png`, and server binary to the
stage. Export `PATH=/mnt:$PATH` from `start-mux.sh`. Rebuild `fs.json` and
content-addressed chunks, removing stale `.bin` files before indexing.

- [ ] **Step 5: Run guest tests and a local i686 build**

Run:

```bash
cargo test -p z3rm_guest_tui
RUSTFLAGS="-C linker=rust-lld -C strip=symbols -C panic=abort" cargo build -p z3rm_guest_tui --target i686-unknown-linux-musl --release
sh website/wasm/z3rm_demo/build-guest-fs.sh
```

Expected: tests pass, the binary is a static i386 ELF, and `fs.json` points
only to files present in `website/public/v86/fs`.

- [ ] **Step 6: Commit and push**

```bash
git add Cargo.toml crates/z3rm_guest_tui crates/z3rm_web/src/local_server.rs website/wasm/z3rm_demo/build-guest-fs.sh website/public/media website/public/v86/fs
git commit -m "v86: package the z3rm landing TUI in the guest"
git push origin HEAD:main
```

---

### Task 4: Render media and execute TUI actions in the GPUI client

**Files:**
- Modify: `crates/terminal_view/src/mux_pane.rs` notification handling and terminal projection
- Modify: `crates/terminal_view/src/terminal_element.rs` image/action painting and click handling
- Modify: `crates/terminal_view/Cargo.toml` only if a browser bridge dependency is required
- Modify: `crates/gpui_web/src/platform.rs` browser action bridge if needed
- Test: `crates/terminal_view/src/mux_pane.rs` focused mux notification tests

**Interfaces:**
- `MuxPaneView` maintains media keyed by `(image_id, sequence)` and removes it
  on delete/pane removal.
- A `PaneAction::DOWNLOAD` invokes a browser callback with `(uri, filename)`;
  a `PaneAction::COPY` invokes a browser clipboard callback with text.
- Mouse events are encoded as SGR mouse reports when the projected terminal
  mode has mouse tracking; otherwise existing scrollback behavior remains.

- [ ] **Step 1: Add failing client tests**

Cover: a media notification creates one visible image at the reported cell
position; a delete removes it; a `z3rm-download:` hyperlink produces a
browser action only on click; copy action preserves Unicode text; wheel input
produces SGR button 64/65 when mouse mode is enabled.

- [ ] **Step 2: Run focused tests to observe failure**

Run: `cargo test -p terminal_view mux_pane`
Expected: new tests fail because media/action notification handling is absent.

- [ ] **Step 3: Implement projection and interaction**

Decode PNG bytes on the client using the existing GPUI image path, retain the
render image cache, and paint media in cell coordinates after the text layer
but before the cursor. Hook notification subscriptions into the existing
`MuxDomain` subscriber path. Reuse existing hyperlink metadata for hover; on
left-click, recognize `z3rm-download:` and call the browser bridge, while
ordinary URLs continue through the normal open-url path. Use the existing
clipboard abstraction for explicit copy gestures and report denied browser
permissions visibly.

- [ ] **Step 4: Implement mouse/scroll routing**

Read terminal mode bits from the authoritative grid. Encode SGR mouse reports
with 1-based cell coordinates and the correct press/release/wheel button code.
Send them through the existing `SendInput` request; do not write directly to
v86 serial. Preserve server-side scrollback for non-mouse-mode terminals.

- [ ] **Step 5: Run focused client tests and checks**

Run:

```bash
cargo test -p terminal_view mux_pane
cargo check -p terminal_view --target wasm32-unknown-unknown
cargo check -p z3rm_web --target wasm32-unknown-unknown
```

Expected: zero errors and all focused behavior tests pass.

- [ ] **Step 6: Commit and push**

```bash
git add crates/terminal_view crates/gpui_web
 git commit -m "terminal_view: render guest media and handle terminal actions"
git push origin HEAD:main
```

---

### Task 5: Add Proto-UI loading progress with real byte rates

**Files:**
- Modify: `website/wasm/z3rm_demo/index.html` head markup/styles
- Modify: `website/wasm/z3rm_demo/v86_bridge.js`
- Test: `website/tests/e2e/v86-smoke.spec.ts`

**Interfaces:**
- DOM ids: `#loading-progress`, `#loading-progress-bar`,
  `#loading-progress-label`, `#loading-progress-detail`.
- `window.__z3rm_progress.stage(name, loaded, total)` accepts byte counters;
  `total = 0` means indeterminate.
- `window.__z3rm_progress.ready()` hides the surface only after GPUI and the
  first guest pane snapshot are ready.

- [ ] **Step 1: Add failing browser assertions**

Add an e2e assertion that the progress surface is visible during a throttled
initial load, contains a stage label and a `B/s` rate, then becomes hidden
when `data-gpui-ready="true"`. Assert a failed guest resource displays an
error instead of a fake 100% state.

- [ ] **Step 2: Implement the Proto-UI surface**

Add a compact dark panel with the existing site variables, a 4px determinate
bar, monospaced counters, status color, and accessible `role="status"` text.
Do not introduce a new frontend framework or dependency.

- [ ] **Step 3: Instrument Fetch without consuming responses**

Install a wrapper before the Trunk module script. For tracked URLs, use
`response.clone().body.getReader()` to count bytes and calculate a rolling
bytes-per-second rate; leave the original response untouched. Track wasm,
v86 runtime/kernel, and `/fs/*.bin` resources. Use determinate width only when
Content-Length is available; otherwise use an indeterminate animation and a
real byte counter.

- [ ] **Step 4: Instrument guest stages**

Update `v86_bridge.js` to report `Loading Linux guest`, `Starting guest
server`, and `Connected` through `window.__z3rm_progress`. Report errors with
the failing URL/stage and expose the retry control by reloading the page.
Call `ready()` after the Rust `data-gpui-ready` attribute and first snapshot
are both observed.

- [ ] **Step 5: Run website checks**

Run:

```bash
pnpm --dir website check
pnpm --dir website test
```

Expected: zero Astro/type/test failures.

- [ ] **Step 6: Commit and push**

```bash
git add website/wasm/z3rm_demo website/tests/e2e/v86-smoke.spec.ts
git commit -m "website: show real wasm and guest loading progress"
git push origin HEAD:main
```

---

### Task 6: CI, production deploy, and end-to-end browser verification

**Files:**
- Modify: `.github/workflows/deploy_website.yml:60-75`
- Modify: `website/tests/e2e/v86-smoke.spec.ts`
- Modify: `website/tests/e2e/site.spec.ts` only for the root-app contract

**Interfaces:**
- `pnpm build:wasm` builds the i686 guest fs before Trunk.
- Production root remains `/z3rm/gpui-demo/index.html` after the existing root
  redirect.

- [ ] **Step 1: Add the guest target to CI and preserve cache correctness**

Install both `wasm32-unknown-unknown` and
`i686-unknown-linux-musl`; run `sh build-guest-fs.sh` through the package
script; keep the existing v86 checksum verification and add a check that every
fs.json content hash exists.

- [ ] **Step 2: Add real-browser TUI assertions**

The e2e flow waits for the visible GPUI canvas, verifies the first screen
contains TUI text, clicks the download link and checks a download event, uses
the copy control under a granted clipboard permission, scrolls with the wheel,
and types `echo IN-GUEST-MUX` plus `z3rm a` to prove the commands are handled by
the guest-owned PTY and wrapper.

- [ ] **Step 3: Run the complete local website verification**

Run:

```bash
sh website/wasm/z3rm_demo/build-guest-fs.sh
pnpm --dir website build
pnpm --dir website test:e2e
```

Expected: build succeeds, all e2e tests pass, and no fallback fake landing is
visible after the real app becomes ready.

- [ ] **Step 4: Commit and push CI changes**

```bash
git add .github/workflows/deploy_website.yml website/tests/e2e
 git commit -m "ci: verify the guest TUI and in-guest mux server"
git push origin HEAD:main
```

- [ ] **Step 5: Verify the deployed site**

Wait for the `Deploy Z3rm website` workflow, require success, then drive
`https://cyjin-yl.github.io/z3rm/` in a real GPU browser. Record evidence for:

- progress panel with a measured rate;
- visible GPUI z3rm chrome and guest TUI text/image;
- wheel scrolling and mouse click;
- browser download and clipboard copy;
- `z3rm a` resolving in the guest;
- `echo` executing through the guest mux_server PTY;
- no client-side `WasmMuxServer` construction.

- [ ] **Step 6: Push any verification-only test fix as its own commit**

If production exposes a real mismatch, fix only that mismatch, run the focused
check, commit with an imperative title, push, and repeat the production check.
Do not mark the plan complete until every bullet above has current evidence.
