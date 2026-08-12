# Per-window mux connection state implementation plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Give each local or SSH mux window an independent persistent status, automatic and manual recovery, actionable error details, and authoritative full-state reconciliation.

**Architecture:** Extend the existing `MuxWindow` binding in `crates/z3rm/src/main.rs`; do not add a second connection registry. A stored window-owned monitor serializes probe and retry work, calls the existing in-place reconnect APIs, and publishes a richer state to the existing status item. The existing synthetic full-snapshot lifecycle event remains the only restoration path.

**Tech Stack:** Rust, GPUI entities/tasks, async channels, `MuxDomain`, `SshSession`, mux protocol integration tests.

---

### Task 1: Rich per-window state transitions

**Files:**
- Modify: `crates/z3rm/src/main.rs:279-570`
- Test: `crates/z3rm/src/main.rs` test module

- [ ] **Step 1: Write failing state tests**

Add tests that construct two `MuxWindow` maps and assert: disconnect/retry/failure on window A retains its attempt count and full error while window B remains connected; a completion with the wrong domain or binding epoch is rejected; explicit retry while reconnecting does not create a second attempt.

- [ ] **Step 2: Verify red**

Run `cargo test -p z3rm --bin z3rm connection_state -- --nocapture` and confirm compilation/test failure because rich attempts, errors, and epoch validation do not exist.

- [ ] **Step 3: Implement the minimal state model**

Replace the copy-only phase with `Connected`, `Reconnecting { attempt: u32 }`, and `Offline { attempts: u32, last_error: SharedString }`. Add binding epoch and state transition helpers. Keep every transition keyed by `WindowId`, domain identity, and epoch.

- [ ] **Step 4: Verify green**

Run the focused test command and confirm all connection-state tests pass.

- [ ] **Step 5: Commit**

Commit as `Model per-window mux recovery state`.

### Task 2: Window-owned local and remote recovery worker

**Files:**
- Modify: `crates/z3rm/src/main.rs:461-617,819-899,2860-2971`
- Modify only if required for test injection: `crates/z3rm/src/daemon.rs`
- Test: `crates/z3rm/src/main.rs` test module

- [ ] **Step 1: Write failing worker tests**

Use a small test recovery backend/driver around the pure transition scheduler to prove periodic failure starts attempt one, bounded failures increment attempts, a retry signal interrupts the wait, successful recovery resets state, and dropping/rebinding one window closes only its monitor.

- [ ] **Step 2: Verify red**

Run `cargo test -p z3rm --bin z3rm mux_connection_monitor -- --nocapture` and confirm failure because no per-window monitor driver exists.

- [ ] **Step 3: Implement recovery scheduling**

Give each binding a coalescing retry channel and stored `Task<()>`. Start/restart it after registration/rebind. On local recovery call `ensure_daemon_running` then `reconnect_local_in_place`; on SSH recovery call `SshSession::reconnect` then `reconnect_at_path_in_place`. Use bounded exponential backoff, reject stale completions, and return on window/binding removal.

- [ ] **Step 4: Route native retry through the worker**

Replace the standalone reconnect action body with a signal to the affected window monitor. Do not spawn a parallel retry path.

- [ ] **Step 5: Verify green**

Run the focused tests and `cargo test -p z3rm --bin z3rm reconnect -- --nocapture`.

- [ ] **Step 6: Commit**

Commit as `Recover mux connections per window`.

### Task 3: Persistent actionable status UI

**Files:**
- Modify: `crates/z3rm/src/main.rs:332-384`
- Test: `crates/z3rm/src/main.rs` test module

- [ ] **Step 1: Write failing projection tests**

Test a pure status projection for exact connected, reconnecting-attempt, and offline-attempt labels; ensure offline projection retains the complete error string and exposes retry/copy actions.

- [ ] **Step 2: Verify red**

Run `cargo test -p z3rm --bin z3rm connection_status -- --nocapture` and confirm the projection is missing.

- [ ] **Step 3: Render the status and controls**

Always render Connected, Reconnecting, or Offline. Add a retry control that signals this `WindowId`; for offline state expose the full error in a tooltip/detail and a Copy Error control that writes it to GPUI's clipboard.

- [ ] **Step 4: Verify green**

Run the projection tests and `cargo check -p z3rm --bin z3rm`.

- [ ] **Step 5: Commit**

Commit as `Show actionable mux connection status`.

### Task 4: End-to-end recovery and cleanup

**Files:**
- Modify: `crates/mux/tests/e2e.rs`
- Modify if a deterministic fault hook is needed: `crates/mux_server/src/connection.rs`

- [ ] **Step 1: Add failing integration tests**

Create two independent client domains/windows. Break one transport and assert only it enters recovery. Restart the server, trigger/await recovery, and assert the returned full snapshot restores pane membership, layout, and focus. Close a recovering owner and assert no later retry or server membership remains.

- [ ] **Step 2: Verify red**

Build `z3rm-server`, set `Z3RM_SERVER_BIN`, run only the new mux e2e tests, and confirm the missing behavior fails.

- [ ] **Step 3: Complete only integration gaps found by the tests**

Keep snapshot restoration in `MuxDomain` and the existing notification consumer. Do not add client-side replay or duplicated layout authority.

- [ ] **Step 4: Verify green and regression gates**

Run:

- `cargo test -p z3rm --bin z3rm -- --nocapture`
- `cargo build -p mux_server --bin z3rm-server`
- `Z3RM_SERVER_BIN=$CARGO_TARGET_DIR/debug/z3rm-server cargo test -p mux --test e2e -- --nocapture`
- `cargo test --workspace`

Expected: all pass without `z3rm-migration`.

- [ ] **Step 5: Commit**

Commit as `Verify per-window mux recovery`.
