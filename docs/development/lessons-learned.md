# z3rm Development Lessons & Implicit Conditions

> Discovered during migration from Zed fork. Each entry documents a non-obvious
> constraint that caused bugs or wasted debugging time.

## GPUI Rendering

### 1. `observe_new<Workspace>` vs direct injection
**Problem:** Items added via `observe_new::<Workspace>` callback don't render on
the first frame, even though `items().count()` confirms the item is present.
**Root cause:** The observer fires during entity creation inside `cx.new()`, but
the Pane's render cycle may not pick up items added at that point.
**Fix:** Add items directly in the `cx.open_window` callback, after
`Workspace::new` returns, via `workspace.update(cx, |w, cx| { ... })`.

### 2. Welcome page blocks terminal
**Problem:** `Workspace::new` calls `center_pane.set_should_display_welcome_page(true)`.
The welcome page renders instead of terminal content.
**Fix:** Call `pane.set_should_display_welcome_page(false)` before adding items.

### 3. `Pixels` field is private
**Problem:** `Pixels(pub(crate) f32)` — the `.0` field is crate-private.
**Fix:** Use `f32::from(pixels)` or `pixels / px(1.0)` for arithmetic.

### 4. ThemeColors field names
**Problem:** Field names don't match intuitive names.
- Selection: `element_selection_background` (NOT `selection_background`)
- No `cursor` field on ThemeColors — use `icon_accent` or `text`

### 5. `rgb()` returns `Rgba`, not `Hsla`
**Problem:** `gpui::rgb(hex)` returns `Rgba`, but `text_color()` and `bg()`
expect `Hsla` (or `impl Into<Hsla>`).
**Fix:** Use `rgb(hex).into()` to convert `Rgba → Hsla`.

### 6. `cx.listener()` closures can't capture locals by reference
**Problem:** Closures passed to `cx.listener()` must be `'static`. Local
variables like `char_width: Pixels` are borrowed, causing E0373.
**Fix:** Define constants inside the closure body: `let cw = px(8.4);`

## Mux Protocol

### 7. `connect_local` signature
**Problem:** Changed from `connect_local(&Path, Handle)` to
`connect_local(Option<&Path>)`. All callers (mux_server/main.rs, cli/main.rs,
ssh.rs, tests) must be updated.
**Fix:** `connect_local(Some(path.as_path()))` or `connect_local(None)`.

### 8. `fetch_grid_update` with `since=0`
**Problem:** Server returned `NoChange(0)` when client's generation was 0 and
server's generation was also 0 (initial state). Client never received grid.
**Fix:** `since == 0` always returns `FullSnapshot` (grid_sync.rs).

## Workspace / Pane

### 9. `add_item` parameter order
**Problem:** `Workspace::add_item(pane, item, dest_index, activate_pane,
focus_item, window, cx)` vs `Pane::add_item(item, activate_pane, focus_item,
dest_index, window, cx)` — parameter order differs.

### 10. Pane render requires valid Project
**Problem:** `Pane::render` calls `self.project.upgrade()` — if the weak
reference is dead, renders an empty div (no items, no welcome page, nothing).

## Build / Migration

### 11. `terminal_view` needs `tracing` dependency
**Problem:** `mux_pane.rs` uses `tracing::error!` but `tracing` wasn't in
`terminal_view/Cargo.toml`.
**Fix:** Add `tracing.workspace = true` to `[dependencies]`.

### 12. Dead code in `crates/z3rm/src/zed/`
**Problem:** ~350KB of upstream Zed files (open_listener, quick_action_bar,
edit_prediction_registry, etc.) never compiled because `zed.rs` declares no
submodules. Violates §8.2 artifact discipline.
**Fix:** Delete the entire `zed/` directory.