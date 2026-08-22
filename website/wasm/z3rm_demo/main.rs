use gpui::{
    App, AppContext, Application, Bounds, Context, InteractiveElement, IntoElement, ParentElement,
    Render, StatefulInteractiveElement, Styled, Window, WindowBounds, WindowOptions, div, point,
    px, rgb, size,
};
use gpui_web::WebPlatform;
use mux_protocol::{
    LayoutNode as ProtoLayoutNode, LayoutTree as ProtoLayoutTree, PaneInfo, PaneLeaf,
    SessionSnapshot, SplitNode, TabInfo, TerminalSize, layout_node,
};
use std::{cell::RefCell, rc::Rc, sync::Arc};
use z3rm_web::WebTerminal;

#[path = "../../../crates/workspace/src/layout_projection.rs"]
mod layout_projection;

use layout_projection::{LayoutTree, ProjectionConfig};

const TAB_IDS: [&str; 3] = ["window-0", "window-1", "window-2"];

thread_local! {
    static APPLICATION: RefCell<Option<gpui::ApplicationHandle>> = const { RefCell::new(None) };
}

struct Z3rmDemo {
    snapshot: SessionSnapshot,
    reconnect_count: u32,
}

impl Z3rmDemo {
    fn new(_cx: &mut Context<Self>) -> Self {
        let tab_id = requested_tab();
        let focused_pane_id = match tab_id {
            "window-1" => "pane-logs",
            "window-2" => "pane-shell",
            _ => "pane-editor",
        };
        Self {
            snapshot: server_snapshot(tab_id, focused_pane_id),
            reconnect_count: 0,
        }
    }

    fn select_tab(&mut self, tab_id: &str, cx: &mut Context<Self>) {
        let focused_pane_id = match tab_id {
            "window-1" => "pane-logs",
            "window-2" => "pane-shell",
            _ => "pane-editor",
        };
        self.snapshot = server_snapshot(tab_id, focused_pane_id);
        cx.notify();
    }

    fn focus_pane(&mut self, pane_id: &str, cx: &mut Context<Self>) {
        self.snapshot.focused_pane_id = pane_id.to_string();
        cx.notify();
    }

    fn reconcile(&mut self, cx: &mut Context<Self>) {
        self.reconnect_count = self.reconnect_count.saturating_add(1);
        let tab_index = TAB_IDS
            .iter()
            .position(|id| *id == self.snapshot.focused_tab_id)
            .unwrap_or(0);
        let next_tab = TAB_IDS[(tab_index + 1) % TAB_IDS.len()];
        let focused_pane_id = match next_tab {
            "window-1" => "pane-logs",
            "window-2" => "pane-shell",
            _ => "pane-editor",
        };
        self.snapshot = server_snapshot(next_tab, focused_pane_id);
        cx.notify();
    }
}

impl Render for Z3rmDemo {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let viewport = window.viewport_size();
        let tab_height = px(38.0);
        let status_height = px(50.0);
        let terminal_height = (viewport.height - tab_height - status_height).max(px(180.0));
        let projection = self
            .snapshot
            .layout
            .as_ref()
            .map(LayoutTree::from_proto)
            .map(|layout| {
                layout.project(ProjectionConfig {
                    available_bounds: Bounds::new(
                        point(px(0.0), px(0.0)),
                        size(viewport.width, terminal_height),
                    ),
                    splitter_width: px(6.0),
                })
            });

        let active_tab = self
            .snapshot
            .tabs
            .iter()
            .find(|tab| tab.id == self.snapshot.focused_tab_id);

        let terminal_surface =
            if let (Some(projection), Some(active_tab)) = (projection, active_tab) {
                let panes = active_tab.panes.iter().filter_map(|pane| {
                    let bounds = *projection.pane_bounds.get(&pane.id)?;
                    let pane_id = pane.id.clone();
                    let focused = pane.id == self.snapshot.focused_pane_id;
                    Some(
                        div()
                            .id(pane.id.clone())
                            .absolute()
                            .left(bounds.origin.x)
                            .top(bounds.origin.y)
                            .w(bounds.size.width)
                            .h(bounds.size.height)
                            .overflow_hidden()
                            .border_1()
                            .border_color(if focused {
                                rgb(0xc7a363)
                            } else {
                                rgb(0x30343a)
                            })
                            .bg(rgb(0x121417))
                            .cursor_pointer()
                            .on_click(cx.listener(move |this, _, _, cx| {
                                this.focus_pane(&pane_id, cx);
                            }))
                            .child(render_pane(pane, focused)),
                    )
                });
                div()
                    .relative()
                    .w_full()
                    .h(terminal_height)
                    .bg(rgb(0x0d0f12))
                    .children(panes)
            } else {
                div()
                    .w_full()
                    .h(terminal_height)
                    .flex()
                    .items_center()
                    .justify_center()
                    .text_color(rgb(0xa2a8b3))
                    .child("Awaiting authoritative session snapshot")
            };

