//! z3rm web client: the browser-side mux client rendering the authoritative
//! server session state through GPUI's web platform.
//!
//! The crate is a library so the demo binary in `website/wasm/z3rm_demo` and
//! the full client share one implementation of the projection path.

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}
