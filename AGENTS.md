# Rust coding guidelines

* Prioritize code correctness and clarity. Speed and efficiency are secondary priorities unless otherwise specified.
* Do not write organizational or comments that summarize the code. Comments should only be written in order to explain "why" the code is written in some way in the case there is a reason that is tricky / non-obvious.
* Prefer implementing functionality in existing files unless it is a new logical component. Avoid creating many small files.
* Avoid using functions that panic like `unwrap()`, instead use mechanisms like `?` to propagate errors.
* Be careful with operations like indexing which may panic if the indexes are out of bounds.
* Never silently discard errors with `let _ =` on fallible operations. Always handle errors appropriately:
  - Propagate errors with `?` when the calling function should handle them
  - Use `.log_err()` or similar when you need to ignore errors but want visibility
  - Use explicit error handling with `match` or `if let Err(...)` when you need custom logic
  - Example: avoid `let _ = client.request(...).await?;` - use `client.request(...).await?;` instead
* When implementing async operations that may fail, ensure errors propagate to the UI layer so users get meaningful feedback.
* Never create files with `mod.rs` paths - prefer `src/some_module.rs` instead of `src/some_module/mod.rs`.
* When creating new crates, prefer specifying the library root path in `Cargo.toml` using `[lib] path = "...rs"` instead of the default `lib.rs`, to maintain consistent and descriptive naming (e.g., `gpui.rs` or `main.rs`).
* Avoid creative additions unless explicitly requested
* Use full words for variable names (no abbreviations like "q" for "queue")
* Use variable shadowing to scope clones in async contexts for clarity, minimizing the lifetime of borrowed references.
  Example:
  ```rust
  executor.spawn({
      let task_ran = task_ran.clone();
      async move {
          *task_ran.borrow_mut() = true;
      }
  });
  ```

# Timers in tests

* In GPUI tests, prefer GPUI executor timers over `smol::Timer::after(...)` when you need timeouts, delays, or to drive `run_until_parked()`:
  - Use `cx.background_executor().timer(duration).await` (or `cx.background_executor.timer(duration).await` in `TestAppContext`) so the work is scheduled on GPUI's dispatcher.
  - Avoid `smol::Timer::after(...)` for test timeouts when you rely on `run_until_parked()`, because it may not be tracked by GPUI's scheduler and can lead to "nothing left to run" when pumping.

# GPUI

GPUI is a UI framework which also provides primitives for state and concurrency management.

## Context

Context types allow interaction with global state, windows, entities, and system services. They are typically passed to functions as the argument named `cx`. When a function takes callbacks they come after the `cx` parameter.

* `App` is the root context type, providing access to global state and read and update of entities.
* `Context<T>` is provided when updating an `Entity<T>`. This context dereferences into `App`, so functions which take `&App` can also take `&Context<T>`.
* `AsyncApp` and `AsyncWindowContext` are provided by `cx.spawn` and `cx.spawn_in`. These can be held across await points.

## `Window`

`Window` provides access to the state of an application window. It is passed to functions as an argument named `window` and comes before `cx` when present. It is used for managing focus, dispatching actions, directly drawing, getting user input state, etc.

## Entities

An `Entity<T>` is a handle to state of type `T`. With `thing: Entity<T>`:

* `thing.entity_id()` returns `EntityId`
* `thing.downgrade()` returns `WeakEntity<T>`
* `thing.read(cx: &App)` returns `&T`.
* `thing.read_with(cx, |thing: &T, cx: &App| ...)` returns the closure's return value.
* `thing.update(cx, |thing: &mut T, cx: &mut Context<T>| ...)` allows the closure to mutate the state, and provides a `Context<T>` for interacting with the entity. It returns the closure's return value.
* `thing.update_in(cx, |thing: &mut T, window: &mut Window, cx: &mut Context<T>| ...)` takes a `AsyncWindowContext` or `VisualTestContext`. It's the same as `update` while also providing the `Window`.

Within the closures, the inner `cx` provided to the closure must be used instead of the outer `cx` to avoid issues with multiple borrows.