        let tabs = self.snapshot.tabs.iter().map(|tab| {
            let tab_id = tab.id.clone();
            let selected = tab.id == self.snapshot.focused_tab_id;
            div()
                .id(tab.id.clone())
                .h_full()
                .px_4()
                .flex()
                .items_center()
                .gap_2()
                .cursor_pointer()
                .border_b_2()
                .border_color(if selected {
                    rgb(0xc7a363)
                } else {
                    rgb(0x202328)
                })
                .bg(if selected {
                    rgb(0x24272d)
                } else {
                    rgb(0x191c20)
                })
                .text_color(if selected {
                    rgb(0xf3f0e8)
                } else {
                    rgb(0xa2a8b3)
                })
                .on_click(cx.listener(move |this, _, _, cx| {
                    this.select_tab(&tab_id, cx);
                }))
                .child(format!("{}  {}", tab.title, tab.panes.len()))
        });

        div()
            .size_full()
            .bg(rgb(0x101215))
            .text_color(rgb(0xe8e9eb))
            .font_family("IBM Plex Mono")
            .child(
                div()
                    .h(tab_height)
                    .w_full()
                    .flex()
                    .items_center()
                    .bg(rgb(0x191c20))
                    .border_b_1()
                    .border_color(rgb(0x30343a))
                    .children(tabs),
            )
            .child(terminal_surface)
            .child(
                div()
                    .h(status_height)
                    .w_full()
                    .px_4()
                    .flex()
                    .items_center()
                    .justify_between()
                    .border_t_1()
                    .border_color(rgb(0x30343a))
                    .bg(rgb(0x17191d))
                    .text_sm()
                    .child(
                        div()
                            .flex()
                            .gap_4()
                            .child("SERVER CANONICAL")
                            .child(format!("session {}", self.snapshot.session_id))
                            .child(format!("focus {}", self.snapshot.focused_pane_id)),
                    )
                    .child(
                        div()
                            .id("reconcile")
                            .px_3()
                            .py_1()
                            .border_1()
                            .border_color(rgb(0x50565f))
                            .rounded_sm()
                            .cursor_pointer()
                            .text_color(rgb(0xc7a363))
                            .on_click(cx.listener(|this, _, _, cx| this.reconcile(cx)))
                            .child(format!("RECONNECT / RECONCILE  {}", self.reconnect_count)),
                    ),
            )
    }
}

thread_local! {
    /// One real emulator per pane. Each renders through the same vendored
    /// alacritty grid the native client uses (z3rm_web::WebTerminal), fed with
    /// the pane's session transcript as PTY-style bytes.
    static PANE_TERMINALS: RefCell<std::collections::HashMap<String, WebTerminal>> =
        RefCell::new(std::collections::HashMap::new());
}

fn pane_terminal(pane_id: &str) -> WebTerminal {
    let mut terminal = WebTerminal::new(120, 32);
    if let Some(bytes) = pane_transcript_bytes(pane_id) {
        terminal.feed(bytes);
    }
    terminal
}

fn pane_transcript_bytes(pane_id: &str) -> Option<&'static [u8]> {
    match pane_id {
        "pane-editor" => Some(b"$ z3rm attach -t work\r\nattached: work/window-0/pane-editor\r\n\r\nserver snapshot generation 1842\r\nlayout: left_right [0.62, 0.38]\r\nhistory: 4,096 rows \xc2\xb7 authoritative\r\n"),
        "pane-tests" => Some(b"$ cargo test -p mux_server --lib\r\nrunning 128 tests\r\ntest layout::wire_depth ... ok\r\ntest reconnect::snapshot ... ok\r\ntest result: ok. 128 passed\r\n"),
        "pane-logs" => Some(b"$ z3rm list-panes -F '#{pane_id} #{pane_generation}'\r\npane-logs 907\r\npane-metrics 332\r\n\r\nPaneDirty(pane-logs) \xe2\x86\x92 fetch_grid_update(906)\r\n"),
        "pane-metrics" => Some(b"mux-server  uptime 19h42m\r\nsessions    3\r\nwindows     8\r\npanes       17\r\nclients     2\r\n"),
        "pane-shell" => Some(b"$ printf 'state survives the window\\n'\r\nstate survives the window\r\n$ _"),
        _ => None,
    }
}

