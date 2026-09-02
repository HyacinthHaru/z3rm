//! §16.9 Markdown as ANSI, for the docs the guest browses.
//!
//! The site's docs are the guest's content: the same files Astro renders are
//! mounted over 9p and read here. That means the subset is known rather than
//! guessed — headings, fenced code, `-` lists, inline code and bold, and
//! nothing else appears in them — so this is a line-oriented renderer rather
//! than a parser, and the guest binary keeps its single dependency on libc.
//!
//! Anything unrecognised passes through as text. A doc that grows a table
//! renders as the pipes it is written with, which is legible; refusing to
//! render it would not be.

/// A heading's own colour, so structure survives a terminal with no bold.
const HEADING_SGR: [&str; 3] = ["\x1b[38;5;45;1m", "\x1b[38;5;81;1m", "\x1b[38;5;117m"];
const CODE_SGR: &str = "\x1b[38;5;180m";
const FENCE_SGR: &str = "\x1b[48;5;235;38;5;187m";
const BULLET_SGR: &str = "\x1b[38;5;45m";
const BOLD_SGR: &str = "\x1b[1m";
const RESET: &str = "\x1b[0m";

/// Strip a leading `---` frontmatter block, returning the `title:` it declared.
///
/// The title is the only field the guest wants, and it is the one Astro's
/// sidebar shows, so the two lists read the same way.
pub fn split_frontmatter(source: &str) -> (Option<String>, &str) {
    let Some(rest) = source.strip_prefix("---\n") else {
        return (None, source);
    };
    let Some(end) = rest.find("\n---") else {
        return (None, source);
    };
    let (front, after) = rest.split_at(end);
    let body = after
        .strip_prefix("\n---\n")
        .or_else(|| after.strip_prefix("\n---"))
        .unwrap_or("");
    let title = front.lines().find_map(|line| {
        let value = line.strip_prefix("title:")?.trim();
        Some(value.trim_matches('"').to_string())
    });
    (title, body)
}

/// Render markdown into terminal lines, each carrying its own SGR and reset.
pub fn render_markdown(source: &str, columns: usize) -> Vec<String> {
    let width = columns.saturating_sub(4).max(20);
    let (_, body) = split_frontmatter(source);
    let mut lines = Vec::new();
    let mut in_fence = false;

    for raw in body.lines() {
        if let Some(info) = raw.trim_start().strip_prefix("```") {
            // The fence's own line carries the language, which is worth
            // showing: `sh` and `toml` blocks are read differently.
            if in_fence {
                in_fence = false;
                lines.push(String::new());
            } else {
                in_fence = true;
                lines.push(String::new());
                if !info.trim().is_empty() {
                    lines.push(format!("  {CODE_SGR}{}{RESET}", info.trim()));
                }
            }
            continue;
        }
        if in_fence {
            lines.push(format!(
                "  {FENCE_SGR} {:<width$} {RESET}",
                raw,
                width = width
            ));
            continue;
        }

        let trimmed = raw.trim_end();
        if trimmed.is_empty() {
            lines.push(String::new());
            continue;
        }
        if let Some((level, text)) = heading(trimmed) {
            let sgr = HEADING_SGR[level.min(HEADING_SGR.len() - 1)];
            lines.push(String::new());
            lines.push(format!("  {sgr}{}{RESET}", inline(text)));
            continue;
        }
        if let Some(item) = trimmed
            .trim_start()
            .strip_prefix("- ")
            .or_else(|| trimmed.trim_start().strip_prefix("* "))
        {
            for (index, wrapped) in wrap(&inline(item), width.saturating_sub(2))
                .into_iter()
                .enumerate()
            {
                if index == 0 {
                    lines.push(format!("  {BULLET_SGR}·{RESET} {wrapped}"));
                } else {
                    lines.push(format!("    {wrapped}"));
                }
            }
            continue;
        }
        for wrapped in wrap(&inline(trimmed), width) {
            lines.push(format!("  {wrapped}"));
        }
    }
    lines
}

