# Terminal Command Blocks Design

**Status:** Approved for implementation

**Issue:** [#10](https://github.com/cyjin-yl/z3rm/issues/10)

## Goal

Expose the shell-command semantics already recorded from OSC 133 in every mux terminal pane: visible command boundaries, live/completed/success/failure state, safe previous/next navigation, and one-command output copying. Commands whose rows are no longer trustworthy remain visible as status-only records; the client never guesses a row.

## Constraints

- `mux_server` remains authoritative for PTY bytes, terminal grids, scrollback, marker positions, and command completion state.
- Command metadata and the structured grid must describe the same server checkpoint before the GUI applies them.
- A missing OSC 133 integration is ordinary terminal operation, not an error state.
- Alternate-screen applications keep the existing full-screen experience: command chrome and command shortcuts are inactive while alternate screen is active.
- No action reruns a command.
- No unbounded history or command-output allocation is introduced.

## Existing Foundation

- `Pane` records OSC 133 A/B/C/D markers, stable marker sequence IDs, addressing epochs, and exit codes.
- `ListCommands` returns grouped `CommandRange` values and omits invalidated line positions.
- `capture-pane --command` already defines the safe output-span fallback order (`C`, then `B`, then `A`) and rejects unaddressable ranges.
- `MuxPaneView` already reconstructs authoritative scrollback and active-screen state from generation-checked snapshots.

## Chosen Architecture

### 1. Command checkpoints are server-atomic

`Pane` gains a command-snapshot method that holds the pane commit lock while reading:

- grouped marker positions,
- terminal history size,
- grid generation,
- history version, and
- a monotonic command version incremented for every accepted OSC 133 marker.

`ListCommandsResponse` carries `generation`, `history_version`, and `command_version`. A marker arrival wakes clients with `PaneDirty`, including a marker-only PTY batch that did not change a visible cell.

This is preferred over polling and over independently reading marker positions and the grid. Polling delays completion state and wastes RPCs; independent reads permit a valid command range to be paired with the wrong scrollback checkpoint.

### 2. Grid and command metadata apply together

The mux-pane fetch pipeline obtains the structured grid/history and command snapshot, then validates that:

- command `generation` equals the prepared grid generation,
- command `history_version` equals the prepared history version, and
- a final grid checkpoint reports no newer generation.

A mismatch retries without mutating the terminal, selected command, or generation. `NoChange` grid replies still carry a refreshed command snapshot so an invisible D marker can change Running to Completed.

### 3. One shared command-span implementation

The pure command selection, output-span, and bounded capture helpers move from the z3rm CLI module into `mux`. Both CLI capture and `MuxPaneView` use the same rules:

- output starts at C, falling back to B and then A;
- D at column zero ends on the preceding line;
- missing D means Running and permits copy through the current visible bottom;
- a missing row on any required marker makes the span unaddressable;
- history pages must match the snapshot history version and dimensions;
- a final generation checkpoint must be stable before copied text is returned.

This avoids a second interpretation of OSC 133 ranges in the GUI.

### 4. Client command model

`MuxPaneView` keeps a bounded `CommandBlockState`:

- the latest command response,
- the selected stable command ID,
- the last command version applied,
- an optional asynchronous copy/navigation error, and
- a copy-in-flight flag.

A command derives one of these presentation states:

- **Running**: no D marker;
- **Succeeded**: D exists and exit code is zero;
- **Failed(code)**: D exists and exit code is non-zero;
- **Completed**: D exists without an exit code.

Location is orthogonal to status:

- **Located**: a safe start and optional end exist;
- **Expired**: markers exist but their rows were invalidated by reflow, clear, capacity rotation, or eviction;
- **Incomplete**: the shell omitted every usable start marker.

### 5. Terminal presentation

For normal-screen panes with at least one command:

- visible start/end rows receive thin horizontal boundary rules in the terminal overlay;
- the selected command receives a stronger boundary and a compact badge;
- a bottom command bar shows command ID, status, exit code when known, and location state;
- previous, next, previous-failure, and copy-output controls are exposed as buttons and GPUI actions.

Boundary coordinates are derived only from the applied command checkpoint and the local display offset. A boundary is rendered only when its tmux row is inside the current viewport.

When all commands are status-only, the bar remains useful but navigation/copy buttons are disabled with the reason “output no longer addressable”. When there are zero recorded markers, no command UI is rendered. When prompt-only markers exist, a muted “shell integration does not report command ranges” state may be shown only after the user invokes a command action; it is not persistent chrome.

### 6. Navigation and copy behavior

Previous/next operates on stable command IDs, not vector indexes retained across refreshes. If the selected command was evicted from the bounded marker list, selection moves to the nearest newer retained command, then the newest retained command as fallback.

Navigation computes the scrollback display offset from the server tmux line. It updates both the display-only terminal and `TerminalView`’s mux scrollback state so the next structured snapshot preserves the jump. A visible-screen command jumps to the live bottom and highlights its boundary.

Copy runs the shared bounded capture helper against the selected command ID. Success writes one clipboard item. Failure emits `MuxPaneEvent::InputFailed` and leaves the selected command unchanged. Copying never falls back to the whole pane.

## Action and Key Context

The existing unique `mux_pane` action namespace adds:

- `PreviousCommand`
- `NextCommand`
- `PreviousFailedCommand`
- `CopyCommandOutput`

Actions are registered on `MuxPaneView`. Default bindings are scoped to `Terminal` and chosen only from currently unbound terminal chords. The buttons remain available for users who override defaults.

## Error Handling

- Unknown pane and transport errors surface through the existing workspace error event.
- Checkpoint races retry a bounded number of times, then retain the last valid UI state and report the failure.
- Malformed dimensions, history counts, marker order, or row ranges are rejected before state mutation.
- Clipboard writes occur only after capture completes successfully.
- Read-only attachment does not disable navigation or copy because neither mutates the server.

## Verification

### Pure/unit tests

- status derivation for running, completed, success, and failure;
- start-marker fallback and column-zero D semantics shared by CLI and GUI;
- visible boundary projection at live bottom and non-zero display offsets;
- expired/incomplete ranges disable navigation and copy;
- selected-ID reconciliation after marker eviction;
- alternate-screen and no-OSC states render no persistent command chrome.

### Server/protocol tests

- command snapshot generation/history version are coherent under output and resize;
- marker-only D increments command version and emits a dirty notification;
- reflow/clear/capacity invalidation preserves exit status but removes row locations;
- protocol compatibility tests cover the new checkpoint fields.

### GPUI tests

- command actions are available in a mux pane;
- previous/next changes selection and authoritative display offset;
- failed-command navigation skips successful/running commands;
- copy writes exactly the selected command output;
- disabled actions expose an addressability reason.

### End-to-end test

A deterministic shell emits complete, partial, running, failed, and expired OSC 133 ranges. The real mux server and GUI-facing client must observe live status transitions, safe navigation, and exact copied output without affecting a plain shell or alternate-screen pane.

## Out of Scope

- Rerunning commands.
- Persisting GUI command selection across application restart.
- Inventing command boundaries from prompt text, regexes, or terminal colors.
- Parsing PTY bytes in the client.