Trying to update an entity while it's already being updated must be avoided as this will cause a panic.

`WeakEntity<T>` is a weak handle. It has `read_with`, `update`, and `update_in` methods that work the same, but always return an `anyhow::Result` so that they can fail if the entity no longer exists. This can be useful to avoid memory leaks - if entities have mutually recursive handles to each other they will never be dropped.

## Concurrency

All use of entities and UI rendering occurs on a single foreground thread.

`cx.spawn(async move |cx| ...)` runs an async closure on the foreground thread. Within the closure, `cx` is `&mut AsyncApp`.

When the outer cx is a `Context<T>`, the use of `spawn` instead looks like `cx.spawn(async move |this, cx| ...)`, where `this: WeakEntity<T>` and `cx: &mut AsyncApp`.

To do work on other threads, `cx.background_spawn(async move { ... })` is used. Often this background task is awaited on by a foreground task which uses the results to update state.

Both `cx.spawn` and `cx.background_spawn` return a `Task<R>`, which is a future that can be awaited upon. If this task is dropped, then its work is cancelled. To prevent this one of the following must be done:

* Awaiting the task in some other async context.
* Detaching the task via `task.detach()` or `task.detach_and_log_err(cx)`, allowing it to run indefinitely.
* Storing the task in a field, if the work should be halted when the struct is dropped.

A task which doesn't do anything but provide a value can be created with `Task::ready(value)`.

## Elements

The `Render` trait is used to render some state into an element tree that is laid out using flexbox layout. An `Entity<T>` where `T` implements `Render` is sometimes called a "view".

Example:

```
struct TextWithBorder(SharedString);

impl Render for TextWithBorder {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div().border_1().child(self.0.clone())
    }
}
```

Since `impl IntoElement for SharedString` exists, it can be used as an argument to `child`. `SharedString` is used to avoid copying strings, and is either an `&'static str` or `Arc<str>`.

UI components that are constructed just to be turned into elements can instead implement the `RenderOnce` trait, which is similar to `Render`, but its `render` method takes ownership of `self` and receives `&mut App` instead of `&mut Context<Self>`. Types that implement this trait can use `#[derive(IntoElement)]` to use them directly as children.

The style methods on elements are similar to those used by Tailwind CSS.

If some attributes or children of an element tree are conditional, `.when(condition, |this| ...)` can be used to run the closure only when `condition` is true. Similarly, `.when_some(option, |this, value| ...)` runs the closure when the `Option` has a value.

## Input events

Input event handlers can be registered on an element via methods like `.on_click(|event, window, cx: &mut App| ...)`.

Often event handlers will want to update the entity that's in the current `Context<T>`. The `cx.listener` method provides this - its use looks like `.on_click(cx.listener(|this: &mut T, event, window, cx: &mut Context<T>| ...)`.

## Actions

Actions are dispatched via user keyboard interaction or in code via `window.dispatch_action(SomeAction.boxed_clone(), cx)` or `focus_handle.dispatch_action(&SomeAction, window, cx)`.

Actions with no data defined with the `actions!(some_namespace, [SomeAction, AnotherAction])` macro call. Otherwise the `Action` derive macro is used. Doc comments on actions are displayed to the user.

Action handlers can be registered on an element via the event handler `.on_action(|action, window, cx| ...)`. Like other event handlers, this is often used with `cx.listener`.

* **Action namespace uniqueness.** Each `actions!(namespace, [...])` registration creates globally unique action types. Registering the same `namespace::ActionName` in multiple crates causes a runtime panic. Before defining a new action, search the codebase for existing registrations with the same namespace. When creating actions in a new crate, use a distinct namespace to avoid conflicts (e.g., `mux_pane` instead of reusing `pane`).

## Notify

When a view's state has changed in a way that may affect its rendering, it should call `cx.notify()`. This will cause the view to be rerendered. It will also cause any observe callbacks registered for the entity with `cx.observe` to be called.

## Entity events

While updating an entity (`cx: Context<T>`), it can emit an event using `cx.emit(event)`. Entities register which events they can emit by declaring `impl EventEmitter<EventType> for EntityType {}`.

