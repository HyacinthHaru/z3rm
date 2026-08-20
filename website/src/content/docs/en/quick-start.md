---
title: Quick start
description: Build Z3rm and create a persistent terminal session.
translationKey: quick-start
section: guide
order: 1
status: verified
---

# Quick start

## Build

```sh
git clone https://github.com/cyjin-yl/z3rm.git
cd z3rm
cargo build -p z3rm -p mux_server
```

The development binaries are `target/debug/z3rm` and `target/debug/z3rm-server`.

## Create and inspect a session

```sh
z3rm new -s work -c "$PWD"
z3rm ls
z3rm list-windows -t work
z3rm list-panes -t work
```

## Open the GUI

```sh
z3rm attach -t work
```

Detach or close the window, then attach again. The daemon remains the owner of the shell and scrollback.

## Drive the same pane from the CLI

```sh
z3rm send-keys -t work:0.0 -l 'printf "hello from z3rm\n"'
z3rm send-keys -t work:0.0 Enter
z3rm capture-pane -t work:0.0 -p -S -20 -E -
```
