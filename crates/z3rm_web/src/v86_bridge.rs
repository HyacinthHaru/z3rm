//! §3.1 v86 serial ↔ authoritative mux pane bridge.
//!
//! `Application::run_embedded` owns the browser run loop and does not return,
//! so the JS bootstrap cannot wait for wasm-bindgen exports. The bridge uses two
//! globals installed from opposite sides instead:
//!
//! - JS installs `window.z3rmV86SerialInput(Uint8Array)` before wasm starts.
//! - Rust installs `window.z3rmPushSerialOutput(Uint8Array)` after the pane exists.
//!
//! Pane writes therefore reach the guest serial input, and guest serial output
//! runs through `Pane::push_guest_output`, preserving the server-owned VT parser,
//! generation accounting, dirty rows, and notifications.

#[cfg(target_family = "wasm")]
use std::{cell::RefCell, sync::Arc};

#[cfg(target_family = "wasm")]
use js_sys::{Function, Reflect, Uint8Array};
#[cfg(target_family = "wasm")]
use wasm_bindgen::{JsCast as _, JsValue, closure::Closure};

#[cfg(target_family = "wasm")]
thread_local! {
    /// Keep the JS callback alive for the lifetime of the browser application.
    static OUTPUT_CALLBACK: RefCell<Option<Closure<dyn FnMut(Uint8Array)>>> = const {
        RefCell::new(None)
    };
}

/// Bind a server-owned pane to v86's serial port.
#[cfg(target_family = "wasm")]
pub fn install(pane: &Arc<mux_server::pane::Pane>, pane_id: &str) {
    let global = js_sys::global();

    match Reflect::get(&global, &JsValue::from_str("z3rmV86SerialInput"))
        .ok()
        .and_then(|value| value.dyn_into::<Function>().ok())
    {
        Some(input) => {
            pane.set_guest_input_handler(Box::new(move |bytes: &[u8]| {
                let bytes = Uint8Array::from(bytes);
                if let Err(error) = input.call1(&JsValue::UNDEFINED, &bytes) {
                    log::error!("v86 serial input callback failed: {error:?}");
                }
            }));
        }
        None => {
            log::warn!("window.z3rmV86SerialInput is unavailable; pane input will be dropped");
        }
    }

    let weak_pane = Arc::downgrade(pane);
    let output = Closure::wrap(Box::new(move |bytes: Uint8Array| {
        let Some(pane) = weak_pane.upgrade() else {
            return;
        };
        pane.push_guest_output(&bytes.to_vec());
    }) as Box<dyn FnMut(Uint8Array)>);

    if let Err(error) = Reflect::set(
        &global,
        &JsValue::from_str("z3rmPushSerialOutput"),
        output.as_ref(),
    ) {
        log::error!("failed to install v86 serial output callback: {error:?}");
        return;
    }
    if let Err(error) = Reflect::set(
        &global,
        &JsValue::from_str("z3rmPaneId"),
        &JsValue::from_str(pane_id),
    ) {
        log::warn!("failed to publish bridged pane id: {error:?}");
    }

    OUTPUT_CALLBACK.with(|slot| slot.replace(Some(output)));
}
