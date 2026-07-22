# 0007 - Editor Read-Only Preservation

**Status:** Accepted

## Context

z3rm needs a read-only editor component for viewing text output (log files, diff output, config files) within terminal panes. Zed's `editor` crate is a full-featured code editor with multi-cursor, inline completions, code actions, LSP integration, and collaborative editing. The crate is large (~50k lines) and tightly coupled to Zed's workspace model. Rather than rewrite an editor from scratch, we can preserve Zed's existing `read_only` mode — a feature already present and battle-tested in Zed's editor for viewing files without edits.

## Decision

Surgically prune the `editor` crate to a read-only viewer + diff component:

- **Preserve:** Tree-sitter syntax highlighting, display map (wrapped lines, folds), code folding, bracket matching, search (find/replace within buffer), diff rendering (added/moved/deleted lines), scrollback navigation, selection (copy only).
- **Remove:** Multi-cursor editing, inline completions, autocomplete popup, code actions, hover popups, go-to-definition, rename, format, LSP integration, snippet expansion, undo/redo per-buffer (replaced by shadow snapshots at pane level), collaborative editing (crdt).
- **Keep but prune:** Buffer model (simplified: no transaction log, no undo stack), text layout (remove edit-time features: soft wrap indicators, cursor blink).
- **No `read_only` flag toggle** — the viewer is always read-only. Editing in terminal panes is a future feature (full-screen editor mode, Phase 12+).

The pruned editor becomes `z3rm_editor:z3emEditor` — a GPUI `View<Editor>` that renders a read-only buffer. Diff mode is a state flag on `Editor` (not a separate component).

## Consequences

- **Positive:** Preserves Zed's mature tree-sitter integration (400+ grammars, lazy parse, highlight cache). Display map and folding work without changes. Diff rendering (line-level add/remove/modify) works via upstream's `diff` support. Search (find in buffer) preserved. Selection/copy works.
- **Negative:** Code paths for edit features remain as dead branches (guarded by `#[cfg(any)]` or runtime checks). Editor crate still references `Workspace`, `Project`, `Language` — must be patched to compile without those. Tree-sitter queries may reference language-specific features (comments, strings) that are irrelevant for display-only mode.
- **Mitigation:** `read_only = true` as compile-time constant where practical (dead code elided by optimizer). `z3rm_workspace` provides stub implementations for `Workspace`/`Project` types. Tree-sitter queries are still useful for highlighting; non-syntax queries (LSP-based semantics) removed. Diff rendering uses upstream's `DiffSnapshot` struct (unchanged).