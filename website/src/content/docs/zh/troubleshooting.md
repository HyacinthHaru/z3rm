---
title: 故障排查
description: 诊断 daemon、socket、重连、远程、渲染和 shell integration 问题。
translationKey: troubleshooting
section: support
order: 1
status: verified
---

# 故障排查

## CLI 无法连接

确认 `z3rm-server` 与客户端一起安装；开发环境可设置 `Z3RM_SERVER_BIN`。只有确认没有 daemon 持有 socket 后才能删除它。

## 重连后会话缺失

对相同本机或 SSH endpoint 运行 `z3rm ls`。重连使用完整 snapshot；endpoint 或版本不同比漏掉 dirty 通知更可能。

## Command capture 为空

`list-commands` 需要 Shell 发出 OSC 133。普通 `capture-pane` 与 `search-scrollback` 不依赖命令标记。

## 远程路径被拒绝

文件 RPC 以 session 工作目录为根；`..` 被拒绝，绝对路径必须位于根内。

## 渲染空白

报告平台、GPU、renderer、日志以及 native fallback 是否工作。不要丢弃 transport error。
