---
title: CLI 参考
description: 当前 mux、history、文件、clipboard 与恢复命令。
translationKey: reference-cli
section: reference
order: 1
status: verified
---

# CLI 参考

## Session

`ls`、`new`、`attach`、`attach --ssh`、`detach`、`has-session`、`rename-session`、`kill`、`kill-server`、`recover`。

## Window 与 pane

`new-window`、`list-windows`、`split-window`、`list-panes`、`select-pane`、`resize-pane`、`rename-window`、`kill-pane`。

## 输入与输出

`send-keys`、`paste-buffer`、`capture-pane`、`list-commands`、`search-scrollback`。`capture-pane --command N` 与 `--last-command` 按 OSC 133 命令边界获取输出。

## 文件、历史与 clipboard

`list-changes`、`list-versions`、`show-version`、`restore`、`list-dir`、`stat-file`、`show-file`、`show-buffer`、`set-buffer`。

## 格式字符串

`#{name}` 替换字段，`#{?name,yes,no}` 条件分支，`##` 输出 `#`。字段覆盖 session、window、pane、尺寸、路径、命令、焦点、attach 与 dead 状态。
