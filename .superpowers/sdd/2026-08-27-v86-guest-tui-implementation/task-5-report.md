# Task 5 report — browser loading progress

## Scope

The browser boot surface now reports real progress for the wasm module, v86
resources, guest filesystem binaries, and mux connection:

- `website/wasm/z3rm_demo/index.html`
- `website/wasm/z3rm_demo/v86_bridge.js`
- `crates/z3rm_web/src/z3rm_web.rs`
- `website/playwright.config.ts`
- `website/tests/e2e/guest-terminal.spec.ts`

## Implementation

- Added the accessible `#loading-progress` status surface with a determinate
  bar, indeterminate animation, byte counters, rolling `B/s` rate, and reload
  retry control.
- Installed the tracker as a classic script before Trunk's module script.
  Fetch reads only `response.clone().body`; XHR listeners observe progress while
  preserving the original response, callbacks, and body handling.
- Classifies wasm, v86 guest resources, and `/fs/*.bin` system binaries while
  retaining the full URL in the stage label. Unknown content lengths remain
  indeterminate instead of fabricating a percentage.
- Reports guest emulator startup, mux-server startup/readiness, mux connection,
  and failures through `window.__z3rm_progress`.
- Hides the surface only after GPUI is ready and the first non-empty authoritative
  pane snapshot has been applied. The fallback serial terminal remains visible
  until that evidence exists.

## Verification

- `pnpm --dir website build`: passed; Trunk wasm, guest packaging, and Astro
  static build completed.
- `pnpm --dir website check`: passed with existing generated/v86 warnings.
- `pnpm --dir website test`: 22 passed.
- `guest-terminal.spec.ts`: real Chromium boot, scroll, guest download, browser
  clipboard action, and loading-error assertions passed in focused runs.

- Deployment run `33239330675` for `949f88b417` completed successfully:
  Verify and build passed all 23 hosted Chromium checks, and GitHub Pages deployed
  the site.
- Direct Chromium verification against
  `https://cyjin-yl.github.io/z3rm/gpui-demo/index.html` passed the complete
  hosted Chromium project (`23 passed`, 1.0m), including real guest boot,
  Kitty/media actions, download, clipboard, percentage progress, and protocol
  checks.
