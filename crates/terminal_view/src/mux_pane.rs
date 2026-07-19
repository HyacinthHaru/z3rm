// §3.1 / §15.1 MuxPaneView — server-canonical terminal panel renderer.
//
// Unlike Zed's TerminalView, this view holds no local PTY or alacritty Term.
// It is a thin client of mux_server:
//   - Fetches FullGridSnapshot/GridDiff via MuxDomain::fetch_grid_update
//   - Renders cells + cursor directly in GPUI
//   - Sends input via MuxDomain::send_input / paste
//   - Listens to PaneDirty notifications to trigger fetch + repaint
//
// This is the most direct implementation of spec §3.1 "client never parses PTY bytes".

use gpui::{
    App, AppContext, Context, EventEmitter, FocusHandle, Focusable, FontWeight, KeyDownEvent,
    Keystroke, Pixels, Render, SharedString, Task, Window, div, px,
};
use mux::MuxDomain;
use mux_protocol::{
    fetch_grid_update_response::Update as FetchUpdate, notification::Event as NotifEvent,
    Cell as MuxCell, CursorState, FullGridSnapshot, GridDiff, Notification,
};
use std::sync::Arc;
use ui::prelude::*;

/// §3.3 MuxPaneView — GPUI view for a mux_server pane.
pub struct MuxPaneView {
    /// §3.10 server-assigned pane id
    pub pane_id: String,
    /// §3.10 MuxDomain client (shared Arc)
    pub domain: Arc<MuxDomain>,
    /// §3.3 current grid snapshot (fetched from server)
    snapshot: FullGridSnapshot,
    /// §3.3 client's known latest generation
    generation: u64,
    /// §3.3 fetch dedup flag
    fetch_in_flight: bool,
    /// GPUI focus handle
    focus_handle: FocusHandle,
    /// §3.4 notification subscription task
    notification_task: Option<Task<()>>,
}

/// §3.3 View events (for workspace to subscribe)
#[derive(Clone, Debug)]
pub enum MuxPaneEvent {
    TitleChanged,
    CloseRequested,
}

impl MuxPaneView {
    /// §3.3 Create view. Immediately triggers fetch_grid_update(0).
    pub fn new(
        pane_id: String,
        domain: Arc<MuxDomain>,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) -> Self {
        let focus_handle = cx.focus_handle();

        let mut view = Self {
            pane_id: pane_id.clone(),
            domain: domain.clone(),
            snapshot: FullGridSnapshot {
                cols: 80,
                rows: 24,
                cells: vec![MuxCell::default(); 80 * 24],
                cursor: Some(CursorState {
                    col: 0,
                    row: 0,
                    style: 1, // BLOCK
                    visible: true,
                }),
                alternate_screen: false,
            },
            generation: 0,
            fetch_in_flight: false,
            focus_handle,
            notification_task: None,
        };
        view.start_notification_listener(cx);
        view.schedule_fetch(cx);
        view
    }

    /// §3.4 Subscribe to PaneDirty / PaneRemoved, trigger fetch + repaint.
    fn start_notification_listener(&mut self, cx: &mut Context<Self>) {
        let pane_id = self.pane_id.clone();
        let rx = self.domain.subscribe();
        let weak = cx.entity().downgrade();

        let task = cx.spawn(async move |_, cx| {
            while let Ok(notif) = rx.recv().await {
                let Some(event) = notif.event else { continue };
                match event {
                    NotifEvent::PaneDirty(dirty) if dirty.pane_id == pane_id => {
                        let _ = weak.update(cx, |view, cx| view.schedule_fetch(cx));
                    }
                    NotifEvent::PaneRemoved(removed) if removed.pane_id == pane_id => {
                        let _ = weak.update(cx, |view, cx| {
                            view.notification_task = None;
                            cx.emit(MuxPaneEvent::CloseRequested);
                        });
                        break;
                    }
                    _ => {}
                }
            }
        });
        self.notification_task = Some(task);
    }

    /// §3.3 Schedule a fetch_grid_update. fetch_in_flight prevents concurrent fetches.
    fn schedule_fetch(&mut self, cx: &mut Context<Self>) {
        if self.fetch_in_flight {
            return;
        }
        self.fetch_in_flight = true;

        let pane_id = self.pane_id.clone();
        let domain = self.domain.clone();
        let since = self.generation;
        let weak = cx.entity().downgrade();

        cx.spawn(async move |_, cx| {
            let result = domain.fetch_grid_update(&pane_id, since).await;
            let _ = weak.update(cx, |view, cx| {
                view.fetch_in_flight = false;
                if let Ok(resp) = result {
                    view.apply_fetch_update(resp, cx);
                }
            });
        })
        .detach();
    }

    /// §3.3 Apply fetch response + notify GPUI to repaint.
    fn apply_fetch_update(
        &mut self,
        resp: mux_protocol::FetchGridUpdateResponse,
        cx: &mut Context<Self>,
    ) {
        self.generation = resp.to_generation;
        match resp.update {
            Some(FetchUpdate::FullSnapshot(full)) => {
                self.snapshot = full;
            }
            Some(FetchUpdate::Diff(diff)) => {
                self.apply_diff(&diff);
            }
            None => {}
        }
        cx.notify();
    }

