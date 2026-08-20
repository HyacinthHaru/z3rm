---
title: 面向人类使用 Z3rm
description: 用于 Shell、长任务和 Agent 审阅的持久工作流。
translationKey: guide-humans
section: guide
order: 4
status: verified
---

# 面向人类使用 Z3rm

## 给工作命名，而不是给窗口命名

为持久工作建立 session，例如 `z3rm new -s release`。在 session 内用 tab 与 pane 组织任务。

## 离开而不结束任务

Detach 或关闭客户端。daemon 保留 PTY、模拟器状态、scrollback 和布局；用 `z3rm attach -t release` 返回。

## 谨慎共享控制

人类 GUI 与 Agent CLI 可以定位同一个权威 pane。尽量显式指定目标，并给后台自动化单独 pane。

## 审阅修改

通过文件树与 diff review 检查 Agent 的文件。Shadow versions 提供本机细粒度历史。

## 主动恢复

Daemon 崩溃后，`z3rm recover --list` 展示持久布局元数据。Grid 内容不落盘，因此恢复需要明确确认。
