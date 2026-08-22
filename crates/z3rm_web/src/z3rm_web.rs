//! z3rm web client: the browser-side mux client rendering the authoritative
//! server session state through GPUI's web platform.
//!
//! The crate is a library so the demo binary in `website/wasm/z3rm_demo` and
//! the full client share one implementation of the projection path.

pub fn version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}


mod web_terminal;

pub use web_terminal::WebTerminal;

mod web_terminal_tests {
    use crate::WebTerminal;

    #[test]
    fn fed_bytes_render_in_the_grid() {
        let mut terminal = WebTerminal::new(80, 24);
        terminal.feed(b"hello web");
        assert_eq!(terminal.viewport_lines().first().map(String::as_str), Some("hello web"));
    }

    #[test]
    fn newline_moves_to_the_next_row() {
        let mut terminal = WebTerminal::new(80, 24);
        terminal.feed(b"one\r\ntwo");
        let lines = terminal.viewport_lines();
        assert_eq!(lines.first().map(String::as_str), Some("one"));
        assert_eq!(lines.get(1).map(String::as_str), Some("two"));
    }

    #[test]
    fn cursor_addressing_places_text() {
        let mut terminal = WebTerminal::new(80, 24);
        terminal.feed(b"\x1b[3;5Hpositioned");
        assert_eq!(
            terminal.viewport_lines().get(2).map(String::as_str),
            Some("    positioned")
        );
    }

    #[test]
    fn resize_preserves_content_within_new_bounds() {
        let mut terminal = WebTerminal::new(80, 24);
        terminal.feed(b"before resize\r\nsecond line");
        terminal.resize(40, 10);
        let lines = terminal.viewport_lines();
        assert_eq!(lines.len(), 10);
        assert_eq!(lines.first().map(String::as_str), Some("before resize"));
    }

    #[test]
    fn erase_display_clears_previous_output() {
        let mut terminal = WebTerminal::new(80, 24);
        terminal.feed(b"stale\x1b[2J\x1b[Hfresh");
        let lines = terminal.viewport_lines();
        assert_eq!(lines.first().map(String::as_str), Some("fresh"));
        assert!(lines.iter().all(|line| !line.contains("stale")));
    }
}
