---
title: CLI Reference
description: "Reference for z3rm's command-line interface, including tmux-compatible mux control and editor open-path usage."
---

# CLI Reference

z3rm exposes one public `z3rm` entry point with two roles:

1. **Mux control** (tmux-compatible) — `ls`, `new`, `attach`, `send-keys`, `capture-pane`, …
2. **Editor open-path** — open files/directories in the GUI

## Packaged vs development binary

| Build | Binary | Behavior |
|---|---|---|
| Development | `cargo run -p z3rm -- <cmd>` | Full mux parser in `crates/z3rm` |
| Packaged | `bin/z3rm` (`crates/cli` wrapper) | Forwards known mux subcommands to `libexec/z3rm` (or platform equivalent) with identical argv and exit status; remaining args use the editor open-path IPC path |

Do **not** document `cargo run -p cli -- ls` as the primary mux entry: that only works because the wrapper forwards to the real binary.

## Mux commands (§3.10)

```sh
z3rm ls
z3rm new -s <name> [-c <cwd>]
z3rm kill-session -t <target>
z3rm kill-server
z3rm attach [-t <target>] [--ssh ssh://user@host[:port]]
z3rm detach
z3rm split-window [-t <pane>] [-h|-v] [-c <command>]
z3rm send-keys [-t <pane>] <keys...>
z3rm capture-pane [-t <pane>] [-p] [-S <lines>] [-e]
z3rm list-panes [-t <session>]
z3rm select-pane [-t <pane>]
z3rm kill-pane [-t <pane>]
z3rm resize-pane [-t <pane>] [-x <cols>] [-y <rows>]
z3rm new-window [-t <session>]
z3rm rename-window [-t <pane>] <title>
z3rm help
```

### Targets

- Omitted pane target → `$Z3RM_PANE` / `$Z3RM_PANE_ID`, else focused pane in `$Z3RM_SESSION`
- `session` → focused pane of that session
- `session:W.P` → tab/pane indexes
- `%N` → global flattened pane index

### Environment in panes

Spawned shells receive:

- `Z3RM_PANE` / `Z3RM_PANE_ID` — current pane id
- `Z3RM_SESSION` — owning session id
- `TERM=xterm-256color`, `COLORTERM=truecolor`

### capture-pane

- `-p` print to stdout
- `-S -N` include the newest N scrollback lines before the visible grid
- `-e` preserve cell styles as SGR (classic 16-color when near palette, else truecolor)

### send-keys

Shared `mux_protocol::parse_key` names: `Enter`, `Tab`, `Escape`, `BSpace`, arrows, `C-c`, `M-x`, `F1`–`F12`, and literal UTF-8.

### Exit classes

| Code | Meaning |
|---|---|
| 0 | Success / help |
| 2 | Parse error |
| 1 | Connection / RPC / runtime error |

## Editor open-path options

When the first argument is **not** a mux subcommand, the wrapper/editor path accepts:

```sh
z3rm [OPTIONS] [PATHS]...
```

Common options (wrapper):

- `-w`, `--wait` — wait for files to close
- `-n`, `--new` — new workspace window
- `-a`, `--add` — add to focused workspace
- `-r`, `--reuse` — reuse existing window

Open a file at a line/column:

```sh
z3rm myfile.txt:42
z3rm myfile.txt:42:10
```

## Debugging accessibility

With AccessKit enabled (default; disable with `Z3RM_A11Y=0`):

- Action `z3rm_debug::DumpAccessibilityTree` writes the last frame AccessKit tree to `$Z3RM_A11Y_DUMP_PATH` or `/tmp/z3rm-a11y-tree.json`.

## Related

- Architecture: `docs/architecture/overview.md`, `docs/architecture/mux-design.md`
- Spec: `docs/superpowers/specs/2026-07-14-z3rm-foundation-design.md` §3.10
- Plan: `docs/superpowers/plans/2026-07-14-plan-27-cli-control-interface.md`
