//! Tests for the operable web terminal: real grid parsing through the
//! vendored alacritty core, byte input, and resize.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config, Term};
use alacritty_terminal::vte::ansi;

/// Feed raw PTY-style bytes into a fresh 80x24 term and return the rendered
/// viewport text, one string per screen line.
fn render_after(cols: usize, rows: usize, bytes: &[u8]) -> Vec<String> {
    let mut parser = ansi::Processor::<ansi::StdSyncHandler>::new();
    let mut term = Term::new(Config::default(), &TermSize::new(cols, rows), VoidListener);
    parser.advance(&mut term, bytes);
    viewport_text(&term, cols, rows)
}

/// Extract the visible viewport as plain text lines.
pub fn viewport_text<T: alacritty_terminal::event::EventListener>(
    term: &Term<T>,
    _cols: usize,
    rows: usize,
) -> Vec<String> {
    let content = term.renderable_content();
    let mut lines: Vec<String> = Vec::with_capacity(rows);
    let mut current = String::new();
    let mut current_line = None;
    for indexed in content.display_iter {
        let line = indexed.point.line.0;
        match current_line {
            None => {
                current_line = Some(line);
            }
            Some(previous) if previous != line => {
                lines.push(current.trim_end().to_string());
                current = String::new();
            }
            _ => {}
        }
        current_line = Some(line);
        current.push(indexed.cell.c);
    }
    if current_line.is_some() {
        lines.push(current.trim_end().to_string());
    }
    lines
}

#[test]
fn plain_bytes_reach_the_grid() {
    let lines = render_after(80, 24, b"hello web");
    assert_eq!(lines.first().map(String::as_str), Some("hello web"));
}

#[test]
fn newline_moves_to_next_row() {
    let lines = render_after(80, 24, b"one\r\ntwo");
    assert_eq!(lines.first().map(String::as_str), Some("one"));
    assert_eq!(lines.get(1).map(String::as_str), Some("two"));
}

#[test]
fn cursor_addressing_places_text() {
    // CUP to row 3 col 5 (1-based), then print.
    let lines = render_after(80, 24, b"\x1b[3;5Hpositioned");
    assert_eq!(lines.get(2).map(String::as_str), Some("    positioned"));
}

#[test]
fn erase_display_clears_previous_output() {
    let lines = render_after(80, 24, b"stale\x1b[2J\x1b[Hfresh");
    assert_eq!(lines.first().map(String::as_str), Some("fresh"));
    assert!(lines.iter().all(|line| !line.contains("stale")));
}
