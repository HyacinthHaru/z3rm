# Shadow Snapshot Review Workflows Design

**Status:** Approved for implementation

**Issues:** [#11](https://github.com/cyjin-yl/z3rm/issues/11), [#12](https://github.com/cyjin-yl/z3rm/issues/12)

## Goal

Turn the existing single-baseline Diff Review into a server-backed Shadow Snapshot review workspace. It presents every version in monotonic `SeqNo` order, compares any version with current content or another version, restores an explicitly confirmed version without overwriting newer work, and reviews every Changed Files entry through a process-local queue. Reviewing never deletes history.

## Constraints

- Shadow Snapshot ordering uses `SeqNo`, never wall clock or mtime.
- The mux server owns session paths and Shadow Snapshot state; local and SSH clients use the same RPC path.
- Restore remains WAL-first and runs on the existing single writer thread.
- Optimistic concurrency is enforced by the server immediately before restore; a client-side refresh alone is insufficient.
- Accept means “reviewed in this open review workspace”. It does not mutate Shadow Snapshot state and does not survive application restart.
- Deleted, empty, binary, oversized, unavailable, and garbage-collected versions are explicit states.
- Existing CLI-provided `DiffReview` tabs remain supported without a mux review session.

## Existing Foundation

- `ListChangedFiles` returns path, version count, and latest `SeqNo` newest-first.
- `ListFileVersions`, `GetFileVersion`, and `DeclineFileVersion` expose the version tree.
- `SnapshotWatch` serializes all engine commands on one recorder thread.
- `DiffReview` renders a read-only line diff and emits accepted/declined events.
- `OpenDiff` presents a one-file selector but reads current content from the client filesystem and automatically chooses the penultimate version.

## Chosen Architecture

### 1. Review state is a server snapshot

Add a bounded `GetFileReviewState` RPC scoped by `session_id` and path. Its handler runs through `SnapshotWatch`’s single-writer command channel and returns:

- versions ordered by ascending `SeqNo`, each with `version_id`, `seq_no`, and trigger;
- the latest `SeqNo`;
- current file existence and size;
- SHA-256 of current bytes when the file exists;
- a content classification: text, empty, binary, too large, or deleted;
- current UTF-8 content only when it is text/empty and at most 2 MiB.

The server reads at most 2 MiB into the response but streams the entire file through SHA-256 when a fingerprint is required. This permits safe restore of binary or oversized current files without pretending they are text-comparable.

This dedicated RPC is preferred over combining `ListFileVersions`, `StatFile`, and paged `ReadFile`: those independent calls cannot define one review baseline, and direct local file reads are incorrect for SSH sessions.

### 2. Historical content has typed availability

`GetFileVersionResponse` gains metadata:

- `total_bytes`,
- `is_binary`, and
- `content_available`.

The recorder reconstructs the target version once. Text/empty content up to 2 MiB is returned; binary or oversized content returns metadata with no text payload. A missing/GC-evicted version remains a typed RPC error. This keeps version-to-version comparison bounded while still allowing a retained binary/large version to be selected as a restore target.

### 3. Restore uses an atomic compare-and-restore guard

`DeclineFileVersionRequest` adds required review preconditions:

- expected latest `SeqNo`,
- expected current-file existence, and
- expected SHA-256 when the file exists.

On the recorder thread, before writing a WAL intent, restore validates:

1. the path’s latest retained `SeqNo` still equals the expected value;
2. current existence still matches; and
3. current bytes still hash to the expected digest.

Any mismatch returns a stale-review error and performs no WAL append, file write, tree mutation, or watcher suppression. If all checks pass, the existing crash-safe decline protocol runs unchanged. The response returns the new restore version ID and `SeqNo`, allowing the client to refresh deterministically.

The content fingerprint closes the watcher-latency race: even when a filesystem event has not reached the recorder yet, changed bytes prevent the restore.

### 4. Creation is a first-class trigger

Add `SnapshotTrigger::Create` and preserve it through storage, WAL, protocol labels, and watcher routing. New files can then be distinguished from modifications without consulting Git. Existing histories whose first event was recorded as `Write` remain “Modified”; the client does not retroactively guess that they were additions.

Queue classification is:

- **Added**: first retained trigger is Create and current file exists;
- **Deleted**: current state is Deleted;
- **Modified**: every other text/empty existing path;
- **Binary** or **Too large**: current content is not text-comparable;
- **Unavailable**: permission, reconstruction, or RPC failure.

### 5. Review workspace composition

`OpenDiff` is retained for selecting one Changed Files entry. A new `change_review::OpenChangedFilesReview` action opens all entries. When `OpenDiff` is invoked with an active `FileViewer`, it opens that path directly; otherwise it opens the existing selector.

A mux-backed review item contains:

- a left queue rail when opened in review-all mode;
- a version timeline for the current file;
- a comparison header naming both endpoints;
- the existing read-only line-diff body for text comparisons;
- restore confirmation and stale/error banners.

The existing standalone `DiffReview::new` path remains for CLI IPC and does not show queue/timeline controls.

### 6. Version timeline and comparison

The timeline is always rendered in ascending `SeqNo` and labels each row with:

- stable version ID,
- `SeqNo`,
- human-readable trigger (Created, Written, Closed, Debounced, Restored, Deleted), and
- selected endpoint role.

Comparison endpoints are explicit:

- **From**: a historical version;
- **To**: Current or another historical version.

Selecting a timeline row normally sets From and compares it with Current. A separate “Compare to” control assigns To, which enables history-to-history comparison. Adjacent previous/next controls move the active endpoint without changing sort order. Only Current can become stale; historical endpoints are immutable unless GC removes them, in which case the endpoint becomes Unavailable and the timeline refreshes.

### 7. Restore confirmation and refresh

Restore is enabled only when From identifies a retained historical version. Activating it opens an in-item confirmation that names:

- the path,
- target version ID and `SeqNo`,
- current classification, and
- that current bytes will be replaced while history remains.

Confirm sends the compare-and-restore preconditions captured by the latest review state. On success:

- mark the queue item reviewed,
- refresh its review state and timeline so the new Restore node is visible, and
- in review-all mode advance to the next unreviewed item.

On stale or any failure, stay on the current item, mark it Needs refresh, show the reason, and do not change review progress.

### 8. Process-local continuous queue

The queue snapshots Changed Files in server order (newest `SeqNo` first). Each entry stores path, baseline latest `SeqNo`, classification, and one of:

- Pending,
- Reviewed,
- NeedsRefresh,
- Loading, or
- Unavailable.

Navigation supports previous, next, and direct row selection. Accept first refreshes `GetFileReviewState`; it marks Reviewed only if latest `SeqNo`, existence, and content digest still match the displayed baseline. Restore success marks Reviewed as described above. Both then advance to the next Pending item.

Every accept, restore, explicit refresh, and queue navigation refreshes `ListChangedFiles`:

- new paths append to the queue and become Pending;
- a changed Pending path becomes NeedsRefresh;
- a changed Reviewed path becomes NeedsRefresh and no longer counts toward completed progress;
- paths are never removed merely because the user reviewed them.

Progress is `freshly reviewed / total current queue entries`. When no entries exist, render “No changed files”. When all entries are freshly reviewed, render a completion state while retaining the queue and timelines.

### 9. Keyboard and actions

Use a unique `change_review` action namespace:

- `OpenChangedFilesReview`
- `PreviousFile`
- `NextFile`
- `PreviousVersion`
- `NextVersion`
- `AcceptCurrent`
- `RestoreSelectedVersion`
- `RefreshCurrent`
- `CloseReview`

The review root adds a `ChangedFilesReview` key context. Default scoped bindings support the complete review loop; buttons expose the same actions. Closing removes only the workspace item and process-local queue state.

## Error Handling

- Session/path authorization continues through mux session root resolution.
- All ordinary review failures return `ResponseBody::Error`; they do not tear down the socket.
- Invalid SHA-256 length or missing required preconditions is rejected before engine mutation.
- UTF-8 failure is classified as Binary, never decoded lossily for a text diff.
- A version evicted between timeline load and selection becomes Unavailable and triggers refresh.
- Accept and restore serialize per review item with an in-flight flag to prevent double submission.
- Queue navigation never marks an item reviewed.

## Verification

### Shadow engine/server tests

- Create trigger survives WAL/storage round-trip and appears in version order.
- conditional restore succeeds when `SeqNo`, existence, and digest match;
- changed bytes with unchanged watcher state reject restore before WAL/tree mutation;
- changed `SeqNo`, create/delete transitions, and malformed digest reject restore;
- successful restore creates a newer Restore node and preserves older nodes;
- review-state classification covers text, empty, deleted, binary, too-large, and unavailable paths.

### Protocol/mux tests

- round-trip new review messages and fields;
- server errors preserve the connection;
- SSH/local clients use identical domain methods;
- bounded historical content metadata does not exceed frame limits.

### Review model tests

- strict ascending `SeqNo` timeline;
- From/To selection supports current-vs-history and history-vs-history;
- adjacent version navigation clamps at endpoints;
- trigger labels and Added/Modified/Deleted classification;
- stale refresh transitions Pending/Reviewed to NeedsRefresh;
- new files append without clearing reviewed history;
- progress excludes stale reviewed entries;
- accept and restore advance only after successful freshness validation.

### GPUI tests

- OpenDiff targets an active FileViewer path;
- review-all opens queue, timeline, and current diff;
- direct selection and previous/next update current file;
- confirmation identifies path and target version;
- restore failure retains current selection and displays error;
- empty/all-complete states render clearly;
- every review action is available under the review key context.

### End-to-end test

With a real mux server and temporary worktree, create, modify, delete, and restore multiple files while a review is open. Verify remote-RPC content, arbitrary version comparison, stale rejection after an intervening same-size write, automatic queue advancement, refreshed Restore nodes, and unchanged historical version counts after Accept.

## Out of Scope

- Persisting reviewed state across process restart.
- Deleting or compacting history from the review UI.
- Editing files in the diff viewer.
- Bulk accept that skips per-file freshness checks.
- Using Git status to infer Shadow Snapshot semantics.
