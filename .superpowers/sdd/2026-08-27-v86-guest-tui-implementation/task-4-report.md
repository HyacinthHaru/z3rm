# Task 4 report — client media rendering and TUI actions

## Scope

Implemented the client-side half of the mux media/action protocol in:

- `crates/terminal_view/src/mux_pane.rs`
- `crates/terminal_view/src/terminal_element.rs`

No additional source file was required. `crates/terminal_view/Cargo.toml`,
`crates/gpui_web/src/platform.rs`, website files, and
`crates/fs/src/wasm_fs.rs` were not modified. The pre-existing untracked
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
- Added `BrowserDownloadCallback` and `BrowserClipboardCallback` injection
  APIs on `MuxPaneView`. Typed DOWNLOAD actions pass the URI and a safe
  basename (root paths use `download`); typed COPY actions preserve the exact
  decoded Unicode string. COPY falls back to the existing GPUI clipboard
  abstraction when no browser callback is installed.
- Added safe `z3rm-download:` hyperlink handling in `TerminalElement`: a
  left-click in a non-mouse-mode pane is captured on press and invokes the
  download callback only when release remains on the same target. Private
  links never fall through to URL navigation when no callback is installed;
  ordinary URLs retain the existing `Terminal::mouse_up` path. Mouse-mode
  panes retain the SGR input path, allowing the guest to produce typed actions.
- Existing authoritative `Terminal::last_content().mode` and DisplayOnly
  input sink continue to govern mouse tracking. Focused coverage verifies
  SGR wheel reports use button 64/65 and are delivered through the input sink
  used by `MuxPaneView`'s `MuxDomain::send_input` path.

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

The final required focused command was also run:

```text
cargo test -p terminal_view mux_pane
33 passed; 2 failed
```

The two failures are existing tests unrelated to this change:

- `mux_pane::tests::a_terminal_selection_is_reported_as_a_text_range`
- `mux_pane::tests::dirty_during_fetch_triggers_cursor_catch_up`

Both failures were present in the initial focused run before the new client
behavior was added; the new media/action/mouse tests pass in the same suite.

Wasm checks completed with exit status 0 (warnings only):

```text
cargo check -p terminal_view --target wasm32-unknown-unknown
cargo check -p z3rm_web --target wasm32-unknown-unknown
```

No formatter, linter, or project-wide test suite was run.

## Concerns

- The host must install the two callback types with
  `MuxPaneView::set_browser_action_callbacks` (or the separate setters). This
  keeps DOM/browser code out of `terminal_view`; no website bridge was changed
  in Task 4.
- `MuxDomain` currently classifies `PaneMedia` and `PaneAction` as lossy in
  its notification fan-out policy. The client consumes them correctly when
  delivered, but a saturated subscriber can still lose a typed event; fixing
  that policy belongs outside the Task 4 file ownership.
- Browser permission rejection is necessarily handled by the injected host
  callback (the callback is intentionally browser-runtime agnostic). The
  existing GPUI clipboard fallback remains available for non-browser hosts.
