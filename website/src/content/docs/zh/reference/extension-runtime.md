---
title: 扩展运行时
description: QuickJS 运行侧、capability、资源限制与 fallback controls。
translationKey: reference-extensions
section: reference
order: 4
status: mixed
---

# 扩展运行时

Z3rm chrome 可由 GPUI 渲染线程之外的 QuickJS extension 提供。`extension.toml` 的 `runtime.side` 可为 `server`、`client` 或 `both`。

## 边界

扩展返回 JSON Virtual DOM 或 display list，不直接调用 GPUI。声明的 capability 控制 host API；memory、CPU 与 I/O 限制可以暂停扩展。

## 故障行为

Host 停止或扩展失败时，核心 terminal 与 mux 命令仍可通过原生键位使用。Extension control center 显示 lifecycle、permission 与 suspension。
