---
title: Configuration
description: Server lifetime, scrollback, themes, terminal, and extension settings.
translationKey: reference-config
section: reference
order: 3
status: verified
---

# Configuration

## Server settings

Create `${XDG_CONFIG_HOME:-$HOME/.config}/z3rm/server.json`:

```json
{ "keep_alive_seconds": 0, "scrollback_lines": 10000 }
```

`0` keeps the daemon alive indefinitely. Scrollback is capped at 100,000 rows. Override the file with `Z3RM_SERVER_SETTINGS` or use `Z3RM_SCROLLBACK_LINES` and `Z3RM_KEEP_ALIVE_SECONDS`. Running servers reload these values.

## Client settings

Use the settings UI for font, terminal, theme, keymap, remote connection, and extension preferences. Configuration structures and checked-in defaults change together; unknown inherited Zed options are not Z3rm guarantees.
