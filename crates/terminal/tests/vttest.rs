//! # VT terminal conformance tests (real alacritty grid assertions)
//!
//! §3.3 / Plan 23 — feeds escape sequences to a real alacritty Term and
//! verifies terminal state (cursor position, grid content, modes).
//!
//! These are NOT byte-value assertions on string literals — they drive
//! `Processor::advance(&mut term, bytes)` and read back grid state.

use alacritty_terminal::event::VoidListener;
use alacritty_terminal::index::{Column, Line, Point};
use alacritty_terminal::term::cell::{Cell, Flags};
use alacritty_terminal::term::test::TermSize;
use alacritty_terminal::term::{Config as TermConfig, Term};
use alacritty_terminal::vte::ansi::Processor;

fn make_term() -> Term<VoidListener> {
    let config = TermConfig::default();
    let size = TermSize::new(24, 80);
    Term::new(config, &size, VoidListener)
}

fn feed(term: &mut Term<VoidListener>, bytes: &[u8]) {
    let mut processor: Processor = Processor::new();
    processor.advance(term, bytes);
}

fn cell_at(term: &Term<VoidListener>, line: i32, col: usize) -> &Cell {
    let point = Point::new(Line(line), Column(col));
    &term.grid()[point]
}

fn cell_char(term: &Term<VoidListener>, line: i32, col: usize) -> char {
    cell_at(term, line, col)
        .c
        .to_string()
        .chars()
        .next()
        .unwrap_or(' ')
}

fn cursor(term: &Term<VoidListener>) -> (i32, usize) {
    let c = term.grid().cursor.point;
    (c.line.0, c.column.0)
}

// ============================================================================
// Cursor movement (CSI A/B/C/D)
// ============================================================================

#[test]
fn cursor_up_moves_cursor() {
    let mut term = make_term();
    feed(&mut term, b"\x1b[5;1H");
    let (line_before, _) = cursor(&term);
    feed(&mut term, b"\x1b[2A");
    let (line_after, _) = cursor(&term);
    assert_eq!(line_before - line_after, 2);
}

#[test]
fn cursor_down_moves_cursor() {
    let mut term = make_term();
    feed(&mut term, b"\x1b[1;1H");
    let (line_before, _) = cursor(&term);
    feed(&mut term, b"\x1b[3B");
    let (line_after, _) = cursor(&term);
    assert_eq!(line_after - line_before, 3);
}

#[test]
fn cursor_forward_moves_column() {
    let mut term = make_term();
    feed(&mut term, b"\x1b[1;1H");
    let (_, col_before) = cursor(&term);
    feed(&mut term, b"\x1b[5C");
    let (_, col_after) = cursor(&term);
    assert_eq!(col_after - col_before, 5);
}

#[test]
fn cursor_back_moves_column() {
    let mut term = make_term();
    feed(&mut term, b"\x1b[1;10H");
    let (_, col_before) = cursor(&term);
    feed(&mut term, b"\x1b[3D");
    let (_, col_after) = cursor(&term);
    assert_eq!(col_before - col_after, 3);
}

// ============================================================================
// Text output and grid content
// ============================================================================

#[test]
fn printed_text_appears_in_grid() {
    let mut term = make_term();
    feed(&mut term, b"Hello");
    assert_eq!(cell_char(&term, 0, 0), 'H');
    assert_eq!(cell_char(&term, 0, 1), 'e');
    assert_eq!(cell_char(&term, 0, 2), 'l');
    assert_eq!(cell_char(&term, 0, 3), 'l');
    assert_eq!(cell_char(&term, 0, 4), 'o');
}

#[test]
fn newline_advances_cursor_line() {
    let mut term = make_term();
    // Alacritty treats LF as line-feed only (no carriage return).
    // Terminals in raw mode need CRLF for "next line" semantics.
    feed(&mut term, b"AB\r\nCD");
    assert_eq!(cell_char(&term, 0, 0), 'A');
    assert_eq!(cell_char(&term, 0, 1), 'B');
    assert_eq!(cell_char(&term, 1, 0), 'C');
    assert_eq!(cell_char(&term, 1, 1), 'D');
}

#[test]
fn carriage_return_moves_to_col_zero() {
    let mut term = make_term();
    feed(&mut term, b"ABCDE\rXY");
    assert_eq!(cell_char(&term, 0, 0), 'X');
    assert_eq!(cell_char(&term, 0, 1), 'Y');
    assert_eq!(cell_char(&term, 0, 2), 'C');
}

// ============================================================================
// CSI H — cursor position (CUP)
// ============================================================================

#[test]
fn cup_sets_absolute_position() {
    let mut term = make_term();
    feed(&mut term, b"\x1b[10;20H");
    let (line, col) = cursor(&term);
    assert_eq!(line, 9);
    assert_eq!(col, 19);
}

#[test]
fn cup_origin_resets_to_home() {
    let mut term = make_term();
    feed(&mut term, b"\x1b[10;20H");
    feed(&mut term, b"\x1b[H");
    let (line, col) = cursor(&term);
    assert_eq!(line, 0);
    assert_eq!(col, 0);
}

// ============================================================================
// ED — erase display
// ============================================================================

#[test]
fn ed_erase_all_clears_screen() {
    let mut term = make_term();
    feed(&mut term, b"Hello\nWorld");
    feed(&mut term, b"\x1b[2J");
    for line in 0..2i32 {
        for col in 0..5 {
            assert_eq!(cell_char(&term, line, col), ' ');
        }
    }
}

#[test]
fn el_erase_to_end_of_line() {
    let mut term = make_term();
    feed(&mut term, b"Hello");
    feed(&mut term, b"\x1b[1;3H\x1b[K");
    assert_eq!(cell_char(&term, 0, 0), 'H');
    assert_eq!(cell_char(&term, 0, 1), 'e');
    assert_eq!(cell_char(&term, 0, 2), ' ');
    assert_eq!(cell_char(&term, 0, 3), ' ');
    assert_eq!(cell_char(&term, 0, 4), ' ');
}

// ============================================================================
// SGR — bold flag
// ============================================================================

#[test]
fn sgr_bold_sets_cell_flags() {
    let mut term = make_term();
    feed(&mut term, b"\x1b[1mB\x1b[0mN");
    assert!(
        cell_at(&term, 0, 0).flags.contains(Flags::BOLD),
        "SGR 1 should set BOLD flag"
    );
    assert!(
        !cell_at(&term, 0, 1).flags.contains(Flags::BOLD),
        "SGR 0 should clear BOLD flag"
    );
}
