//! Operable terminal core for the web client.
//!
//! Wraps the vendored alacritty `Term` (the same grid/vte emulation the native
//! client uses) behind a small API the GPUI WebAssembly demo and the full
//! browser client can drive: feed raw PTY-style bytes, read the viewport, and
//! resize. No PTY is involved — bytes come from a browser-side shell sandbox
//! or, in the full client, from the mux server's grid stream.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;

pub struct WebTerminal {
    term: Term<VoidListener>,
    parser: ansi::Processor<ansi::StdSyncHandler>,
    columns: usize,
    screen_lines: usize,
}

impl WebTerminal {
    pub fn new(columns: usize, screen_lines: usize) -> Self {
        Self {
            term: Term::new(
                Config::default(),
                &TermSize::new(columns, screen_lines),
                VoidListener,
            ),
            parser: ansi::Processor::<ansi::StdSyncHandler>::new(),
            columns,
            screen_lines,
        }
    }

    /// Feed raw terminal output bytes (what a PTY would emit) into the
    /// emulator.
    pub fn feed(&mut self, bytes: &[u8]) {
        self.parser.advance(&mut self.term, bytes);
    }

    /// The visible viewport as plain text, one entry per screen line with
    /// trailing whitespace trimmed. Empty lines are kept so callers can index
    /// by row.
    pub fn viewport_lines(&self) -> Vec<String> {
        let content = self.term.renderable_content();
        let mut lines = vec![String::new(); self.screen_lines];
        for indexed in content.display_iter {
            let row = indexed.point.line.0;
            if row >= 0 && (row as usize) < self.screen_lines {
                lines[row as usize].push(indexed.cell.c);
            }
        }
        lines.into_iter().map(|line| line.trim_end().to_string()).collect()
    }

    /// Cursor position as (row, column), zero-based.
    pub fn cursor(&self) -> (usize, usize) {
        let cursor = self.term.renderable_content().cursor;
        (cursor.point.line.0.max(0) as usize, cursor.point.column.0)
    }

    /// Resize the grid. Existing lines are kept where they still fit.
    pub fn resize(&mut self, columns: usize, screen_lines: usize) {
        self.term.resize(TermSize::new(columns, screen_lines));
        self.columns = columns;
        self.screen_lines = screen_lines;
    }

    /// Grid dimensions as (columns, rows).
    pub fn size(&self) -> (usize, usize) {
        (self.columns, self.screen_lines)
    }
}

#[cfg(target_arch = "wasm32")]
pub mod bridge {
    //! JavaScript entry points: drive one shared terminal from the page.
    //!
    //! `receive_shell_bytes(bytes)` feeds shell output into the grid,
    //! `receive_shell_result(command, stdout, stderr, exit_code)` is a
    //! convenience for line-oriented sandbox results, and `terminal_viewport()`
    //! returns the rendered screen.

    use super::WebTerminal;
    use std::{cell::RefCell, rc::Rc};
    use wasm_bindgen::prelude::*;

    thread_local! {
        static TERMINAL: Rc<RefCell<WebTerminal>> = Rc::new(RefCell::new(WebTerminal::new(80, 24)));
    }

    #[wasm_bindgen]
    pub fn receive_shell_bytes(bytes: &[u8]) {
        TERMINAL.with(|slot| slot.borrow_mut().feed(bytes));
    }

    #[wasm_bindgen]
    pub fn receive_shell_result(command: &str, stdout: &str, stderr: &str, exit_code: u32) {
        let mut bytes = Vec::new();
        if !command.is_empty() {
            bytes.extend_from_slice(format!("visitor@z3rm:~$ {command}\r\n").as_bytes());
        }
        if !stdout.is_empty() {
            bytes.extend_from_slice(stdout.as_bytes());
            if !stdout.ends_with('\n') {
                bytes.push(b'\r');
                bytes.push(b'\n');
            }
        }
        if !stderr.is_empty() {
            bytes.extend_from_slice(stderr.as_bytes());
            if !stderr.ends_with('\n') {
                bytes.push(b'\r');
                bytes.push(b'\n');
            }
        }
        if exit_code != 0 {
            bytes.extend_from_slice(format!("exit {exit_code}\r\n").as_bytes());
        }
        TERMINAL.with(|slot| slot.borrow_mut().feed(&bytes));
    }

    #[wasm_bindgen]
    pub fn terminal_viewport() -> String {
        TERMINAL.with(|slot| slot.borrow().viewport_lines().join("\n"))
    }

    #[wasm_bindgen]
    pub fn terminal_cursor() -> String {
        TERMINAL.with(|slot| {
            let (row, column) = slot.borrow().cursor();
            format!("{row}:{column}")
        })
    }

    #[wasm_bindgen]
    pub fn resize_terminal(columns: usize, rows: usize) {
        TERMINAL.with(|slot| slot.borrow_mut().resize(columns, rows));
    }
}
