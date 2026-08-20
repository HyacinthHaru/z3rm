---
title: Session、window 与 pane
description: Z3rm 工作区的目标层级与生命周期。
translationKey: concept-sessions
section: concepts
order: 1
status: verified
---

# Session、window 与 pane

**Session** 是服务器持有的持久工作区；**window** 是 session 中的 tab；**pane** 持有一个 PTY 与终端模拟器。

## 目标

使用 `session:window.pane`。脚本操作可变化布局前应先列出对象。

## 生命周期

新建 session 会生成首个 pane。关闭最后一个 pane 会移除 window；关闭最后一个 window 会结束 session。`kill-server` 是明确的 daemon 关闭路径。
