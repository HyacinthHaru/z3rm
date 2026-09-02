---
title: Server-canonical terminal state
description: Why Z3rm keeps PTYs, grids, history, and layouts in mux_server.
translationKey: concept-server
section: concepts
order: 2
status: verified
---

# Server-canonical terminal state

`mux_server` owns PTY file descriptors, the Alacritty emulator, scrollback, layout, focus, and generation counters. The GUI renders structured snapshots and row diffs.

## Push signal, pull data

A lightweight dirty notification schedules repaint. The client then fetches a grid update. Lifecycle events use stronger delivery semantics because losing pane removal would create stale UI.

## Reconnect from truth

On attach or reconnect, the client receives a complete authoritative snapshot. It does not reconstruct state from notifications it may have missed.
