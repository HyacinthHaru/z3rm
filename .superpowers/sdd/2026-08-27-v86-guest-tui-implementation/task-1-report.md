# Task 1 Report — mux_protocol PaneMedia/PaneAction notifications

## Goal
Add additive Notification oneof variants `PaneMedia` and `PaneAction` to the
mux wire protocol so the v86 guest terminal can push canvas/framebuffer media
snapshots and structured guest actions to the client. Bump only the protocol
minor version; add behavior-focused frame/unframe round-trip tests; commit and
push. Do not touch mux_server or client implementation.

## Changed files

- `crates/mux_protocol/proto/mux.proto`
  - `Notification.oneof event`: added `PaneMedia pane_media = 17` and
    `PaneAction pane_action = 18` (fields 1–16 untouched; all existing
    notifications preserved).
  - New message `PaneMedia { pane_id, media_type, data, width, height,
    timestamp_ms }` — binary media payload shape aligned with v86
    `screen_make_screenshot()` (PNG bytes, pixel dimensions, monotonic
    ms clock for ordering/dedupe).
  - New message `PaneAction { pane_id, action_name, payload, timestamp_ms }`
    — opaque client-forwarded action string + JSON payload + ordering clock.
- `crates/mux_protocol/src/mux_protocol.rs`
  - `PROTOCOL_VERSION.minor` 5 → 6 (major unchanged; forward-compatible).
- `crates/mux_protocol/tests/round_trip.rs`
  - `test_pane_media_notification_round_trip` — verifies `pane_id`,
    `media_type`, raw `data` (PNG magic prefix), `width`, `height`,
    `timestamp_ms` all survive frame → unframe.
  - `test_pane_action_notification_round_trip` — verifies `pane_id`,
    `action_name`, JSON `payload`, `timestamp_ms` round-trip.
  - `test_protocol_version_minor_bumped_for_media_and_action` — asserts
    `PROTOCOL_VERSION.minor >= 6` as a contract lock.

## Commands & output

```
$ cargo test --package mux_protocol --test round_trip
   Compiling mux_protocol ...
   Finished `test` profile [unoptimized + debuginfo]
   Running tests/round_trip.rs
   running 16 tests
   test read_file_pagination_round_trip                       ... ok
   test recovery_messages_round_trip                          ... ok
   test test_client_identity_round_trip                       ... ok
   test test_file_version_round_trip                          ... ok
   test test_frame_unframe_round_trip                         ... ok
   test test_grid_diff_round_trip                             ... ok
   test test_attach_request_with_identity                     ... ok
   test test_pane_action_notification_round_trip              ... ok
   test test_pane_bell_notification                           ... ok
   test test_pane_media_notification_round_trip               ... ok
   test session_layout_changed_snapshot_round_trip            ... ok
   test test_pane_title_changed_notification                  ... ok
   test test_protocol_version_minor_bumped_for_media_and_action ... ok
   test test_shadow_file_version_envelope_round_trip          ... ok
   test test_scrollback_request_response_round_trip           ... ok
   test test_full_snapshot_serialization                      ... ok
   test result: ok. 16 passed; 0 failed; 0 ignored
```

Second run (after rebase onto remote main) identical: 16 passed.

```
$ git commit -m "Add PaneMedia/PaneAction notifications and bump protocol minor to 6"
$ git push origin main
   74e77c17fb..b087ed4c04  main -> main
```

## Commit

`b087ed4c04` — `Add PaneMedia/PaneAction notifications and bump protocol minor to 6`

## Notes / concerns

- Push was rejected on first attempt (remote had diverged). Rebased onto
  `origin/main`, verified tests still pass, then pushed successfully.
- No mux_server or client source was modified — server-side emit and
  client-side consume are deferred to later tasks per the v86 guest TUI
  plan.
- Generated Rust types (via `prost-build` in `build.rs`) now expose
  `PaneMedia` and `PaneAction` at `mux_protocol::proto::{PaneMedia, PaneAction}`
  through the existing `pub use proto::*` in `mux_protocol.rs`; verified by
  the tests compiling and running.
- Fields are additive only; existing field numbers (1–16 in `Notification`,
  plus all other messages) unchanged — forward-compatible with 1.5 clients
  (they will simply ignore the unknown 17/18 tags).
- The `task-1-brief.md` file was not present in the workspace; the
  requirements above were taken from the task context contract and the
  surrounding codebase conventions (existing `PaneTitleChanged`/`PaneBell`
  test style, proto commenting style, minor-version bump pattern).
