---
title: 使用 CLI
description: 通过脚本创建、定位、观察和恢复 Z3rm 会话。
translationKey: guide-cli
section: guide
order: 2
status: verified
---

# 使用 CLI

CLI 直接与 `mux_server` 通信，不抓取 GUI。目标采用类似 tmux 的 session、window、pane 层级。

## 持久工作流

```sh
z3rm new -s build -c /path/to/project
z3rm split-window -t build:0.0 -h
z3rm send-keys -t build:0.0 -l 'cargo test'
z3rm send-keys -t build:0.0 Enter
z3rm capture-pane -t build:0.0 -p --last-command
z3rm detach
z3rm attach -t build
```

## 显式选择目标

存在多个 pane 时使用 `session:window.pane`。先用格式字符串发现标识：

```sh
z3rm list-panes -t build -F '#{session_name}:#{window_index}.#{pane_index}	#{pane_title}'
```

## 把错误当成控制流

`has-session` 与 `search-scrollback` 在没有对象或匹配时返回非零。文件命令拒绝 `..` 与 session 根目录之外的路径。
