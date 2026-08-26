//! §3.1-in-guest The JS boundary of the serial link.
//!
//! The guest's serial port is owned by the page's v86 emulator; this module is
//! the only place bytes cross the language boundary. Output arrives from JS
//! and is routed through [`serial_link`]; input is handed to JS, which writes
//! it into the emulator's serial input.

use crate::serial_link;

/// Feed one batch of the guest's serial output into the serial link.
///
/// Called from JS, ideally once per animation frame rather than once per byte:
/// a booting kernel prints faster than a frame. Before the mux server inside
/// the guest signals ready, these bytes are boot text the page renders itself;
/// after it, they are mux protocol frames.
#[wasm_bindgen::prelude::wasm_bindgen]
pub fn z3rm_v86_serial_bytes(bytes: &[u8]) {
    serial_link::on_guest_bytes(bytes);
}
