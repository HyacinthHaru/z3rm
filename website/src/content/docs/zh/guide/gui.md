---
title: 使用 GUI
description: Attach 会话、组织 pane、检查输出并审阅文件。
translationKey: guide-gui
section: guide
order: 3
status: verified
---

# 使用 GUI

## 返回已有会话

运行 `z3rm attach -t NAME`。窗口从服务器完整快照重建 session、tab、pane、焦点、布局和终端 generation。

## 操作 pane

通过命令面板或原生键位 split、focus、resize、zoom、新建 tab 与关闭 pane。QuickJS chrome 故障时，核心控制仍可用。

## 滚动、搜索和复制

Scrollback 来自服务器终端。选择和复制发生在客户端；输入、粘贴和鼠标报告通过 mux protocol 进入权威 PTY。

## 检查文件和 diff

打开 session worktree 的文件树。文件以只读模式打开；diff review 与 shadow version 用于检查和恢复。

## 重连

断线会显示错误。重连获取完整 session snapshot，而不是猜测错过的通知。
