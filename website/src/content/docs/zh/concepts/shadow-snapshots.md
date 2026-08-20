---
title: Shadow snapshots
description: 独立于 Git 的崩溃安全细粒度文件历史。
translationKey: concept-snapshots
section: concepts
order: 4
status: verified
---

# Shadow snapshots

Shadow snapshot 在 worktree 所在主机记录文件版本。它补充 Git，不替代 commit 或 branch。

## 顺序与持久性

单调 SeqNo 排序版本；单写线程在 restore 写文件前先追加并同步 WAL；delta chain 有上限并通过 Rope 重放。

## 检查与恢复

```sh
z3rm list-changes -t work
z3rm list-versions -t work src/main.rs
z3rm show-version -t work src/main.rs VERSION
z3rm restore -t work src/main.rs VERSION
```
