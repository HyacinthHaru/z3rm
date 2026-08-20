---
title: Extension runtime
description: QuickJS runtime sides, capabilities, limits, and fallback controls.
translationKey: reference-extensions
section: reference
order: 4
status: mixed
---

# Extension runtime

Z3rm chrome can be provided by QuickJS extensions running off the GPUI render thread. `extension.toml` declares `runtime.side` as `server`, `client`, or `both`.

## Boundaries

Extensions return JSON virtual DOM or display-list data; they do not call GPUI directly. Declared capabilities gate host APIs. Memory, CPU, and I/O limits can suspend an extension.

## Failure behavior

Core terminal and mux commands remain reachable through native keybindings when the host is stopped or an extension fails. Use the extension control center to inspect lifecycle, permissions, and suspension state.