Other entities can then register a callback to handle these events by doing `cx.subscribe(other_entity, |this, other_entity, event, cx| ...)`. This will return a `Subscription` which deregisters the callback when dropped.  Typically `cx.subscribe` happens when creating a new entity and the subscriptions are stored in a `_subscriptions: Vec<Subscription>` field.

# Mux architecture guidelines

z3rm uses a server-canonical multiplexer model. The GUI client renders state; `mux_server` owns all authority.

* **Server-canonical terminal state.** `mux_server` owns PTY fds, runs the alacritty emulator, parses DEC escape sequences, and holds scrollback. The GUI client never parses PTY bytes and never holds layout authority.（来源：spec §3.1, §15.1）
* **One data path.** Local and remote sessions use the same framed binary protocol over a socket. There is no shared-memory fast path and no dual parsing.（来源：spec §3.1）
* **`MuxDomain` is a concrete struct.** Do not introduce a `Domain` trait for a single implementation. Extract a trait only if a second real implementation appears.（来源：spec §3.2）
* **Generation counter.** Each pane has a monotonic generation counter that increments on every render-affecting change: PTY output, cursor style, alternate screen switch, scroll offset, and title update.（来源：spec §3.3, §15.13）
* **Row-level grid diff.** Grid diffs are row-level and aligned with alacritty's internal `dirty_lines` damage tracking. Do not invent a separate damage model.（来源：spec §3.3, §16.3）
* **Push signal, pull data.** `mux_server` sends a lightweight `PaneDirty(PaneId)` push notification; the client schedules a repaint and then calls `fetch_grid_update` to pull the actual diff or full snapshot.（来源：spec §3.3, §3.4）
* **Notification delivery semantics.** `PaneDirty` is at-most-once (missing one is harmless). Lifecycle events such as `PaneAdded`, `PaneRemoved`, and `SessionLayoutChanged` are at-least-once; losing a `PaneRemoved` creates a zombie pane.（来源：spec §3.4）
* **Authoritative reconnect.** On attach or reconnect, the server returns a full session snapshot (panes, tabs, layout tree, focus, generations). The client reconciles from this snapshot rather than relying on missed push notifications.（来源：spec §15.4, §15.12）
* **Process keepalive.** By default `keep_alive = true`. The daemon stays alive until explicitly killed, matching tmux expectations.（来源：spec §3.5, §16.1）

# Migration tracking

z3rm is migrating from a Zed fork. Migration holes are marked explicitly and fixed by removing the marker.

* **Mark holes with `#[z3rm_todo]`.** Use this attribute on functions, modules, or items that reference deleted crates, broken references, stubs, or disabled features.（来源：spec §8.1）
* **Fixing a hole means deleting the attribute.** There is no separate cleanup step. When the underlying issue is resolved, remove `#[z3rm_todo]`.
* **Categories.** Use exactly one of:
  - `removed-crate` — code depends on a crate that has been deleted.
  - `broken-ref` — a type/function reference is broken because its source was pruned.
  - `stub` — a placeholder implementation that needs real logic.
  - `disabled-feature` — functionality temporarily disabled during migration.
* **Feature flag behavior.** Without the `z3rm-migration` feature, `#[z3rm_todo]` expands to `compile_error!`. With `--features z3rm-migration` it registers the hole and compilation succeeds.（来源：spec §8.1）
* **配置跟随结构。** 修改 `SettingsContent` 或任何配置结构体后，必须重写对应的默认配置文件（如 `default.json`）以匹配新结构，而不是逐个修补缺失字段。
* **Two-pass discipline.** Pass 1 scans and marks holes; Pass 2 fixes them. Verify milestones with `cargo check --features z3rm-migration`.（来源：spec §8.2）
* **`.rs.old` files must never be committed.** They are local temporary artifacts. Git history is the official backup; delete `.rs.old` files before any commit.（来源：spec §8.2）

# Extension system

z3rm's UI chrome is implemented as QuickJS extensions. Native GPUI chrome is the Day 0 baseline, not a fallback.

