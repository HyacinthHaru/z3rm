---
title: CLI reference
description: Current mux, history, file, clipboard, and recovery commands.
translationKey: reference-cli
section: reference
order: 1
status: verified
---

# CLI reference

## Sessions

- `ls [-F FORMAT]`
- `new -s NAME [-c CWD]`
- `attach [-t TARGET]`
- `attach --ssh ssh://URI`
- `detach`
- `has-session -t TARGET`
- `rename-session [-t TARGET] NAME`
- `kill -t TARGET`
- `kill-server`
- `recover [--list | -t SESSION]`

## Windows and panes

- `new-window [-t SESSION]`
- `list-windows [-t SESSION] [-F FORMAT]`
- `split-window [-t TARGET] [-h|-v] [-c COMMAND]`
- `list-panes [-t SESSION] [-F FORMAT]`
- `select-pane -t TARGET`
- `resize-pane [-t TARGET] [-x W] [-y H] [-Z]`
- `rename-window -t TARGET TITLE`
- `kill-pane -t TARGET`

## Input and output

- `send-keys -t TARGET [-l|-H] [-N COUNT] KEYS...`
- `paste-buffer [-t TARGET]`
- `capture-pane [-t TARGET] [-p] [-S LINE] [-E LINE] [-J] [-e]`
- `capture-pane --command N` or `--last-command`
- `list-commands [-t TARGET] [-n MAX]`
- `search-scrollback [-t TARGET] [-n MAX] [-S LINE] [-f] REGEX`

## Files, history, and clipboard

- `list-changes`, `list-versions`, `show-version`, `restore`
- `list-dir`, `stat-file`, `show-file`
- `show-buffer [-I]`, `set-buffer [--type text|png|path] DATA`

## Format strings

`#{name}` substitutes, `#{?name,yes,no}` branches, and `##` emits `#`. Supported fields include session, window, pane, size, path, command, focus, attachment, and dead-pane state.
