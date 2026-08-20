---
title: Shadow snapshots
description: Crash-safe, fine-grained file history independent of Git.
translationKey: concept-snapshots
section: concepts
order: 4
status: verified
---

# Shadow snapshots

Shadow snapshots record file versions on the host where the worktree lives. They complement Git; they do not replace commits or branches.

## Ordering and durability

A monotonic sequence number orders versions. The single writer appends and syncs the WAL before a restore writes the filesystem. Delta chains are bounded and replay through ropes.

## Inspect and restore

```sh
z3rm list-changes -t work
z3rm list-versions -t work src/main.rs
z3rm show-version -t work src/main.rs VERSION
z3rm restore -t work src/main.rs VERSION
```