    /// §3.3 Apply GridDiff to snapshot.cells (row-major flat array).
    fn apply_diff(&mut self, diff: &GridDiff) {
        let cols = self.snapshot.cols as usize;
        let rows = self.snapshot.rows as usize;
        for row_change in &diff.rows {
            let row_idx = row_change.row as usize;
            if row_idx >= rows {
                continue;
            }
            for (col_idx, cell) in row_change.cells.iter().enumerate() {
                if col_idx >= cols {
                    break;
                }
                let flat = row_idx * cols + col_idx;
                if flat < self.snapshot.cells.len() {
                    self.snapshot.cells[flat] = cell.clone();
                }
            }
        }
    }

    /// §3.10 keystroke → terminal bytes → send_input.
    fn dispatch_keystroke(&mut self, keystroke: &Keystroke, cx: &mut Context<Self>) {
        let bytes = keystroke_to_bytes(keystroke);
        if bytes.is_empty() {
            return;
        }
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        cx.background_executor()
            .spawn(async move {
                let _ = domain.send_input(&pane_id, &bytes).await;
            })
            .detach();
    }

    /// §3.3 Current snapshot title (for tabbar).
    pub fn title(&self) -> SharedString {
        if self.snapshot.cells.is_empty() {
            "terminal".into()
        } else {
            let cols = self.snapshot.cols as usize;
            let first_line: String = self.snapshot.cells[..cols]
                .iter()
                .map(|c| c.char.chars().next().unwrap_or(' '))
                .collect();
            let trimmed = first_line.trim();
            if trimmed.is_empty() {
                "terminal".into()
            } else {
                trimmed.to_string().into()
            }
        }
    }

    /// §3.10 resize — notify server, server bumps generation + pushes new diff.
    pub fn resize(&mut self, cols: u32, rows: u32, cx: &mut Context<Self>) {
        let domain = self.domain.clone();
        let pane_id = self.pane_id.clone();
        cx.background_executor()
            .spawn(async move {
                let _ = domain.resize_pane(&pane_id, cols, rows).await;
            })
            .detach();
    }
}

/// §3.1 keystroke → terminal byte sequence (xterm standard).
/// Handles Ctrl-letter, Alt (ESC prefix), arrow keys, function keys.
fn keystroke_to_bytes(keystroke: &Keystroke) -> Vec<u8> {
    let ctrl = keystroke.modifiers.control;
    let alt = keystroke.modifiers.alt;
    let mut bytes = Vec::new();

    if let Some(key_char) = keystroke.key_char.as_ref() {
        let ch = key_char.chars().next().unwrap_or('\0');
        if ctrl && ch.is_ascii_alphabetic() {
            let base = if ch.is_ascii_uppercase() { b'A' } else { b'a' };
            let b = (ch as u8).wrapping_sub(base).wrapping_add(1);
            if alt {
                bytes.push(0x1B);
            }
            bytes.push(b);
            return bytes;
        }
        if ctrl {
            let ctrl_byte = match ch {
                '@' => Some(0x00),
                '[' => Some(0x1B),
                '\\' => Some(0x1C),
                ']' => Some(0x1D),
                '^' => Some(0x1E),
                '_' => Some(0x1F),
                ' ' => Some(0x00),
                _ => None,
            };
            if let Some(b) = ctrl_byte {
                if alt {
                    bytes.push(0x1B);
                }
                bytes.push(b);
                return bytes;
            }
        }
        if alt {
            bytes.push(0x1B);
        }
        let mut buf = [0u8; 4];
        let s = ch.encode_utf8(&mut buf);
        bytes.extend_from_slice(s.as_bytes());
        return bytes;
    }

    let esc_seq: &[u8] = match keystroke.key.as_str() {
        "up" => b"\x1b[A",
        "down" => b"\x1b[B",
        "right" => b"\x1b[C",
        "left" => b"\x1b[D",
        "home" => b"\x1b[H",
        "end" => b"\x1b[F",
        "insert" => b"\x1b[2~",
        "delete" => b"\x1b[3~",
        "pageup" => b"\x1b[5~",
        "pagedown" => b"\x1b[6~",
        "tab" => b"\t",
        "backspace" => b"\x7f",
        "enter" => b"\r",
        "escape" => b"\x1b",
        _ => &[],
    };
    if esc_seq.is_empty() {
        return bytes;
    }
    if alt {
        bytes.push(0x1B);
    }
    bytes.extend_from_slice(esc_seq);
    bytes
}

impl Focusable for MuxPaneView {
    fn focus_handle(&self, _cx: &App) -> FocusHandle {
        self.focus_handle.clone()
    }
}

impl EventEmitter<MuxPaneEvent> for MuxPaneView {}

impl Render for MuxPaneView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let colors = cx.theme().colors();
        let bg = colors.editor_background;
        let fg = colors.text;

        let cols = self.snapshot.cols as usize;
        let rows = self.snapshot.rows as usize;
        let mut text_buf = String::with_capacity(cols * rows);
        for row in 0..rows {
            for col in 0..cols {
                let flat = row * cols + col;
                let ch = self.snapshot.cells[flat].char.chars().next().unwrap_or(' ');
                text_buf.push(ch);
            }
            if row + 1 < rows {
                text_buf.push('\n');
            }
        }
        let text_content: SharedString = text_buf.into();

        div()
            .track_focus(&self.focus_handle)
            .size_full()
            .bg(bg)
            .text_color(fg)
            .font_family("monospace")
            .text_size(px(14.0))
            .on_key_down(cx.listener(|this, event: &KeyDownEvent, _window, cx| {
                this.dispatch_keystroke(&event.keystroke, cx);
            }))
            .child(text_content)
    }
}
