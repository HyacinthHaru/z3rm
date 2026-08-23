//! Z3rm GPUI WebAssembly application entry point.
//!
//! Instantiates the real `z3rm_web::Z3rmSessionView` running on GPUI's WebPlatform.

use gpui::{App, AppContext, Application, Bounds, WindowBounds, WindowOptions, px, size};
use gpui_web::WebPlatform;
use std::{cell::RefCell, rc::Rc, sync::Arc};
use z3rm_web::Z3rmSessionView;

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
            let bounds = Bounds::centered(None, size(px(980.0), px(560.0)), cx);
            let _window_handle = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| {
                    let view = cx.new(Z3rmSessionView::new);
                    z3rm_web::session_view::ACTIVE_VIEW.with(|slot| {
                        slot.replace(Some(view.clone()));
                    });
                    view
                },
            );

            cx.activate(true);
        });

    APPLICATION.with(|application| {
        application.replace(Some(handle));
    });
}
