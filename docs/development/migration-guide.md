# Migration Guide: Zed Fork to z3rm

This document describes the two-pass migration process that converts the Zed editor fork into a terminal multiplexer by pruning ~90 crates and marking every broken reference.

## Two-Pass Process

### Pass 1: Scan and Mark

Every subagent runs `cargo check --features z3rm-migration -p <crate>` and parses compiler errors. For each broken reference, it adds `#[z3rm_todo("category", "description")]` to the enclosing item.

**Categories:** `removed-crate` (deleted crate), `broken-ref` (pruned module), `stub` (placeholder), `disabled-feature` (disabled code).

**Acceptance:** `cargo check --features z3rm-migration` compiles with zero errors. Total hole count > 0.

### Pass 2: Fix Holes Category by Category

Fixing a hole = deleting the `#[z3rm_todo]` attribute AND resolving the underlying reference.

1. **removed-crate** -- delete imports, stubs, and feature-gated branches for deleted crates
2. **broken-ref** -- prune modules in retained crates (workspace, project, editor, terminal_view)
3. **stub + disabled-feature** -- delete stubs, clean up feature gates

**Acceptance:** Total hole count = 0, both with and without `--features z3rm-migration`.

## Milestone Verification (M0-M4)

| Milestone | Criteria |
|---|---|
| M0 (Foundation) | `z3rm_macros` crate exists, `#[z3rm_todo]` macro compiles |
| M1 (Pass 1 done) | All holes marked, `cargo check --features z3rm-migration` compiles |
| M2 (removed-crate = 0) | All deleted-crate references resolved |
| M3 (broken-ref = 0) | All retained-crate pruning complete |
| M4 (All holes = 0) | `#[z3rm_todo]` count = 0, clean build without feature flag |

## .rs.old Discipline

When replacing a file during migration, preserve the original as `.rs.old`. Delete these files when M4 passes.

## Check Commands

```sh
# During migration (holes are OK)
cargo check --features z3rm-migration

# Count remaining holes
cargo run -p z3rm_macros --bin count_todos

# Final gate (no feature flag, no compile_error! triggers)
cargo check
```