* **QuickJS runtime on a dedicated OS thread.** The extension host must not run on the GPUI render thread. Extensions communicate with the UI via async channels; a hung extension freezes only itself.（来源：spec §5.2）
* **Extensions declare their runtime side.** In `extension.toml`, set `[runtime] side = "server" | "client" | "both"`. Server-side extensions run on the remote host and access remote PTY/grid/filesystem; client-side extensions render chrome via GPUI.（来源：spec §16.8）
* **Core commands work without the extension host.** Split pane, switch pane, create/close tab, attach/detach, settings, and kill server/session must all be reachable through native keybindings even if QuickJS is not running, crashed, or fuel-limited.（来源：spec §15.7）
* **Chrome renders through JSON, not direct GPUI calls.** Extensions return a Virtual DOM or display-list JSON; the native GPUI bridge maps it to elements. High-frequency widgets (clocks, meters) use the display-list pattern.（来源：spec §5.4）
* **Capabilities and resource limits.** Enforce declared capabilities and resource limits (`memory_limit_mb`, `cpu_budget_ms`, `io_rate_limit`) at runtime. Violations suspend the extension.（来源：spec §5.3, §5.6）

# Shadow snapshot constraints

Shadow snapshot provides crash-safe, fine-grained filesystem versioning independent of git.

* **Single-writer thread for WAL.** WAL appends and MemTable inserts happen on exactly one watcher processing thread. There is no concurrent insertion.（来源：spec §4.3, §4.5）
* **SeqNo is monotonic.** Assign `SeqNo` atomically before WAL append. Never use wall clock or file mtime for ordering; NTP rollback must not break ordering.（来源：spec §4.4, §4.5）
* **WAL append before file write.** For decline/undo operations, append the WAL entry and fsync before writing the restored file to disk. Crash recovery replays the WAL to finish incomplete operations.（来源：spec §4.8）
* **Bounded delta chain with Rope replay.** Delta chains are capped at `D_max = 16`. The 17th version forces a full snapshot. Replay deltas using Rope operations, not string concatenation.（来源：spec §4.6）
* **Age-based eviction.** Evict old nodes by `SeqNo` (FIFO), not LRU. Promote delta children to full snapshots when needed during GC.（来源：spec §4.9）

# Build guidelines

* During active migration, use `cargo check --features z3rm-migration`. This lets `#[z3rm_todo]` holes compile so you can verify intermediate states.
* For final verification, use `cargo check` with no migration feature. Unfixed holes become `compile_error!`, so a clean check means migration is complete.
* Do not use `./script/clippy` until the migration scripts are updated for z3rm.

# Pull request hygiene

When an agent opens or updates a pull request, it must:

* Use a clear, correctly capitalized, imperative PR title (for example, `Fix crash in session manager` or `Add scrollback search to terminal pane`).
* Avoid conventional commit prefixes in PR titles (`fix:`, `feat:`, `docs:`, etc.).
* Avoid trailing punctuation in PR titles.
* Optionally prefix the title with a crate name when one crate is the clear scope (for example, `mux: Add remote transport handshake`).
* Include a `Release Notes:` section as the final section in the PR body.
* Use one bullet under `Release Notes:`:
  - `- Added ...`, `- Fixed ...`, or `- Improved ...` for user-facing changes, or
  - `- N/A` for docs-only and other non-user-facing changes.
* Format release notes exactly with a blank line after the heading, for example:

```
Release Notes:

- N/A
```

# GPUI rendering debug guide

排查 GPUI 渲染问题时的关键要点和方法。

## 窗口创建必须遵循 Zed 模式

