# 0005 - Chrome as Plugin

**Status:** Accepted

## Context

z3rm's UI chrome (tabs, status bar, command palette, sidebar, modal dialogs) must exist on Day 0 for the GPUI from Day 0 (native chrome). However, the long-term vision is that *all* chrome is implementable as QuickJS extensions — tabs, status bar, command palette, sidebar, modal dialogs — so users can replace or extend any UI surface. This requires the extension host to be functional before chrome-as-extension.

## Decision

Two-phase approach:
- **Phase 1 (Day 0–9):** Native GPUI chrome in `z3rm_chrome` crate. Core commands (new tab, close tab, split, resize, command palette, settings) are native GPUI commands registered in `z3rm_commands`. Extension host (`z3rm_extension_host`) starts in parallel but chrome remains native.
- **Phase 2 (Phase 10+):** Chrome surfaces exposed as extension APIs (`z3rm_chrome_api` crate). Native chrome reimplemented as built-in QuickJS extensions in `z3rm_chrome_extensions`. Native chrome becomes a built-in extension bundle, disabled by setting `chrome.mode = "extension"`.

Core commands in `z3rm_commands` **must work without extension host** (ADR-0005 consequence §15.7). Extension APIs are additive; core commands call directly into `z3rm_mux` / `z3rm_workspace` / `z3rm_terminal`.

## Consequences

- **Positive:** Day 0 usable without extension host. Extension host can iterate independently. Chrome-as-extension is opt-in, not forced. Core commands remain fast native paths.
- **Negative:** Dual maintenance of native + extension chrome during transition. Extension APIs must cover 100% of native chrome surface before switchover. Extension host startup latency added for chrome-as-extension mode.
- **Mitigation:** Feature flag `chrome_extension_mode` gates extension chrome. CI tests both modes. Native chrome removed only after extension parity verified.