//! z3rm in a browser tab.
//!
//! Everything past `boot` is the real client: `crates/z3rm_web` opens the same
//! `MultiWorkspace` the desktop binary does, over a mux server running in this
//! same wasm module.

#[cfg(not(target_family = "wasm"))]
fn main() {}

#[cfg(target_family = "wasm")]
use gpui::{App, Application};
#[cfg(target_family = "wasm")]
use gpui_web::{WebBackendPreference,  WebPlatform};
#[cfg(target_family = "wasm")]
use std::{cell::RefCell, rc::Rc, sync::Arc};

#[cfg(target_family = "wasm")]
thread_local! {
    static APPLICATION: RefCell<Option<gpui::ApplicationHandle>> = const { RefCell::new(None) };
}

#[cfg(target_family = "wasm")]
fn main() {
    console_error_panic_hook::set_once();
    gpui_web::init_logging();
    let platform = Rc::new(WebPlatform::new(false));
    let http_client = Arc::new(platform.fetch_http_client());
    let handle = Application::with_platform(platform)
        .with_http_client(http_client)
        // Fonts and icons come out of the same bundle the desktop reads, so the
        // asset source has to be set before anything renders.
        .with_assets(assets::Assets)
        .run_embedded(|cx: &mut App| {
            z3rm_web::boot(cx);
            cx.activate(true);
        });

    APPLICATION.with(|application| {
        application.replace(Some(handle));
    });
}
