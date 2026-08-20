---
title: 快速开始
description: 构建 Z3rm 并创建一个持久终端会话。
translationKey: quick-start
section: guide
order: 1
status: verified
---

# 快速开始

## 构建

```sh
git clone https://github.com/cyjin-yl/z3rm.git
cd z3rm
cargo build -p z3rm -p mux_server
```

开发二进制是 `target/debug/z3rm` 与 `target/debug/z3rm-server`。

## 创建并查看会话

```sh
z3rm new -s work -c "$PWD"
z3rm ls
z3rm list-windows -t work
z3rm list-panes -t work
```

## 打开 GUI

```sh
z3rm attach -t work
```

Detach 或关闭窗口，再次 attach。Shell 与 scrollback 仍由 daemon 持有。

## 用 CLI 控制同一 pane

```sh
z3rm send-keys -t work:0.0 -l 'printf "hello from z3rm\n"'
z3rm send-keys -t work:0.0 Enter
z3rm capture-pane -t work:0.0 -p -S -20 -E -
```
