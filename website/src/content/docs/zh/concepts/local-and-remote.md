---
title: 本机与远程会话
description: 本机 socket 与 SSH 转发使用同一个 mux protocol。
translationKey: concept-remote
section: concepts
order: 3
status: verified
---

# 本机与远程会话

本机和远程客户端使用相同 framed binary mux protocol。本机使用 Unix socket 或 named pipe；远程使用 SSH forwarded channel。

## 远程 attach

```sh
z3rm attach --ssh ssh://user@example.com/path/to/project
```

远程主机持有 PTY、grid、文件与 shadow snapshot。客户端负责显示和输入事件；重连总是从服务器完整快照恢复。
