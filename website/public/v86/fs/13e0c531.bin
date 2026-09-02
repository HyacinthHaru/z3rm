---
title: Z3rm for humans
description: A durable workflow for shells, long-running jobs, and agent review.
translationKey: guide-humans
section: guide
order: 4
status: verified
---

# Z3rm for humans

## Name work, not windows

Create a session for a durable unit of work: `z3rm new -s release`. Tabs and panes organize activity inside it.

## Leave without killing the job

Detach or close the client. The daemon retains the PTY, emulator state, scrollback, and layout. Return with `z3rm attach -t release`.

## Share control carefully

A human GUI and an agent CLI can target the same canonical pane. Keep targets explicit and reserve separate panes for background automation when possible.

## Review changes

Use the file tree and diff review to inspect an agent's files. Shadow versions provide a host-local history for decline or restore operations.

## Recover deliberately

After a daemon crash, `z3rm recover --list` shows persisted layout metadata. Recovery requires explicit confirmation because terminal grid contents are not persisted.
