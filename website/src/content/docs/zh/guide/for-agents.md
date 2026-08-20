---
title: 面向 Agent 使用 Z3rm
description: 不夺取人类上下文的确定性终端控制协议。
translationKey: guide-agents
section: guide
order: 5
status: verified
---

# 面向 Agent 使用 Z3rm

Z3rm 暴露终端会话，不内置 LLM。Agent 通过确定性 CLI 与普通退出状态控制它。

## 先发现，再操作

```sh
z3rm ls -F '#{session_name}	#{session_attached}'
z3rm list-windows -t work -F '#{window_index}	#{window_name}	#{window_active}'
z3rm list-panes -t work -F '#{pane_id}	#{pane_current_path}	#{pane_dead}'
```

始终使用 `work:0.1` 这样的显式目标，不依赖另一个客户端的焦点。

## 输入与观察分离

用 `send-keys -l` 发送字面文本，再单独发送 `Enter`。用 `capture-pane`、`list-commands` 或 `search-scrollback` 观察输出，不根据耗时推断完成。

## 处理失败

检查每个退出状态。搜索无匹配返回非零；格式字段会报告 dead pane；传输错误必须上报。轮询应有界，并在命令标记报告 `exit=N`、`done` 或 pane 结束时停止。

## 与人类协作

除非需要可见焦点，不要调用 `select-pane`。优先使用专用 pane，报告所用目标，并把文件修改留给人类 diff review。