* **必须用 `MultiWorkspace` 做窗口根视图。** `Workspace` 不能作为 `open_window` 的根视图——它的 render 依赖 `MultiWorkspace` 提供的上下文（`Workspace::for_window` 通过 `window.root::<MultiWorkspace>()` 查找）。直接用 `Workspace` 做根视图会导致 render 被调用但内部子组件（PaneGroup、Pane、items）不渲染。
* **Items 必须在 `cx.new(|cx| Workspace::new(...))` 闭包内添加。** 不能在 workspace 创建后通过 `workspace.update(cx, ...)` 添加——此时 workspace 的渲染树已经构建完毕，后添加的 items 不会触发正确的渲染链。正确模式（参考 `Workspace::new_local`）：
  ```rust
  let window = cx.open_window(options, move |window, cx| {
      let workspace = cx.new(|cx| {
          let mut workspace = Workspace::new(id, project, app_state, window, cx);
          // 在构造期间添加 items（init 回调模式）
          workspace.add_item(pane, item, None, true, true, window, cx);
          workspace
      });
      cx.new(|cx| MultiWorkspace::new(workspace, window, cx))
  });
  ```
* **`open_window` 回调返回 `Entity<MultiWorkspace>`，不是 `Entity<Workspace>`。**

## 渲染链排查方法

渲染链：`open_window` → `MultiWorkspace::render` → `.child(workspace)` → `Workspace::render` → `render_center` → `PaneGroup::render` → `Member::Pane` → `AnyView::from(pane)` → `Pane::render` → `item.to_any_view()` → `MuxPaneView::render`

* **逐层加 `eprintln!` 定位断点。** 在每一层的 render 方法入口加 `eprintln!`，确认哪一层没有被调用。
* **`BottomDockLayout` 默认值必须是 `Full`。** `Stacked` 分支的 Workspace render 只渲染 bottom dock，完全遗漏 `render_center`（中心 pane group），导致整个终端区域不渲染。迁移 stub 类型时，必须检查每个枚举变体在 render 中的分支是否完整。（来源：实际排查 2026-07）
* **`gpui::Empty` 陷阱。** 搜索所有返回 `gpui::Empty` 的 render 方法——它们会在特定条件下静默吞掉整个子树。关键位置：
  - `pane.rs:render_tab_bar` — `workspace.upgrade().is_none()` 时返回 Empty（tabbar 消失）
  - `pane.rs:Render` — `project.upgrade().is_none()` 时返回空 div（整个 pane 消失）
* **`set_should_display_welcome_page(false)` 必须在添加 items 前调用。** `Workspace::new` 默认设为 true；welcome page 在 pane 无 items 时渲染，会遮挡终端。

## GPUI 类型陷阱

* **`Pixels(pub(crate) f32)`** — `.0` 字段是 crate-private。用 `f32::from(pixels)` 做算术。
* **`gpui::rgb(hex)` 返回 `Rgba`，不是 `Hsla`。** `text_color()` 和 `bg()` 接受 `impl Into<Hsla>`。需要 `rgb(hex).into()` 转换。
* **`ThemeColors` 字段名不直观。** 选择背景色是 `element_selection_background`（不是 `selection_background`）；没有 `cursor` 字段。
* **`cx.listener()` 闭包必须是 `'static`。** 不能捕获局部变量引用。在闭包内部定义常量：`let cw = px(8.4);`
* **`log::info!` vs `eprintln!`。** `log` crate 的输出可能被 tracing bridge 吞掉。排查时用 `eprintln!` 确保输出到 stderr。

## Mux 协议陷阱

* **`connect_local` 签名是 `Option<&Path>`。** 所有调用方必须用 `Some(path.as_path())` 或 `None`。
* **`fetch_grid_update(since=0)` 必须返回 `FullSnapshot`。** 初始 generation 为 0 时，`since == current == 0` 不能返回 `NoChange`，否则客户端永远收不到初始网格。
* **`let _ = domain.send_input(...)` 会静默丢弃按键。** 传输失败时用户无感知。至少用 `.log_err()` 或通知 UI 层。

* **§3.1 in-place render-path 例外。** 客户端可以订阅 `PaneOutputChunk` PTY 字节流作为补充通知通道，驱动 `fetch_grid_update` 拉取渲染状态。字节流不改变 grid-diff 渲染路径，仅作为额外的 dirty 信号。spec §3.1 例外条款授权此优化（来源：spec §3.1, 行204）。