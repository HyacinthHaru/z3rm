---
title: Features
description: What Z3rm can do today, linked to the guide for each workflow.
translationKey: features
section: features
order: 1
status: verified
---

# Features

Z3rm combines a GPU terminal, a persistent multiplexer, and a review surface. Every item here links to behavior implemented in the current build.

## Sessions that survive the window

The daemon owns PTYs and terminal state. Closing the GUI does not end a named session. Return with `z3rm attach -t NAME`.

## One session, two control surfaces

Humans arrange panes in the GUI. Scripts and agents use `list-panes`, `send-keys`, `capture-pane`, and format strings against the same server state.

## Searchable command history

OSC 133 shell markers let `list-commands` identify command boundaries. `search-scrollback` searches history and visible rows; `capture-pane --command` retrieves one command.

## Review without becoming an editor

The file tree, read-only viewer, diff review, and shadow versions make agent changes inspectable and reversible.

## Local and remote

Local sockets and SSH-forwarded connections carry the same framed protocol. The GUI does not own PTYs or parse a second authoritative terminal stream.
