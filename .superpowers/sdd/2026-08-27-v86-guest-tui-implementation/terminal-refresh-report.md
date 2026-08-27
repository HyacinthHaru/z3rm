# Terminal Refresh Report — intermittent pane repaint lag

## Symptom
Pane output sometimes arrives at the client but the visible terminal does not
repaint promptly. Independent of the in-progress server terminal-media parser;
scoped entirely to the client refresh/invalidation path.

## Root cause
The `MuxPaneView` notification listener (`start_notification_listener` in
`crates/terminal_view/src/mux_pane.rs`) gated **every** flush on a fixed
`8ms` quiet-window timer: after a `PaneOutput`/`PaneDirty` notification set
`pending_dirty`, the listener slept 8ms (awaiting
`cx.background_executor().timer(8ms)`) before it drained the channel and
called `flush_pending` → `schedule_fetch`.

That timer was a pure latency tax, redundant on two layers that already
provide at-most-once coalescing:

1. **Server side** — `mux_server`'s `AdaptiveCoalescer` (`crates/mux_server/src/coalescing.rs`, §16.3)
   already bounds `PaneDirty` cadence (0ms interactive, 2ms normal, 8–16ms
   high-throughput with drop of intermediate frame notifications). The
   client-side window added delay on top of the server's own batching.
2. **Client side** — `schedule_fetch`'s `fetch_in_flight`/`fetch_pending`
   pair coalesces any notifications that arrive while a fetch is in flight
   into exactly one catch-up pull. No client timer was needed for
   at-most-once semantics.

For a single notification (the common interactive case — a keystroke echo), a
refresh therefore took `8ms + fetch RTT + frame` instead of `fetch RTT + frame`,
violating the §15.5 interactive budget (local keystroke → screen p95 < 16ms)
and reading as "output arrived but no repaint".

## Fix
`crates/terminal_view/src/mux_pane.rs` — `start_notification_listener`:
- Removed the `8ms` quiet-window timer and the fragile
  `err.to_string().contains("empty")` error matching.
- The listener now flushes on the next executor tick as soon as a dirty
  notification is accumulated, draining whatever is already queued first so a
  tight burst still produces a single `fetch_grid_update` pull.
- At-most-once dirty semantics and the authoritative pull path are unchanged;
  the in-flight/pending pair re-arms exactly one catch-up fetch for anything
  that lands mid-fetch.

## Files
- `crates/terminal_view/src/mux_pane.rs` — production fix
  (`start_notification_listener`); test helpers `read_request_or_quiet`,
  `serve_prompt_refresh`, `serve_dirty_burst`; tests
  `pane_dirty_triggers_prompt_refetch_without_a_coalescing_delay` and
  `repeated_dirty_notifications_coalesce_into_one_fetch`.
- No changes to `mux_server`, `mux_protocol` schema, or `terminal_media.rs`.

## Tests
- `pane_dirty_triggers_prompt_refetch_without_a_coalescing_delay` — broadcast
  a lone `PaneDirty`, pump the executor with **no** clock advancement, assert
  the refetch is already on the wire and the terminal repaints to generation 2
  ("hi" grid). **Fails before, passes after.**
- `repeated_dirty_notifications_coalesce_into_one_fetch` — broadcast an 8-
  notification burst, assert the mock server receives **exactly one** post-
  dirty fetch (not 8, not 0). **Fails before, passes after.**
- Existing `dirty_during_fetch_triggers_cursor_catch_up`,
  `new_fetches_generation_zero_for_a_quiet_pane` unchanged and still passing
  (generation/error-handling semantics preserved).

### Exact output

Before the fix (listener temporarily restored to HEAD version):

```
$ cargo test --package terminal_view --lib -- pane_dirty_triggers_prompt_refetch_without_a_coalescing_delay
thread 'mux_pane::tests::pane_dirty_triggers_prompt_refetch_without_a_coalescing_delay' panicked at crates/terminal_view/src/mux_pane.rs:3532:13:
pane did not refetch promptly after PaneDirty
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 106 filtered out; finished in 5.03s

$ cargo test --package terminal_view --lib -- repeated_dirty_notifications_coalesce_into_one_fetch
test result: FAILED. 0 passed; 1 failed; 0 ignored; 0 measured; 106 filtered out; finished in 5.43s
```

After the fix (focused set covering the changed path):

```
$ cargo test --package terminal_view --lib -- pane_dirty_triggers_prompt_refetch repeated_dirty_notifications_coalesce dirty_during_fetch_triggers_cursor_catch_up new_fetches_generation_zero
running 4 tests
test mux_pane::tests::dirty_during_fetch_triggers_cursor_catch_up ... ok
test mux_pane::tests::pane_dirty_triggers_prompt_refetch_without_a_coalescing_delay ... ok
test mux_pane::tests::new_fetches_generation_zero_for_a_quiet_pane ... ok
test mux_pane::tests::repeated_dirty_notifications_coalesce_into_one_fetch ... ok
test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 103 filtered out; finished in 0.35s

$ cargo test --package terminal_view --lib
test result: ok. 107 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.29s

$ cargo check -p terminal_view
    Finished `dev` profile [unoptimized + debuginfo] target(s) in 1m 10s
```

## Commit
`2e0b530d2e` — `terminal_view: Flush pane refresh on the next executor tick instead of an 8ms coalescing timer`
pushed to `origin/main` (fast-forward `2de779f5d5..2e0b530d2e`, no force; upstream
parser work advanced disjoint files only — verified `0e60c6bfaf..2de779f5d5` did
not touch `crates/terminal_view/src/mux_pane.rs`).

## Concerns
None blocking. The trailing 300ms quiet drain in `serve_dirty_burst` adds a
~300ms join cost to that test after the client settles; the first post-dirty
fetch still must arrive within 5s, so a missed prompt refresh fails the test
deterministically.