fn heading(line: &str) -> Option<(usize, &str)> {
    let hashes = line.len() - line.trim_start_matches('#').len();
    if hashes == 0 || hashes > 4 {
        return None;
    }
    let text = line[hashes..].strip_prefix(' ')?;
    Some((hashes - 1, text))
}

/// Apply inline `code` and **bold**.
///
/// Unclosed markers stay as written: a stray backtick is far more likely to be
/// prose than an intent to colour the rest of the paragraph.
fn inline(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let bytes = text.as_bytes();
    let mut index = 0;
    while index < bytes.len() {
        if bytes[index] == b'`'
            && let Some(end) = text[index + 1..].find('`')
        {
            out.push_str(CODE_SGR);
            out.push_str(&text[index + 1..index + 1 + end]);
            out.push_str(RESET);
            index += end + 2;
            continue;
        }
        if text[index..].starts_with("**")
            && let Some(end) = text[index + 2..].find("**")
        {
            out.push_str(BOLD_SGR);
            out.push_str(&text[index + 2..index + 2 + end]);
            out.push_str(RESET);
            index += end + 4;
            continue;
        }
        let char_end = next_char_boundary(text, index);
        out.push_str(&text[index..char_end]);
        index = char_end;
    }
    out
}

fn next_char_boundary(text: &str, index: usize) -> usize {
    let mut end = index + 1;
    while end < text.len() && !text.is_char_boundary(end) {
        end += 1;
    }
    end.min(text.len())
}

/// Wrap on spaces, measuring only what is printed.
///
/// The SGR already inserted by `inline` costs no columns, so counting bytes
/// would wrap short by however much colour a line happens to carry.
fn wrap(text: &str, width: usize) -> Vec<String> {
    let mut lines = Vec::new();
    let mut current = String::new();
    let mut printed = 0;
    for word in text.split(' ') {
        let word_width = printed_width(word);
        if printed > 0 && printed + 1 + word_width > width {
            lines.push(std::mem::take(&mut current));
            printed = 0;
        }
        if printed > 0 {
            current.push(' ');
            printed += 1;
        }
        current.push_str(word);
        printed += word_width;
    }
    if !current.is_empty() || lines.is_empty() {
        lines.push(current);
    }
    lines
}

fn printed_width(text: &str) -> usize {
    let mut width = 0;
    let mut chars = text.chars();
    while let Some(character) = chars.next() {
        if character == '\x1b' {
            for escape in chars.by_ref() {
                if escape == 'm' {
                    break;
                }
            }
            continue;
        }
        width += 1;
    }
    width
}

#[cfg(test)]
mod tests {
    use super::{printed_width, render_markdown, split_frontmatter};

    #[test]
    fn frontmatter_yields_the_title_and_is_not_rendered() {
        let source = "---\ntitle: Quick start\norder: 1\n---\n\n# Quick start\n";
        let (title, body) = split_frontmatter(source);

        assert_eq!(title.as_deref(), Some("Quick start"));
        assert!(
            !body.contains("order:"),
            "frontmatter must not reach the page"
        );

        let rendered = render_markdown(source, 80).join("\n");
        assert!(!rendered.contains("order"), "got: {rendered}");
    }

    /// A doc with no frontmatter is still a doc; refusing to render it would
    /// lose content the site publishes.
    #[test]
    fn a_body_without_frontmatter_renders_whole() {
        let (title, body) = split_frontmatter("# Heading\n\ntext\n");
        assert_eq!(title, None);
        assert!(body.contains("# Heading"));
    }

