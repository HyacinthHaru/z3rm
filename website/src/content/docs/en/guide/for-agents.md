---
title: Z3rm for agents
description: A deterministic CLI contract for controlling terminal sessions without stealing human context.
translationKey: guide-agents
section: guide
order: 5
status: verified
---

# Z3rm for agents

Z3rm exposes a terminal session, not an LLM. An agent controls it through deterministic CLI commands and ordinary exit status.

## Discover before acting

```sh
z3rm ls -F '#{session_name}	#{session_attached}'
z3rm list-windows -t work -F '#{window_index}	#{window_name}	#{window_active}'
z3rm list-panes -t work -F '#{pane_id}	#{pane_current_path}	#{pane_dead}'
```

Always use an explicit target such as `work:0.1`; do not rely on another client's focus.

## Separate input from observation

Use `send-keys -l` for literal text, then send `Enter` separately. Use `capture-pane`, `list-commands`, or `search-scrollback` to observe output. Do not infer completion from elapsed time.

```sh
z3rm send-keys -t work:0.1 -l 'cargo test -p mux'
z3rm send-keys -t work:0.1 Enter
z3rm list-commands -t work:0.1 -n 1
z3rm capture-pane -t work:0.1 -p --last-command
```

## Handle failure

Check every exit status. A missing search match is non-zero; a dead pane is visible in formats; a transport error must reach the operator. Bound polling and stop when command markers report `exit=N`, `done`, or the pane dies.

## Coordinate with humans

Avoid `select-pane` unless visible focus is required. Prefer a dedicated pane, report the target you used, and leave file changes for human diff review.
