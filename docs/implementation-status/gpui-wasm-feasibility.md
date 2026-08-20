# GPUI WASM feasibility

Audited on 2026-08-20 against commit `bcba9edbce`.

## Result

The GPUI core compiles for `wasm32-unknown-unknown`:

```sh
cargo check -p gpui --target wasm32-unknown-unknown
```

The Z3rm application does not. Its graph reaches OS-specific dependencies before a browser client boundary is established:

```text
errno 0.3.14: target OS "unknown" or "none" is unsupported
```

Reproducing command:

```sh
cargo check -p z3rm --target wasm32-unknown-unknown
```

## Decision

Do not ship a browser terminal imitation. The public site uses screenshots captured by Z3rm's GPUI headless regression harness and labels them as rendered evidence. A real browser demo would require a dedicated client crate with web transport, a browser-safe subset of GPUI, and no PTY or local-socket dependencies. That work is outside the documentation site and is not a foundation-spec requirement.
