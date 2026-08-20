---
title: Use the GUI
description: Attach to sessions, arrange panes, inspect output, and review files.
translationKey: guide-gui
section: guide
order: 3
status: verified
---

# Use the GUI

## Attach instead of starting over

Run `z3rm attach -t NAME`. The window reconciles from the server snapshot: sessions, tabs, panes, focus, layouts, and terminal generations.

## Work with panes

Use the command palette or native keybindings to split, focus, resize, zoom, create tabs, and close panes. These controls remain available if extension chrome stops.

## Scroll, search, and copy

Scrollback comes from the server-owned terminal. Selection and copy are client interactions; input, paste, and mouse reporting travel to the authoritative PTY through the mux protocol.

## Inspect files and diffs

Open the file tree for the session worktree. Files open read-only. Diff review and shadow versions let you inspect and restore prior content without turning Z3rm into a general editor.

## Reconnect

A disconnected window reports the failure. Reconnect requests a complete session snapshot rather than replaying guessed notifications.
