---
title: 服务器权威终端状态
description: 为什么 PTY、grid、history 与布局都保存在 mux_server。
translationKey: concept-server
section: concepts
order: 2
status: verified
---

# 服务器权威终端状态

`mux_server` 持有 PTY、Alacritty 模拟器、scrollback、布局、焦点和 generation counter。GUI 只渲染结构化 snapshot 与行级 diff。

## Push 信号，pull 数据

轻量 dirty 通知触发重绘，客户端再获取 grid update。生命周期事件使用更强的交付语义，避免 zombie pane。

## 从事实重连

Attach 或重连时，客户端接收完整权威 snapshot，而不是依赖可能错过的通知。
