# Per-window mux connection state and recovery

## Scope

Issue #14 requires every mux-backed GPUI window to own a persistent, actionable connection state. The state applies equally to local socket and SSH-forwarded domains; transport-specific recovery stays behind the window binding. One failed domain must never change another window's indicator or retry loop.

## Existing foundation

`crates/z3rm/src/main.rs` already gives each mux window its own `MuxDomain`, session id, optional `SshSession`, and status item. `MuxDomain::reconnect_local_in_place` and `reconnect_at_path_in_place` preserve the domain identity and broadcast an authoritative `SessionLayoutChanged` snapshot after attach. The notification watcher applies that snapshot to layout, pane membership, focus, zoom, sidebar state, and extension-visible state.

The current implementation is incomplete: it stores only a three-value phase, probes only SSH windows, performs only user-triggered SSH recovery, hides the healthy state, and loses retry progress and the last error.

## State model

Replace the phase-only enum with an immutable view state carried by each `MuxWindow` and mirrored into its `MuxConnectionStatusItem`:

- `Connected`: the latest probe or reconnect RPC succeeded.
- `Reconnecting { attempt }`: one transport recovery is in flight. `attempt` starts at one and increases for consecutive failures.
- `Offline { attempts, last_error }`: the latest recovery failed or the probe detected loss before the first recovery begins. `last_error` is a complete user-readable error chain.

Each binding also owns:

- a retry signal channel used by the status-bar control and `mux::Reconnect`;
- one stored monitor task;
- a monotonically increasing binding epoch used to reject completion from a domain that has since been rebound.

No process-global connection phase exists. Status lookup, retry signaling, state transitions, and error text are always keyed by `WindowId` and validated against the binding's `Arc<MuxDomain>` plus epoch.

## Recovery worker

`open_mux_window_with_snapshot` and every window rebind start one window-owned monitor. The task is stored in `MuxWindow`; replacing or removing the binding drops and cancels it. The task keeps only a weak domain reference and the `WindowId`, so it cannot keep a closed window, mux socket, or SSH tunnel alive.

The worker waits for either the periodic probe interval or an explicit retry signal:

1. A successful probe publishes `Connected` and resets attempt/backoff state.
2. A failed probe transitions only that binding to `Reconnecting { attempt }` and runs recovery.
3. A local binding first calls `ensure_daemon_running`, then `reconnect_local_in_place`.
4. A remote binding rebuilds its SSH forwarding endpoint through `SshSession::reconnect`, then calls `reconnect_at_path_in_place`.
5. Success publishes `Connected`, resets the backoff, and relies on the reconnect helper's full attach snapshot for authoritative UI reconciliation.
6. Failure publishes `Offline { attempts, last_error }`, waits with bounded exponential backoff, and retries automatically. An explicit retry interrupts the wait and begins immediately without creating a second concurrent attempt.

The monitor exits when its weak domain cannot upgrade, its binding is gone/replaced, its retry channel closes, or its window context is no longer available. Completion for an obsolete domain/epoch is ignored.

## User interface

The window status bar always renders one compact state:

- `Connected` in the success/muted treatment.
- `Reconnecting · attempt N` in the warning treatment.
- `Offline · N attempts` in the error treatment.

Reconnecting and offline states expose an immediate Retry control. Offline state exposes the complete last error as a tooltip/detail surface and a Copy Error control that writes the full text to the clipboard. The native `mux::Reconnect` action sends the same per-window retry signal, so mouse and keyboard behavior cannot diverge. Core actions remain native and available when QuickJS is unavailable.

## Authoritative restoration

No disconnected-period notification is replayed. A successful in-place reconnect must attach using the existing logical window id and receive a full `SessionSnapshot`. `MuxDomain` publishes that snapshot in the synthetic lifecycle notification; the existing window notification path reconciles panes, tabs, layout, focus, zoom, sidebar state, and extension-visible state from it. The status changes to `Connected` only after attach and snapshot publication succeed.

## Error handling

Every recovery error is retained as a formatted error chain and shown in the affected window. Errors are never discarded. A failed retry leaves the window usable for native retry, settings, detach, and shutdown actions. Failure in one binding does not mutate the process-wide app domain or any other `MuxWindow`.

## Verification

Unit tests cover:

- valid phase/attempt/error transitions;
- duplicate retry signals coalescing into one in-flight attempt;
- stale domain/epoch completion rejection;
- one window's state transition leaving a second window unchanged;
- rebind and window removal cancelling/replacing the owned monitor state;
- status text and full error detail projection;
- native reconnect action registration.

Integration tests use controllable local domains to cover disconnect, automatic retry, manual retry interruption, cancellation on close, and authoritative snapshot reconciliation including layout and focus. Existing SSH helpers remain unit-tested without requiring a live SSH host; local and remote recovery share the same state-machine tests.
