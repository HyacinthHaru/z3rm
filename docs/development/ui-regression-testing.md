# UI regression testing

`crates/z3rm/tests/ui_screenshot_regression.rs` renders real frames through
GPUI's headless path and asserts on both the framebuffer and the accessibility
tree. It covers the terminal pane and the extension chrome — the two surfaces a
`cargo check` cannot say anything about.

## Running

```sh
cargo test -p z3rm --test ui_screenshot_regression
```

On a machine without Xcode (Command Line Tools only) the `metal` shader
compiler is missing, so add:

```sh
cargo test -p z3rm --test ui_screenshot_regression \
    --features gpui_platform/runtime_shaders
```

The suite owns its harness (`harness = false`). macOS rejects AppKit and Metal
calls off the main thread, and libtest only keeps tests there when running one
at a time; owning the harness means callers do not have to remember
`--test-threads=1`. Pass a substring to run a subset:

```sh
cargo test -p z3rm --test ui_screenshot_regression -- extension_chrome
```

## Output

Every case writes what it rendered to `target/ui_screenshots/`:

- `<case>.png` — the captured framebuffer
- `<case>.a11y.json` — the accessibility tree for that frame

These are diagnostic artifacts, not baselines. Look at them when an assertion
fails; nothing compares against them.

## Why there are no baseline images

Pixel comparison across machines is dominated by font rasterization and GPU
driver differences, which produce failures that say nothing about the code. The
suite asserts on properties instead:

- the framebuffer is not blank, and carries the colors the view asked for
- the accessibility tree contains the expected roles (`Terminal` for a pane,
  a `TextRun` per visible line)
- a display-list repaint changes the pixels without disturbing the surrounding
  VDOM tree

The accessibility tree is the load-bearing half. It answers "did this element
actually get rendered, with the right semantics" far more precisely than pixels
do, and it is stable across machines.

## How the harness works

`HeadlessAppContext` (`crates/gpui/src/app/headless_app_context.rs`) pairs a
real GPU renderer with `TestDispatcher`, so frames are really rasterized while
scheduling stays deterministic. `Z3RM_A11Y_BUILD_HEADLESS=1` — set by the suite
before any window opens — makes `TestWindow::a11y_init` pump the in-memory
AccessKit builder so `window.debug_a11y_tree_json()` returns a tree.

Frames are drawn in a loop until the tree converges, because chrome that
depends on an extension event needs more than one frame to settle. The loop
gives up after 15 seconds and reports the roles it did see.

## Stress coverage

Throughput and lifecycle stress lives with the mux end-to-end tests
(`crates/mux/tests/`), which drive a real `z3rm-server` subprocess. They are the
right place for generation-counter monotonicity and scrollback eviction, none of
which needs a window.
