//! §3.1 The guest end of a pane's pty: a v86 machine's serial port.
//!
//! Natively a pane owns a pty master and the kernel carries bytes to a child
//! process. In the browser the "child" is a Linux running inside v86, reached
//! over its emulated serial port, and JS owns the emulator. So this is the only
//! place the two directions cross the language boundary:
//!
//! * pane → guest: `WasmPty`'s input handler calls the JS `send`.
//! * guest → pane: JS calls [`z3rm_v86_serial_bytes`], which feeds
//!   `Pane::push_guest_output` — the same entry point the native read loop uses.
//!
//! Nothing about the emulator is modelled here. JS decides when the machine is
//! up and how to batch what it prints; this only has to be told.

use mux_server::pane::Pane;
use mux_server::wasm_server::WasmMuxServer;
use std::cell::RefCell;
use std::sync::Arc;
use wasm_bindgen::prelude::*;

#[wasm_bindgen]
unsafe extern "C" {
    /// Hand bytes to the guest's serial input.
    ///
    /// Defined by the page as `window.__z3rm_v86.send`. Missing until the
    /// emulator has been constructed, which is why this is fallible.
    #[wasm_bindgen(js_namespace = ["window", "__z3rm_v86"], js_name = send, catch)]
    fn v86_send(bytes: &[u8]) -> Result<(), JsValue>;
}

thread_local! {
    /// The pane the guest's serial output is routed to.
    ///
    /// One machine, one console, so one pane: a second v86 would be a second
    /// bridge, not another entry in a table.
    static BRIDGED_PANE: RefCell<Option<Arc<Pane>>> = const { RefCell::new(None) };
}

/// Point a pane's pty at the guest's serial port, in both directions.
///
/// Returns whether the pane was found. It will not be until the server has
/// finished spawning it, so the caller decides whether that is worth retrying.
pub fn attach(server: &Arc<WasmMuxServer>, pane_id: &str) -> bool {
    let Some(pane) = find_pane(server, pane_id) else {
        return false;
    };

    pane.set_guest_input_handler(Box::new(|bytes| {
        // A write that cannot reach the guest is a dropped keystroke, not a
        // reason to tear the pane down: the machine may still be booting.
        if let Err(error) = v86_send(bytes) {
            log::warn!("v86 serial input rejected {} bytes: {error:?}", bytes.len());
        }
    }));

    BRIDGED_PANE.with(|bridged| bridged.replace(Some(pane)));
    true
}

fn find_pane(server: &Arc<WasmMuxServer>, pane_id: &str) -> Option<Arc<Pane>> {
    server
        .sessions()
        .read()
        .iter()
        .find_map(|session| session.panes.read().get(pane_id).cloned())
}

/// Feed one batch of the guest's serial output into its pane.
///
/// Called from JS, ideally once per animation frame rather than once per byte:
/// a booting kernel prints faster than a frame, and every call here runs the
/// emulator parser and wakes the client.
#[wasm_bindgen]
pub fn z3rm_v86_serial_bytes(bytes: &[u8]) {
    BRIDGED_PANE.with(|bridged| {
        let Some(pane) = bridged.borrow().clone() else {
            // Output before the pane exists is boot noise from a machine the
            // page started early; there is nowhere to put it.
            return;
        };
        pane.push_guest_output(bytes);
    });
}

/// The size the pane last asked its pty for, as `rows` and `cols`.
///
/// The guest cannot be told its window size over a serial line, so the page
/// reads this and types `stty` into the shell instead.
#[wasm_bindgen]
pub fn z3rm_v86_pane_size() -> Option<Vec<u16>> {
    BRIDGED_PANE.with(|bridged| {
        let pane = bridged.borrow().clone()?;
        let size = pane.guest_pty_size();
        Some(vec![size.rows, size.cols])
    })
}
