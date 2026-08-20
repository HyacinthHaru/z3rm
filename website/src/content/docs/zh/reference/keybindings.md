---
title: 键位参考
description: 核心 fallback controls 与当前 keymap 的检查方式。
translationKey: reference-keys
section: reference
order: 2
status: mixed
---

# 键位参考

Z3rm 在 `assets/keymaps/` 提供平台 keymap 与 overlay。当前 keymap 才是权威来源；本文不硬编码会漂移的平台修饰键名称。

## 核心 fallback action

即使 QuickJS chrome 不可用，session attach/detach、新建/关闭 tab、split pane、pane focus、settings、command palette 与 server/session 终止仍可通过原生 action 到达。

在 keymap editor 中检查或覆盖 action 绑定；冲突使用当前平台的可朗读修饰键名称。
