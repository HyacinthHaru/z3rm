//! Z3rm GPUI WebAssembly application entry point.
//!
//! Boots the production workspace tree through GPUI's browser platform.

use gpui::{App, Application};
use gpui_web::WebPlatform;
use std::{cell::RefCell, rc::Rc, sync::Arc};

thread_local! {
    static APPLICATION: RefCell<Option<gpui::ApplicationHandle>> = const { RefCell::new(None) };
}

fn main() {
    console_error_panic_hook::set_once();
    gpui_web::init_logging();
    let platform = Rc::new(WebPlatform::new(false));
    let http_client = Arc::new(platform.fetch_http_client());
    let handle = Application::with_platform(platform)
        .with_http_client(http_client)
        .run_embedded(|cx: &mut App| {
            z3rm_web::open_real_workspace(cx);
            cx.activate(true);
        });

    APPLICATION.with(|application| {
        application.replace(Some(handle));
    });
}
