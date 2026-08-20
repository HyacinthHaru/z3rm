---
title: Sessions, windows, and panes
description: The target hierarchy and lifecycle of a Z3rm workspace.
translationKey: concept-sessions
section: concepts
order: 1
status: verified
---

# Sessions, windows, and panes

A **session** is the durable server-owned workspace. A **window** is a tab in that session. A **pane** owns one PTY and terminal emulator.

## Targets

Use `session:window.pane`. Names and numeric indices are accepted where the command permits them. List objects before scripting against a layout that can change.

## Lifecycle

Creating a session spawns its first pane. Closing the last pane removes its window; closing the last window ends the session. `kill-server` is the explicit daemon shutdown path.
