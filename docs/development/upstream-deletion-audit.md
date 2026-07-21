# Upstream Deletion Impact Audit

Date: 2026-07-22

## Summary

Checked all source crates for references to deleted upstream Zed crates and dead code paths.

## Findings

### PASS — No deleted crate references in active code
- `crates/z3rm/src/` — no references to vim, agent, collab, debugger, repl, node_runtime
- `crates/terminal_view/src/` — clean
- `crates/workspace/src/` — clean (channel/oneshot are futures::channel, not collab crate)

### PASS — Editor stubs correctly isolated
- `crates/editor/src/stubs/` contains DisableAiSettings, RevealStrategy, ProjectBufferExt, parse_zed_link
- These are intentional migration stubs (spec §8.1)

### PASS — workspace unimplemented!() are trait defaults
- `item.rs:258` clone_on_split — default impl panics if can_split()=true but not overridden
- `item.rs:288,297,305` save/save_as/reload — default impls for can_save()=true
- These are upstream Zed patterns, not migration holes

### PASS — dock.rs panic is debug-only guard
- `dock.rs:714` — `cfg!(debug_assertions)` panel priority uniqueness check (developer guard, not runtime)

### PARTIAL — Dead collab actions in workspace.rs
- `workspace.rs:257` — `FollowNextCollaborator` action defined but never dispatched
- `workspace.rs:8208` — `ShareProject` action defined but never dispatched
- Impact: Zero runtime effect (dead code, never triggered). Should be marked #[z3rm_todo("disabled-feature")] or removed.
- Priority: Low (cosmetic, no functional impact)

### PASS — No ZED_ static vars in z3rm main.rs
- Clean — no leftover ZED_ prefixed statics

## Conclusion

5/6 checks PASS. 1 PARTIAL (dead collab actions, zero runtime impact).
No blocking issues. The fork is cleanly separated from deleted upstream crates.
