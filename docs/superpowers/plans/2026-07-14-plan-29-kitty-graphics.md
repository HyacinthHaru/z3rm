# Plan 29: Kitty Graphics Protocol / iTerm2 OSC 1337

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development or superpowers:executing-plans.

**Goal:** Implement Kitty graphics protocol (kitty-graphics) and iTerm2 OSC 1337 image display support. These are enhancements for displaying images in terminal panes.

**Dependencies:** `terminal` (alacritty-based), `mux_server`.

**Spec:** §11.2 Terminal Emulation (post-foundation)

> **未实现：Sixel。** 计划原文写的“Sixel is already supported by alacritty”不成立。
> 工作区固定的 alacritty fork（`zed-industries/alacritty` rev `4c129667`）在
> `alacritty_terminal/src/` 下对 `sixel` 零命中，`[features]` 只有 `serde`；
> 依赖的 `vte` 0.15 也没有把 DCS `q` 转交给 `Handler`（`hook`/`put`/`unhook`
> 全是 `debug!("[unhandled ...]")`）。要支持 Sixel 需要自己写解码器并把像素
> 写进网格，等同于 fork 模拟器，超出本计划范围，因此本次不做。
>
> **未实现：mux 场景。** 本次只接通本地 `terminal` 路径。`mux_protocol` 里没有
> pane 图像消息，服务端侧改动见实现汇报。

---

### Task 1: Kitty graphics protocol parser

**Files:**
- Create: `crates/terminal/src/kitty_graphics.rs`

- [x] **Step 1: Implement OSC 1337 parser (iTerm2)**

Parse `ESC ] 1337 ; File=name=...;inline=1 : base64_data ST`。同时支持
`MultipartFile=` / `FilePart=` / `FileEnd` 分片传输。

- [x] **Step 2: Implement kitty-graphics protocol parser**

Parse `ESC _ G f=100,... ; base64_data ESC \`（APC，不是 DCS）。支持 `m=1`
分块累积、`a=t/T/p/d/q`、`f=24/32/100`、`t=d/f/t/s`、`i`/`I`/`p`/`s`/`v`/`c`/`r`/
`x`/`y`/`w`/`h`/`z`/`C`/`q`/`d`/`o`/`U` 等键。

传输介质 `t=f`/`t=t`/`t=s` 与 `o=z` zlib 压缩返回 `ENOTSUPPORTED`（前者是从
子进程到宿主文件系统的旁路，后者需要新增解压依赖）。动画 `a=f/a/c` 同样
返回 `ENOTSUPPORTED`。

- [x] **Step 3: Image cache management**

按 pane 缓存已解码的 `RenderImage`，字节数与张数双限额 LRU 淘汰；淘汰的图像
交给 `App::drop_image` 释放 GPUI 纹理。

- [x] **Step 4: 把 PTY 字节流接到解析器上**

vte 会整段吞掉 APC，也不会把未知 OSC 号转给 `Handler`，所以两个协议都拿不到
handler 钩子。改为在 PTY 读取侧加一层 `GraphicsTapPty`（`crates/terminal/src/
alacritty.rs`）旁路扫描字节；`write_output` 与无 PTY 的子进程管道同样各挂一个
扫描器。字节流本身不改写。

---

### Task 2: GPUI image rendering

**Files:**
- Modify: `crates/terminal_view/src/terminal_element.rs`

- [x] **Step 1: Render images in terminal grid**

`Content.images` 里的可见放置在 `TerminalElement::paint` 里用
`Window::paint_image` 画出来，按 z-index 排序。

- [x] **Step 2: Image cell positioning**

放置锚在滚动缓冲区绝对行号上，`make_content` 把它投影成视口行号，滚动时图像
跟随内容移动、滚出视口后不再绘制。

> **已知限制：放置精度。** 锚点取“图形事件在 UI 线程被执行时光标在哪儿”。
> 扫描器在 PTY 读取线程上先于 alacritty 看到字节，但事件跨线程送达时同一批
> 字节里图形序列之后的文本可能已经被模拟器消化，所以行列是近似值。要做到
> 逐格精确需要实现 kitty 的 Unicode placeholder 方案，把图像锚在真实网格单元
> 上（后续工作）。

---

### Task 3: Tests + Commit

- [x] `cargo check -p terminal -p terminal_view` passes
- [x] `cargo test -p terminal -p terminal_view` passes（36 个图形相关用例）
- [ ] Commit + push
