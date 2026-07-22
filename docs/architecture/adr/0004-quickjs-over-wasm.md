# 0004 - QuickJS over WASM

**Status:** Accepted

## Context

z3rm's extension system must support: JavaScript/TypeScript (developer familiarity, npm ecosystem), hot-reload (edit-refresh cycle), sandboxing (untrusted extensions), and cross-platform (Linux, macOS, Windows). Options considered: V8 (heavy, complex embedding, no stable Rust API), Wasmtime/WASM (WASM components model immature, no native JS engine), QuickJS via `rquickjs` (lightweight, pure Rust bindings, ES2023 support, synchronous API, easy embedding).

## Decision

Use QuickJS via `rquickjs` crate as the extension runtime. Extensions are ES modules loaded into isolated QuickJS contexts. Each extension gets a sandboxed context with capability-based API surface (no direct FFI, no `eval`, no `Function` constructor). Hot-reload via context teardown/reload on file change. npm dependencies bundled via esbuild at install time (offline-first, no runtime network).

## Consequences

- **Positive:** Tiny runtime (~1MB), fast startup (~1ms), pure Rust (no V8 build chain), synchronous API fits GPUI task model, ES2023 support sufficient for modern JS/TS, easy sandboxing via context isolation.
- **Negative:** No JIT (interpreter-only, ~10-50x slower than V8 for CPU-intensive work), no WASM SIMD/threads in QuickJS yet, software sandbox only (no hardware-enforced isolation like WASM), single-threaded per context (extensions run on extension host thread pool).
- **Mitigation:** CPU-intensive work offloaded to native Rust commands (via capability API). Sandbox is defense-in-depth; extensions are user-installed (trusted). Extension host runs on dedicated thread pool. WASM components as future extension target (Phase 15+).