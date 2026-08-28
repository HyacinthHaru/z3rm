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
    flags `final_chunk`/`delete`; delete media is emitted for `a=d` and the
    modern targeted form `a=q,d=i`).
  - OSC 8 hyperlinks: preserved byte-for-byte in `grid_bytes`, no action
    (a rendered `z3rm-download:` link never triggers a download).
  - OSC 9 `z3rm-download:` / `z3rm-copy:` (BEL- or ST-terminated): consumed
    from the grid, emitted as typed `PaneAction` DOWNLOAD / COPY (copy value
    base64-decoded). Ordinary OSC 9 stays in the grid.
  - OSC 52: preserved byte-for-byte in `grid_bytes` for alacritty's existing
    `ClipboardStore` hook path (clipboard hook → ServerClipboard). No
    `PaneAction` is emitted — the clipboard hook is the sole copy path.
  - All control accumulation bounded at 4 MiB; overflow/log malformed input and
    recover so later ordinary text is emitted.
  - No `unwrap()`; no silent fallible-error discard.
- `crates/mux_server/src/mux_server.rs`: `pub mod terminal_media;` added.
- 22 unit tests in `terminal_media.rs` covering ordinary surrounding text,
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


## Fix round 1 (review) — reviewer `agent://ReviewTerminalMediaParser`

Initial review verdict: *incorrect* — the committed scanner violated the OSC 9
semicolon wire format, Kitty continuation/`q`/delete contracts, the aggregate
4 MiB recovery/bounding contract, passthrough byte preservation, and the
single-effect OSC 52 path. Every finding below is implemented and covered by a
focused test; the fixture set grew from 10 to 22.

