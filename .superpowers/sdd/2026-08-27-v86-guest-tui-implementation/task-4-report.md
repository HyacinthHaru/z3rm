# Task 4 report — client media rendering and TUI actions

## Scope

Implemented the client-side half of the mux media/action protocol in:

- `crates/terminal_view/src/mux_pane.rs`
- `crates/terminal_view/src/terminal_element.rs`

Follow-up hardening and browser integration also touched
`crates/mux/src/mux.rs`, `crates/terminal_view/Cargo.toml`, and the wasm host
path in `crates/z3rm_web`. The unrelated untracked
`crates/fs/src/wasm_fs.rs` was left untouched.

## Implementation

- Added `PaneMediaStore` to `MuxPaneView`, keyed by `(image_id, sequence)`.
  It reassembles notification chunks, validates the PNG format, decodes with
  `terminal::kitty_graphics::decode_encoded_image`, and retains the resulting
  `Arc<gpui::RenderImage>` for rendering.
- Media is projected through `TerminalElement::new_with_media` and painted at
  the server-reported `(row, column)` and `(columns, rows)` cell rectangle,
  after text and Kitty images but before IME/cursor painting.
- Delete notifications remove every cached frame for the image id and release
  decoded images. `PaneRemoved` clears the entire media cache and emits the
  existing `CloseRequested` event.
- Added `BrowserDownloadCallback` and `BrowserClipboardCallback` injection APIs
  on `MuxPaneView`. Typed DOWNLOAD actions pass the URI and a sanitized
  basename (including query/fragment, separator, dot-name, and control-byte
  handling); typed COPY actions preserve the exact decoded Unicode string.
  COPY falls back to the existing GPUI clipboard abstraction when no callback
  is installed.
- Added safe `z3rm-download:` hyperlink handling in `TerminalElement`: a
  left-click in a non-mouse-mode pane is captured on press and invokes the
  download callback only when release remains on the same target. Private
  links are consumed for all modifiers and never fall through to URL
  navigation; ordinary URLs retain the existing `Terminal::mouse_up` path.
  Mouse-mode panes retain the SGR input path, allowing the guest to produce
  typed actions.
- Existing authoritative `Terminal::last_content().mode` and DisplayOnly input
  sink continue to govern mouse tracking. Focused coverage verifies SGR wheel
  reports use button 64/65 and are delivered through the input sink used by
  `MuxPaneView`'s `MuxDomain::send_input` path.

## Follow-up hardening

- Media metadata is inherited from the first chunk for a `(image_id, sequence)`
  key, format 100 requires a PNG signature, and placement dimensions are
  bounded before painting.
- The client accounts decoded and in-flight media bytes under a 256 MiB
  per-pane limit, releases encoded allocations after decode, rejects new frames
  instead of evicting live entries when its 256-frame cap is reached, orders
  visible frames by sequence, releases entries on delete/pane removal, and
  suppresses live media while viewing scrollback.
- `PaneMedia` and `PaneAction` are reliable notification classes in `MuxDomain`,
  so saturated subscribers backpressure (or use the wasm overflow queue)
  instead of silently dropping typed events.

## Focused tests and checks

The required red test-first run was performed before implementation; it failed
because `PaneMediaStore`, download-target parsing, and COPY handling were
absent.

Passing focused behavior tests:

```text
cargo test -p terminal_view mux_notifications_apply_media_delete_and_browser_actions
1 passed

cargo test -p terminal_view authoritative_mouse_mode_wheel_uses_sgr_button_64_and_65
1 passed

cargo test -p terminal_view pane_action_
2 passed

cargo test -p terminal_view pane_media_notifications_create_and_delete_visible_images
1 passed
```

The final required focused command was rerun after the hardening and browser
host integration:

```text
cargo test -p terminal_view mux_pane
37 passed; 0 failed
```

The focused suite covers media add/decode/position/delete and pane removal,
metadata inheritance, cache-limit retention, typed DOWNLOAD/COPY dispatch
including Unicode, private-link gating, and SGR wheel buttons 64/65.

Wasm checks completed with exit status 0 (warnings only):

```text
cargo check -p terminal_view --target wasm32-unknown-unknown
cargo check -p z3rm_web --target wasm32-unknown-unknown
```

No formatter, linter, or project-wide test suite was run.

## Remaining risk

- The wasm host now installs default browser download/clipboard callbacks in
  every `MuxPaneView` construction path, including split panes. Unsupported
  download schemes and rejected clipboard promises are logged by that host
  seam; native hosts can still inject their own callbacks or use the GPUI
  clipboard fallback.
