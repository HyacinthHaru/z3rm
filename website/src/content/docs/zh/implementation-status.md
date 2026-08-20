---
title: 实现状态
description: 基于证据展示 Z3rm foundation spec 的实现状态。
translationKey: implementation-status
section: status
order: 1
status: mixed
---

# 实现状态

本站审计阶段会从版本库中的 foundation matrix 生成此页。只有实现路径与直接场景均验证通过的功能才能出现在首页。

## 状态定义

- **Verified：** 当前 tree 已直接运行验证。
- **Experimental：** 可执行，但仍有明确边界。
- **Missing：** 无法复现 spec 描述的行为。
- **Divergent：** 实现与 spec 不一致。
- **Not user-visible：** 内部约束，有直接代码或测试证据。

确认缺失或偏差后，条目会链接到包含复现步骤和验收标准的 GitHub issue。