1. **Overflow recovery** (Critical 1): on overflow the scanner now enters a
   `Discard`/`DiscardEsc` state that survives across `feed` calls and consumes
   bytes until a BEL or ST (`ESC \`) terminator; the remaining bytes of the
   overflowing feed are resumed *after* the terminator instead of being either
   dropped wholesale or emitted as text. `buffer_byte`/`buffer_byte_after_escape`
   also give the terminator-on-the-overflowing-byte case correct recovery
   (BEL/ST closes Discard on the current byte). Tests:
   `overflow_discards_until_terminator_then_resumes`,
   `overflow_cross_feed_discard_then_resume`, `overflow_bel_terminator_resumes`,
   new `overflow_osc_cross_feed_discards_until_terminator`.
2. **Aggregate pending-media budget** (Critical 2): the scanner tracks a
   *single* active non-interleaved Kitty transfer (`pending: Option<(u32,
   PendingMedia)>`) instead of one entry per image id, and every append —
   continuation chunks and the final chunk alike — is preflighted with
   `checked_add` against `MAX_REASSEMBLED_MEDIA_BYTES` before extending,
   rejecting (and dropping the pending transfer) instead of allocating past
   the cap. Test: new
   `kitty_pending_payload_budget_is_checked_before_final_append`.
3. **APC passthrough byte preservation** (Important 2): a non-ST byte after
   ESC inside a non-Kitty APC is now consumed into the buffer (the previous
   code dropped it); consecutive ESCs are retained, and BEL terminates a
   pass-through APC as well as OSC. Tests: existing
   `apc_passthrough_non_st_preserves_byte`, new
   `apc_passthrough_bel_after_non_st_esc_terminates`.
4. **OSC 9 semicolon wire format** (Critical 3): actions match
   `OSC 9;z3rm-download;<uri>` and `OSC 9;z3rm-copy;<base64>` (semicolon after
   the action name, per the design spec), BEL- or ST-terminated. Tests updated
   to the semicolon format (`download_and_copy_actions_are_bounded_and_decoded`).
5. **Image-id inheritance** (Critical 4): the final chunk `m=0` inherits the
   active transfer's image id when it omits `i`
   (`parsed.image_id.or_else(… pending …)`), so the reassembled image is
   published. Test: `kitty_continuation_without_image_id_uses_pending`.
6. **`q` is response suppression** (Important 2): Kitty payload bytes are
   always base64-decoded for every `q` (0/1/2); the value is parsed and
   ignored as a quiet flag, never interpreted as an encoding selector. Test:
   `kitty_q_is_response_suppression_not_encoding` (all three values).
7. **`d` is a deletion selector** (Important 2): `d` parses as a character
   selector; `d=i` deletes by image id under both `a=d` (classic) and `a=q`
   (modern targeted form). Tests: `kitty_delete_emits_delete_media`,
   `kitty_delete_with_d_i_selector`.
8. **Delete media flags** (Important 2): delete notifications carry empty
   `data`, `delete=true`, and `final_chunk=false` (final_chunk denotes the
   last data chunk, not a deletion). Asserted in the delete tests.
9. **OSC 52 single effect** (Critical 5): OSC 52 is preserved byte-for-byte in
   `grid_bytes` for alacritty's existing `ClipboardStore` hook and emits **no**
   `PaneAction` — the clipboard hook is the sole copy path (design ruling, no
   double copy). Test: `osc52_preserves_grid_and_emits_no_action`.
10. **Consecutive ESC preservation** (Important 2): `ESC ESC …` no longer
    collapses; each ESC is emitted in order (the trailing one stays deferred
    only while a sequence introducer is still possible). Test:
    `consecutive_esc_bytes_are_preserved` (input `ESC ESC text` → both ESCs
    preserved).
11. **Crop offsets not cell coordinates** (Important 2): lowercase `x`/`y`
    keys are parsed and ignored; `PaneMedia` row/column stay 0 for later Pane
    wiring to supply cursor-cell placement. Test:
    `kitty_ignores_x_y_crop_offsets`.

Minors also fixed: BEL never enters decoded Kitty data (handled before
buffering; `kitty_bel_terminated_does_not_include_bel_in_data`), and OSC 9
action values whose UTF-8 or base64 decoding fails are logged and dropped
(never lossily converted; the `from_utf8_lossy` paths are gone).

Verification after the fixes (bounded target only):

```text
$ timeout 180 cargo test -p mux_server terminal_media
running 22 tests
test result: ok. 22 passed; 0 failed; 0 ignored
```

Status: fix commit `6d8382b4a1` pushed to `origin/main` (base `63b04cafc7`, no
force).

## Fix round 2 (scoped re-review)

The re-review found one malformed-OSC recovery gap: `OscPayloadEscape` retained a non-ST byte and swallowed subsequent CSI/text. The arm now clears the typed OSC buffer and re-dispatches that byte from `Escape`, with regression coverage in `malformed_osc_payload_aborts_on_non_st_esc_and_resumes`.

Verification: `cargo test -p mux_server terminal_media` — 23 passed; 0 failed.

Follow-up commit: `46943df3d3` pushed to `origin/main` (base `12f126aae3`, no force).
## Pane / connection wiring

The reviewed scanner is now on the server-canonical PTY path. `ReadLoopState`
owns one persistent `TerminalMediaScanner` for both native PTY reads and the
guest-output entry point. `Pane::process_pty_bytes` feeds the scanner first;
DEC-2026, OSC 7/133, history observation, and alacritty all receive only
`ScanOutput.grid_bytes`. Raw `PaneOutput` remains the documented lossy wakeup.

`ScanOutput` now carries an internal ordered event ledger with grid-byte
offsets. Pane merges those offsets with OSC 133 boundaries while advancing the
authoritative alacritty terminal, captures `a=T` row/column at the Kitty event
offset, and stamps media/actions from one Pane-owned monotonic sequence. Media
adds/deletes publish a generation under the same commit fence. Typed events are
delivered after commit through the registered media/action hooks; hooks are
`Send + Sync`, session-scoped in `connection.rs`, and use weak registry
references so pane removal does not create an Arc cycle. Recovered, spawned,
and split panes install the hooks alongside clipboard/subscriber registration;
the connection hook targets only the owning session's lifecycle subscribers,
so each attached client receives one length-delimited notification. OSC 52
continues through the existing ClipboardStore hook only.

## Wiring verification

All commands used the shared bounded target directory
`/run/media/ezra/13D010B6FDBC1A06/projects/z3rm-target-verify`.

```text
$ cargo test -p mux_server terminal_media
test result: ok. 22 passed; 0 failed; 0 ignored

$ cargo test -p mux_server --lib -- kitty_media_is_stripped_but_surrounding_text_reaches_grid
test result: ok. 1 passed; 0 failed; 0 ignored

$ cargo test -p mux_server --lib -- media_
test result: ok. 5 passed; 0 failed; 0 ignored

$ cargo check -p mux_server --lib
Finished successfully (warnings only; exit 0).

$ cargo check -p mux_server --lib --no-default-features --features guest
Finished successfully (warnings only; exit 0).

$ RUSTFLAGS="-C linker=rust-lld" cargo check -p mux_server \
    --target i686-unknown-linux-musl --no-default-features --features guest
Finished successfully (warnings only; exit 0).
```

Concerns: the existing workspace emits unrelated warnings (dependency patch
and unused/dead-code warnings). The cursor-specific test was not rerun after
the final source edit; the parser, grid-strip, media-order/generation, native
check, and i686 guest check above all completed successfully.

## Fix round 3 — late hooks and split-feed placement

The scoped re-review exposed two wiring issues. Completed typed events are now
kept in one bounded per-pane queue until their matching media/action hook is
installed. Queue draining is serialized without retaining the queue or hook
lock during callbacks; media and actions therefore retain one cross-type order
through sequential connection registration. Panes without a session do not
retain unobserved events, and queue overflow drops only the newest event with a
warning.

Kitty continuation placement now emits an internal `Placement` boundary for
the initial `m=1,a=T` chunk. `ReadLoopState` retains that single cursor cell
across reads, and the final media event consumes it by image id. Final event
offsets remain local to the completing feed; no prior-feed offset is reused for
terminal advancement. The regression feeds ordinary bytes after the initial
chunk and completes the transfer in a shorter later feed.

The first run reproduced both failures:

```text
$ cargo test -p mux_server --lib -- media_event_waits_for_late_hook_registration
test result: FAILED; 0 passed; 1 failed
assertion `left == right` failed: left: 0, right: 1

$ cargo test -p mux_server --lib -- kitty_continuation_preserves_initial_display_cursor_across_feeds
test result: FAILED; 0 passed; 1 failed
assertion `left == right` failed: left: (4, 11), right: (4, 6)
```

After the fixes, bounded verification completed as follows (shared target
directory `/run/media/ezra/13D010B6FDBC1A06/projects/z3rm-target-verify`):

```text
$ cargo test -p mux_server --lib -- media_
test result: ok. 6 passed; 0 failed; 0 ignored

$ cargo test -p mux_server --lib -- kitty_continuation_preserves_initial_display_cursor_across_feeds
test result: ok. 1 passed; 0 failed; 0 ignored

$ cargo test -p mux_server --lib -- kitty_delete
test result: ok. 2 passed; 0 failed; 0 ignored

$ cargo test -p mux_server terminal_media
test result: ok. 23 passed; 0 failed; 0 ignored
```

The native and i686 guest checks are rerun below after the final source edit.
Existing dependency-patch and unrelated unused/dead-code warnings remain.

```text
$ cargo check -p mux_server
Finished successfully (warnings only; exit 0).

$ RUSTFLAGS="-C linker=rust-lld" cargo check -p mux_server \
    --target i686-unknown-linux-musl --no-default-features --features guest
Finished successfully (warnings only; exit 0).
```

## Fix round 4 — lossless session hook queue and feed-local offsets

The follow-up review correctly rejected the bounded pending-event cap: a
session-scoped pane must not silently lose media/actions while connection hooks
are installed sequentially. The Pane queue now retains every pending typed
event for the pane lifetime, drains only from the front when the matching hook
exists, and invokes callbacks without queue or hook locks. Panes without a
session still retain no unobserved events unless a matching hook is already
installed.

The scanner now resets only the transient control-sequence offset when a
control sequence itself crosses a feed boundary. The pending continuation's
initial `a=T` cursor is carried by the internal `Placement` event and persistent
Pane cursor state; final media offsets remain local to the completing feed.
Delete media returns immediately after its single typed event, preventing a
second fall-through media notification.

```text
$ cargo test -p mux_server terminal_media
test result: ok. 23 passed; 0 failed; 0 ignored

$ cargo test -p mux_server --lib -- media_
test result: ok. 6 passed; 0 failed; 0 ignored
```

```text
$ cargo check -p mux_server
Finished successfully (warnings only; exit 0).

$ RUSTFLAGS="-C linker=rust-lld" cargo check -p mux_server \
    --target i686-unknown-linux-musl --no-default-features --features guest
Finished successfully (warnings only; exit 0).
```

## Fix round 5 — drain handoff race

The final review identified a registration race in the lossless pending-event
drainer: a hook setter could observe an active drainer while the drainer was
about to requeue an event with no matching hook, leaving that event stuck. A
`retry_requested` handoff flag now causes the drainer to restart after the
requeue, while still invoking callbacks without queue or hook locks. The pane
drop regression was also made deterministic by using an exiting child and a
bounded wait for the reader's strong reference to release.

Final focused verification:

```text
$ cargo test -p mux_server --lib -- media_
cargo test: 6 passed (1 suite, 283 filtered, 0.00s)

$ cargo test -p mux_server --lib -- hook_registration_during_drain_releases_pending_action
cargo test: 1 passed (1 suite, 288 filtered, 0.00s)

$ cargo test -p mux_server --lib -- pane_drop_releases_media_hook_without_arc_cycle
cargo test: 1 passed (1 suite, 288 filtered, 0.00s)

$ cargo test -p mux_server terminal_media
cargo test: 23 passed (5 suites, 312 filtered, 0.00s)

$ cargo check -p mux_server
Finished successfully (warnings only; exit 0).

$ RUSTFLAGS="-C linker=rust-lld" cargo check -p mux_server \
    --target i686-unknown-linux-musl --no-default-features --features guest
Finished successfully (warnings only; exit 0).
```

The `media_` filter includes six matching tests; all six pass. Existing
workspace dependency-patch and unused/dead-code warnings remain unrelated.
