---
title: Use the CLI
description: Create, target, observe, and recover Z3rm sessions from scripts.
translationKey: guide-cli
section: guide
order: 2
status: verified
---

# Use the CLI

The CLI talks to `mux_server`; it does not scrape the GUI. Commands use tmux-like session, window, and pane targets.

## Persistent workflow

```sh
z3rm new -s build -c /path/to/project
z3rm split-window -t build:0.0 -h
z3rm send-keys -t build:0.0 -l 'cargo test'
z3rm send-keys -t build:0.0 Enter
z3rm capture-pane -t build:0.0 -p --last-command
z3rm detach
z3rm attach -t build
```

## Choose targets explicitly

Use `session:window.pane` when more than one pane exists. Discover identifiers with format strings:

```sh
z3rm list-panes -t build -F '#{session_name}:#{window_index}.#{pane_index}	#{pane_title}'
```

## Treat errors as control flow

`has-session` and `search-scrollback` return non-zero when no session or match exists. File commands reject `..` and paths outside the session root. Do not discard stderr or retry a transport failure without inspecting it.
