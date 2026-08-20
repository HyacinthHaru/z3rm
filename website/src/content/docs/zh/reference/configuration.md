---
title: 配置参考
description: Daemon 生命周期、scrollback、主题、终端与扩展设置。
translationKey: reference-config
section: reference
order: 3
status: verified
---

# 配置参考

## Server 设置

创建 `${XDG_CONFIG_HOME:-$HOME/.config}/z3rm/server.json`：

```json
{ "keep_alive_seconds": 0, "scrollback_lines": 10000 }
```

`0` 表示 daemon 永久保持；scrollback 上限 100,000 行。可通过 `Z3RM_SERVER_SETTINGS`、`Z3RM_SCROLLBACK_LINES` 和 `Z3RM_KEEP_ALIVE_SECONDS` 覆盖。运行中的 server 会热加载。

## Client 设置

在 settings UI 中配置字体、终端、主题、keymap、远程连接与扩展。继承的未知 Zed 选项不构成 Z3rm 保证。
