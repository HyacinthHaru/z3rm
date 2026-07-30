# Mux Protocol

Defined in `crates/mux_protocol/proto/mux.proto` (package `z3rm.mux`), compiled
with `prost-build` from `crates/mux_protocol/build.rs`.

**This is not gRPC.** The wire format is a length-prefixed protobuf `Envelope`
carried over a Unix domain socket (or a Windows named pipe / SSH-forwarded
socket). There is no service definition, no HTTP/2, and no streaming RPC — push
notifications travel as their own envelope variant instead.

## Framing

```
| varint length | protobuf-encoded Envelope |
```

Helpers live in `crates/mux_protocol/src/mux_protocol.rs`: `frame()`,
`unframe()`, `parse_len_prefix()`, `check_frame_len()`.

Hardening limits, enforced *before* any allocation so a hostile length prefix
cannot exhaust memory:

| Constant | Value | Purpose |
|---|---|---|
| `MAX_VARINT_LEN` | 10 | Reject overlong length prefixes. |
| `MAX_FRAME_PAYLOAD` | 64 MiB | Cap a single frame. |
| `MAX_GRID_CELLS` | 1 048 576 | Cap cells in one grid payload; scrollback fetches page against this. |

## Envelope

```protobuf
message Envelope {
    ProtocolVersion version = 1;
    oneof payload {
        Request request = 2;
        Response response = 3;
        Notification notification = 4;
    }
}
```

`PROTOCOL_VERSION` is `{major: 1, minor: 2}`. A major mismatch is rejected at
handshake (`crates/mux_server/src/connection.rs`); a minor mismatch is accepted
so older clients keep working against newer servers.

## Request / Response

`Request` carries a `request_id` plus a `oneof body` with 33 variants; the
matching `Response` echoes `request_id` and carries either a non-empty `error`
string or a typed body. Correlation is by `request_id` alone — responses may
arrive out of order.

Session and window lifecycle
: `CreateSession`, `ListSessions`, `KillSession`, `RenameSession`, `Attach`,
  `Detach`, `NewWindow`, `Shutdown`

Panes and layout
: `SpawnPane`, `SplitPane`, `ClosePane`, `FocusPane`, `ResizePane`,
  `SetPaneTitle`, `ZoomPane`, `ResizeLayout`

Terminal I/O
: `SendInput`, `Paste`, `SubscribePaneOutput`, `ShellIntegration`

Grid and scrollback
: `FetchGridUpdate`, `FetchScrollback`, `SearchScrollback`

Files and clipboard
: `ReadFile`, `ListDir`, `StatFile`, `SetClipboard`, `GetClipboard`

Shadow snapshot (§4)
: `ListFileVersions`, `GetFileVersion`, `DeclineFileVersion`

Extensions
: `InstallExtension`

## Notifications

Server-to-client pushes, `oneof event` with 16 variants:

`PaneDirty`, `PaneAdded`, `PaneRemoved`, `PaneFocused`, `PaneTitleChanged`,
`PaneZoomed`, `PaneBell`, `PaneOutputChunk`, `TabTitleChanged`,
`SessionLayoutChanged`, `ClipboardChanged`, `ExtensionChromeUpdate`,
`SyncScrollbackNotification`, `WindowAdded`, `WindowRemoved`,
`ShellIntegrationChanged`.

Delivery semantics differ by kind, and the two kinds are routed through
different channels (§3.4):

- **`PaneDirty` is at-most-once.** It is a wake-up signal only; dropping one is
  harmless because the next one — or the client's own repaint — pulls the same
  state. Fan-out uses a lossy `try_send`.
- **Lifecycle events are at-least-once.** `PaneAdded`, `PaneRemoved`,
  `SessionLayoutChanged`, `PaneZoomed`, `PaneTitleChanged` and `PaneBell` go
  through per-connection unbounded channels with a blocking send. Losing a
  `PaneRemoved` would strand a zombie pane on the client.

## Grid sync

Push the signal, pull the data (§3.3):

1. PTY output advances the pane's monotonic `generation` counter.
2. The server pushes `PaneDirty(pane_id)` — no payload.
3. The client calls `FetchGridUpdate(pane_id, since_generation)`.
4. The server answers from a 64-entry ring of row-level diffs.

`FetchGridUpdateResponse` is either a `GridDiff` (row-level changes, aligned
with alacritty's own damage tracking) or a `FullGridSnapshot`. The server falls
back to a full snapshot when:

- `since == 0`, or `since > current` (the client is ahead — it reconnected),
- the requested generation has already rolled out of the ring,
- **any entry in the range is flagged `requires_full_snapshot`.**

That last case is the one to remember when adding code: cursor moves, terminal
mode changes, scroll-offset changes and resizes cannot be expressed as row
diffs, so they must be published with `push_requiring_full_snapshot` rather than
`push`. Publishing them as a plain diff silently loses non-cell render state on
the client.

## Scrollback

`FetchScrollback(pane_id, from_line, direction, count)` returns rows plus
`total_lines` and a `scrollback_version`. History indices run oldest-first, from
`0` to `total_lines - 1`. `direction = 0` walks up and returns
`[from - count + 1, from]`; `direction = 1` walks down and returns
`[from, from + count)`. Callers must page against `MAX_GRID_CELLS / cols` rather
than asking for the whole history at once.

`scrollback_version` changes whenever authoritative history contents or layout
change, and is what clients use to invalidate a cached history snapshot.

## Client identity and roles

`AttachRequest` carries an optional `ClientIdentity` and an `AttachMode`
(`Shared`, `Steal`, `ReadOnly`). The server derives an effective `ClientRole`
from both and stores it on the connection; every request is then checked against
it (`check_permission` in `crates/mux_server/src/connection.rs`). Attaching
read-only downgrades the whole connection, not just the attach call.

## Where the code lives

| Concern | File |
|---|---|
| Schema | `crates/mux_protocol/proto/mux.proto` |
| Framing, limits, key/target parsing | `crates/mux_protocol/src/mux_protocol.rs` |
| Input routing state machine (§16.5) | `crates/mux_protocol/src/input.rs` |
| Client transport, request correlation | `crates/mux/src/mux.rs` |
| Server dispatch, permissions, fan-out | `crates/mux_server/src/connection.rs` |
| Grid diff ring | `crates/mux_server/src/grid_sync.rs` |
