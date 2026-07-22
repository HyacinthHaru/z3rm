# Mux Protocol (gRPC)

Defined in `z3rm_mux_proto/proto/mux.proto`. All RPCs are unary unless marked `stream`.

## Service: MuxService

```protobuf
service MuxService {
  // Session management
  rpc CreateSession(CreateSessionRequest) returns (Session);
  rpc AttachSession(AttachSessionRequest) returns (AttachSessionResponse);
  rpc DetachSession(DetachSessionRequest) returns (google.protobuf.Empty);
  rpc ListSessions(google.protobuf.Empty) returns (stream Session);
  rpc GetSession(GetSessionRequest) returns (Session);
  rpc DestroySession(DestroySessionRequest) returns (google.protobuf.Empty);
  rpc RenameSession(RenameSessionRequest) returns (Session);

  // Grid management
  rpc ResizeGrid(ResizeGridRequest) returns (Grid);
  rpc SplitPane(SplitPaneRequest) returns (Pane);
  rpc ClosePane(ClosePaneRequest) returns (google.protobuf.Empty);
  rpc FocusPane(FocusPaneRequest) returns (Pane);
  rpc MovePane(MovePaneRequest) returns (Pane);
  rpc SwapPanes(SwapPanesRequest) returns (Pane);
  rpc SetPaneTitle(SetPaneTitleRequest) returns (Pane);
  rpc SubscribeGrid(SubscribeGridRequest) returns (stream GridUpdate);

  // PTY I/O
  rpc WritePty(WritePtyRequest) returns (google.protobuf.Empty);
  rpc ReadPty(ReadPtyRequest) returns (stream PtyOutput);
  rpc ResizePty(ResizePtyRequest) returns (google.protobuf.Empty);

  // Workspace
  rpc GetWorkspace(GetWorkspaceRequest) returns (Workspace);
  rpc SetWorkspace(SetWorkspaceRequest) returns (Workspace);

  // Shadow snapshots
  rpc CreateSnapshot(CreateSnapshotRequest) returns (Snapshot);
  rpc RestoreSnapshot(RestoreSnapshotRequest) returns (Snapshot);
  rpc BranchSnapshot(BranchSnapshotRequest) returns (Snapshot);
  rpc ListSnapshots(ListSnapshotsRequest) returns (stream Snapshot);
  rpc DeleteSnapshot(DeleteSnapshotRequest) returns (google.protobuf.Empty);
  rpc SubscribeSnapshots(SubscribeSnapshotsRequest) returns (stream SnapshotEvent);
}
```

## Key Messages

### Session
```protobuf
message Session {
  string id = 1;              // UUID v7
  string name = 2;
  string workspace_id = 3;
  SessionState state = 4;     // CREATED, ATTACHED, DETACHED, DESTROYED
  google.protobuf.Timestamp created_at = 5;
  google.protobuf.Timestamp updated_at = 6;
}
```

### Grid / Pane
```protobuf
message Grid {
  string session_id = 1;
  string root_pane_id = 2;
  map<string, Pane> panes = 3;  // pane_id -> Pane
  uint32 cols = 4;
  uint32 rows = 5;
}

message Pane {
  string id = 1;
  string session_id = 2;
  PaneType type = 3;          // TERMINAL, EDITOR, PLUGIN
  PaneState state = 4;        // ACTIVE, INACTIVE, MINIMIZED
  uint32 cols = 5;
  uint32 rows = 6;
  string title = 7;
  string cwd = 8;
  string shell = 9;
  string pty_id = 10;         // if TERMINAL
  repeated string children = 11;  // if SPLIT (horizontal/vertical)
  SplitDirection direction = 12;  // HORIZONTAL, VERTICAL
}
```

### GridUpdate (stream)
```protobuf
message GridUpdate {
  oneof update {
    PaneCreated pane_created = 1;
    PaneDestroyed pane_destroyed = 2;
    PaneResized pane_resized = 3;
    PaneFocused pane_focused = 4;
    PaneMoved pane_moved = 5;
    PaneSwapped pane_swapped = 6;
    PaneTitleChanged pane_title_changed = 7;
    GridResized grid_resized = 8;
  }
  uint64 sequence = 100;      // monotonically increasing per session
}
```

### PTY I/O
```protobuf
message WritePtyRequest {
  string session_id = 1;
  string pane_id = 2;
  bytes data = 3;             // raw bytes to stdin
}

message PtyOutput {
  string session_id = 1;
  string pane_id = 2;
  bytes data = 3;             // raw bytes from stdout
  uint64 sequence = 4;        // monotonically increasing per pane
}

message ResizePtyRequest {
  string session_id = 1;
  string pane_id = 2;
  uint32 cols = 3;
  uint32 rows = 4;
  uint32 pixel_width = 5;
  uint32 pixel_height = 6;
}
```

### Shadow Snapshots
```protobuf
message Snapshot {
  string id = 1;              // UUID v7
  string session_id = 2;
  string branch_id = 3;
  uint64 seq_no = 4;          // global monotonic sequence
  uint64 parent_seq_no = 5;   // 0 for root
  SnapshotType type = 6;      // MANUAL, AUTO, BRANCH
  google.protobuf.Timestamp created_at = 7;
  bytes payload_hash = 8;     // SHA-256 of serialized payload
  string description = 9;
}

message CreateSnapshotRequest {
  string session_id = 1;
  string branch_id = 2;       // empty = current branch
  string description = 3;
  SnapshotType type = 4;
}

message RestoreSnapshotRequest {
  string snapshot_id = 1;
  bool create_branch = 2;     // true = branch from snapshot, false = reset current
}

message BranchSnapshotRequest {
  string snapshot_id = 1;
  string new_branch_id = 2;   // empty = auto-generate
}
```

## Sequence Numbers

All streaming responses (`SubscribeGrid`, `ReadPty`, `SubscribeSnapshots`) include a monotonically increasing `sequence` per stream. Client uses this for:
- Detecting missed messages (gap detection)
- Ordering guarantees
- Resume tokens (send last seen sequence on reconnect)

## Error Codes

Standard gRPC codes with `mux_error` detail:
- `NOT_FOUND`: session/pane/snapshot not found
- `FAILED_PRECONDITION`: invalid state transition (e.g., split on destroyed pane)
- `RESOURCE_EXHAUSTED`: max sessions/panes/snapshots reached
- `UNAVAILABLE`: server shutting down, try another
- `INTERNAL`: bug, check server logs