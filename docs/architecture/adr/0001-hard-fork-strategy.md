# 0001 - Hard Fork Strategy

**Status:** Accepted

## Context

z3rm is a hard fork of Zed editor. Zed's development velocity and architectural direction diverge significantly from z3rm's goals (terminal multiplexer, mosh-style transport, QuickJS extensions, shadow snapshots). Zed's architecture is editor-first; z3rm is mux-first with editor as a detachable component. Attempting to maintain a mergeable fork would require constant rebasing, conflict resolution, and architectural compromises that degrade both projects.

## Decision

z3rm is a hard fork of Zed. We will periodically cherry-pick upstream improvements (GPUI fixes, GPU rendering improvements, tree-sitter upgrades, GPUI bugfixes) via manual cherry-picks into a dedicated `upstream/` branch, then rebase/cherry-pick into `main`. No automatic merge strategy, no rebase-onto-upstream workflow.

Layered naming convention preserves cherry-pick compatibility:
- Upstream crates retain `zed_*` / `gpui` names
- z3rm crates use `z3rm_*` prefix
- Shared internal crates use `z3rm_*` prefix with `zed_` re-exports where needed for cherry-pick compatibility

GPUI updates are manually ported — GPUI is the only upstream crate we track closely.

## Consequences

- **Positive:** Zero merge conflicts from upstream architectural shifts. z3rm architecture evolves independently. GPUI improvements are selectively portable.
- **Negative:** GPUI upgrades require manual porting effort. Zed editor features (collaboration, AI, etc.) are not automatically available. Divergence increases over time.
- **Mitigation:** Maintain `upstream/` tracking branch. Schedule monthly cherry-pick windows for GPUI. Document porting checklist in CONTRIBUTING.md.