# 0002 - Mux Domain Unification

**Status:** Accepted

## Context

The mux domain model originally followed Zed's pattern: a `Domain` trait with multiple implementations (local, SSH, Docker, etc.). Zed uses `async_trait` for dynamic dispatch across domain implementations. In z3rm, only one domain implementation exists: the local mux server. No SSH domain, no Docker domain, no remote domain — the transport layer (ADR-0003) handles remote connectivity orthogonally.

## Decision

Use a single concrete `MuxDomain` struct with no `Domain` trait. No `async_trait`, no dynamic dispatch, no trait objects. If a second domain implementation becomes necessary (e.g., embedded mode for tests), extract a trait at that point — not before.

## Consequences

- **Positive:** Zero `async_trait` overhead (no heap allocation per async call, no vtable indirection). Simpler code, easier inlining, better compile times. Direct method calls enable full inlining and optimization.
- **Negative:** Adding a second domain requires extracting a trait retroactively. Test doubles require the concrete type or a manual mock.
- **Mitigation:** Use `cfg(test)` module with a `MockMuxDomain` struct implementing the same inherent methods if testing demands it. Extract trait only when a second real implementation exists.