fn render_pane(pane: &PaneInfo, focused: bool) -> impl IntoElement {
    let lines = PANE_TERMINALS.with(|slot| {
        let mut map = slot.borrow_mut();
        map.entry(pane.id.clone())
            .or_insert_with(|| pane_terminal(&pane.id))
            .viewport_lines()
    });
    div()
        .size_full()
        .flex()
        .flex_col()
        .child(
            div()
                .h(px(28.0))
                .px_3()
                .flex()
                .items_center()
                .justify_between()
                .bg(if focused {
                    rgb(0x312b23)
                } else {
                    rgb(0x202328)
                })
                .text_xs()
                .child(format!("{}  ·  {}", pane.title, pane.cwd))
                .child(format!(
                    "gen {}  {}×{}",
                    pane.generation,
                    pane.size.as_ref().map_or(0, |size| size.cols),
                    pane.size.as_ref().map_or(0, |size| size.rows)
                )),
        )
        .child(
            div()
                .flex_1()
                .p_3()
                .overflow_hidden()
                .text_sm()
                .line_height(px(19.0))
                .text_color(rgb(0xc9cdd4))
                .children(lines.into_iter().map(|line| div().child(line))),
        )
}


fn server_snapshot(focused_tab_id: &str, focused_pane_id: &str) -> SessionSnapshot {
    let tabs = vec![
        tab("window-0", "work", &["pane-editor", "pane-tests"]),
        tab("window-1", "observe", &["pane-logs", "pane-metrics"]),
        tab("window-2", "shell", &["pane-shell"]),
    ];
    let root = match focused_tab_id {
        "window-1" => split(
            "split-observe",
            2,
            vec![leaf("pane-logs"), leaf("pane-metrics")],
            vec![0.68, 0.32],
        ),
        "window-2" => leaf("pane-shell"),
        _ => split(
            "split-work",
            1,
            vec![leaf("pane-editor"), leaf("pane-tests")],
            vec![0.62, 0.38],
        ),
    };
    SessionSnapshot {
        tabs,
        layout: Some(ProtoLayoutTree { root: Some(root) }),
        focused_pane_id: focused_pane_id.to_string(),
        focused_tab_id: focused_tab_id.to_string(),
        session_id: "work".to_string(),
    }
}

fn tab(id: &str, title: &str, pane_ids: &[&str]) -> TabInfo {
    TabInfo {
        id: id.to_string(),
        title: title.to_string(),
        panes: pane_ids
            .iter()
            .enumerate()
            .map(|(index, pane_id)| PaneInfo {
                id: (*pane_id).to_string(),
                cwd: format!("~/z3rm/{}", title),
                title: pane_id.trim_start_matches("pane-").to_string(),
                command: "zsh".to_string(),
                generation: 300 + index as u64 * 607,
                size: Some(TerminalSize {
                    cols: 120,
                    rows: 32,
                }),
                is_alive: true,
                zoomed: false,
            })
            .collect(),
    }
}

fn leaf(pane_id: &str) -> ProtoLayoutNode {
    ProtoLayoutNode {
        id: format!("leaf-{pane_id}"),
        node: Some(layout_node::Node::Pane(PaneLeaf {
            pane_id: pane_id.to_string(),
        })),
    }
}

fn split(
    id: &str,
    direction: i32,
    children: Vec<ProtoLayoutNode>,
    ratios: Vec<f32>,
) -> ProtoLayoutNode {
    ProtoLayoutNode {
        id: id.to_string(),
        node: Some(layout_node::Node::Split(SplitNode {
            direction,
            children,
            ratios,
        })),
    }
}

fn requested_tab() -> &'static str {
    let search = web_sys::window()
        .and_then(|window| window.location().search().ok())
        .unwrap_or_default();
    match search.trim_start_matches('?') {
        "window=window-1" => "window-1",
        "window=window-2" => "window-2",
        _ => "window-0",
    }
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
            if let Err(error) = cx.open_window(
                WindowOptions {
                    window_bounds: Some(WindowBounds::Windowed(bounds)),
                    ..Default::default()
                },
                |_, cx| cx.new(Z3rmDemo::new),
            ) {
                log::error!("failed to open Z3rm GPUI web demo: {error:#}");
                return;
            }
            cx.activate(true);
        });
    APPLICATION.with(|application| {
        application.replace(Some(handle));
    });
}