    #[test]
    fn headings_lists_and_code_each_get_their_own_colour() {
        let rendered = render_markdown(
            "# Title\n\n## Section\n\n- first item\n- second item\n\n```sh\ncargo build\n```\n",
            80,
        )
        .join("\n");

        assert!(rendered.contains("Title"), "got: {rendered}");
        assert!(rendered.contains("Section"));
        assert!(rendered.contains("first item"));
        assert!(rendered.contains("cargo build"));
        // Three distinct roles must not collapse to one appearance, or the
        // structure the markdown carries is lost on the way to the terminal.
        let heading_sgr = rendered.matches("\x1b[38;5;45;1m").count();
        let fence_sgr = rendered.matches("\x1b[48;5;235;38;5;187m").count();
        assert_eq!(heading_sgr, 1, "one level-one heading");
        assert_eq!(fence_sgr, 1, "one fenced line");
    }

    #[test]
    fn fenced_code_is_not_reinterpreted_as_markdown() {
        let rendered =
            render_markdown("```sh\n# not a heading\n- not a list\n```\n", 80).join("\n");

        assert!(rendered.contains("# not a heading"), "got: {rendered}");
        assert!(rendered.contains("- not a list"));
        assert!(
            !rendered.contains("\x1b[38;5;45;1m# not a heading"),
            "code must not be styled as a heading"
        );
    }

    #[test]
    fn inline_code_and_bold_are_styled_and_unwrapped() {
        let rendered =
            render_markdown("Run `cargo build` for a **debug** binary.\n", 80).join("\n");

        assert!(rendered.contains("cargo build"), "got: {rendered}");
        assert!(!rendered.contains('`'), "the marker itself must not print");
        assert!(!rendered.contains("**"), "the marker itself must not print");
        assert!(
            rendered.contains("\x1b[1mdebug"),
            "bold must reach the terminal"
        );
    }

    /// An unclosed marker is prose. Colouring the rest of the paragraph
    /// because of one stray backtick is worse than printing the backtick.
    #[test]
    fn an_unclosed_marker_stays_as_written() {
        let rendered = render_markdown("a lone ` backtick and ** stars\n", 80).join("\n");

        assert!(rendered.contains('`'), "got: {rendered}");
        assert!(rendered.contains("**"));
    }

    /// Wrapping counts printed columns, not bytes. Inline colour is free on
    /// screen, so measuring it would wrap short by however much a line carries.
    #[test]
    fn wrapping_measures_columns_rather_than_bytes() {
        let plain = render_markdown(&format!("{}\n", "word ".repeat(40)), 40);
        let coloured = render_markdown(&format!("{}\n", "`word` ".repeat(40)), 40);

        assert_eq!(
            plain.len(),
            coloured.len(),
            "colour must not change how many lines a paragraph takes"
        );
        for line in &plain {
            assert!(printed_width(line) <= 40, "over-wide line: {line:?}");
        }
        for line in &coloured {
            assert!(printed_width(line) <= 40, "over-wide line: {line:?}");
        }
    }

    /// The docs are the input this exists for, so one of the real ones is the
    /// test. Synthetic markdown only proves the renderer handles markdown the
    /// test author thought of.
    #[test]
    fn a_published_document_renders_whole() {
        const QUICK_START: &str =
            include_str!("../../../website/src/content/docs/en/quick-start.md");

        let (title, _) = split_frontmatter(QUICK_START);
        assert_eq!(title.as_deref(), Some("Quick start"));

        let rendered = render_markdown(QUICK_START, 100).join("\n");
        assert!(!rendered.contains("translationKey"), "frontmatter leaked");
        assert!(
            rendered.contains("cargo build -p z3rm"),
            "a code block is missing"
        );
        assert!(
            rendered.contains("\x1b[48;5;235;38;5;187m"),
            "the code blocks lost their styling"
        );
        assert!(
            rendered.contains("\x1b[38;5;81;1m"),
            "the `##` sections lost their styling"
        );
        for line in render_markdown(QUICK_START, 100) {
            assert!(printed_width(&line) <= 100, "over-wide line: {line:?}");
        }
    }

    #[test]
    fn multibyte_text_is_not_split_mid_character() {
        let rendered = render_markdown("会话是工作，窗口只是视图。\n", 40).join("\n");
        assert!(rendered.contains("会话是工作"), "got: {rendered}");
    }
}
