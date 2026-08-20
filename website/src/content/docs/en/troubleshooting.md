---
title: Troubleshooting
description: Diagnose daemon, socket, reconnect, remote, rendering, and shell integration failures.
translationKey: troubleshooting
section: support
order: 1
status: verified
---

# Troubleshooting

## The CLI cannot connect

Confirm `z3rm-server` is installed beside the client or set `Z3RM_SERVER_BIN` in development. Remove a socket only after confirming no daemon owns it.

## A session is missing after reconnect

Run `z3rm ls` against the same local or SSH endpoint. Reconnect uses a full snapshot; a different endpoint or server version is a more likely cause than a missed dirty notification.

## Command capture is empty

`list-commands` requires shell integration that emits OSC 133 markers. Plain `capture-pane` and `search-scrollback` do not require command markers.

## A remote path is rejected

File RPCs are rooted in the session working directory. `..` is rejected and absolute paths must remain inside that root.

## Rendering is blank

Report platform, GPU, renderer, logs, and whether native fallback controls work. Do not discard a transport error: it may explain the missing grid.
