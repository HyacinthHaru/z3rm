---
title: 功能
description: Z3rm 当前可用的能力，以及每种工作流的指南入口。
translationKey: features
section: features
order: 1
status: verified
---

# 功能

Z3rm 把 GPU 终端、持久多路复用器和审阅界面放在同一个工作区。这里的每项能力都对应当前实现。

## 会话不随窗口结束

daemon 持有 PTY 和终端状态。关闭 GUI 不会终止命名会话；使用 `z3rm attach -t NAME` 返回。

## 一个会话，两种控制方式

人类在 GUI 中组织 pane；脚本和 Agent 通过 `list-panes`、`send-keys`、`capture-pane` 和格式字符串控制同一份服务器状态。

## 可搜索的命令历史

OSC 133 标记让 `list-commands` 识别命令边界；`search-scrollback` 搜索历史与可见行。

## 只读审阅

文件树、只读查看器、diff review 和 shadow versions 让 Agent 的修改可检查、可恢复。

## 本机与远程同一协议

本机 socket 和 SSH 转发都承载相同的 framed protocol。GUI 不持有 PTY，也不维护第二份权威终端状态。
