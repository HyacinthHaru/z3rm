//! §16.9 The doc set the guest browses.
//!
//! The site's markdown is mounted at `/mnt/docs` over 9p, so the guest reads
//! the same files Astro renders and the two cannot drift. The build writes an
//! index beside them rather than having this scan the directory: the order is
//! the sidebar's order, which is editorial, and a `readdir` here would return
//! it in whatever order the filesystem chose.

use crate::markdown;

/// One browsable document.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct DocEntry {
    /// Path relative to the docs root, as written in the index.
    pub path: String,
    pub title: String,
}

/// Parse the index the website build writes: `path<TAB>title`, one per line.
///
/// A malformed line is skipped rather than failing the whole list — one bad
/// entry should cost that document, not the browser.
pub fn parse_index(source: &str) -> Vec<DocEntry> {
    source
        .lines()
        .filter_map(|line| {
            let (path, title) = line.split_once('\t')?;
            let (path, title) = (path.trim(), title.trim());
            if path.is_empty() || title.is_empty() {
                return None;
            }
            Some(DocEntry {
                path: path.to_string(),
                title: title.to_string(),
            })
        })
        .collect()
}

/// A document opened for reading.
#[derive(Clone, Debug)]
pub struct OpenDoc {
    pub title: String,
    pub lines: Vec<String>,
    pub offset: usize,
}

impl OpenDoc {
    /// Render `source` for a terminal `columns` wide.
    ///
    /// The frontmatter title wins over the index's when it disagrees: the
    /// document says what it is called, and the index is a copy.
    pub fn render(fallback_title: &str, source: &str, columns: usize) -> Self {
        let (title, _) = markdown::split_frontmatter(source);
        Self {
            title: title.unwrap_or_else(|| fallback_title.to_string()),
            lines: markdown::render_markdown(source, columns),
            offset: 0,
        }
    }

    pub fn max_offset(&self, viewport: usize) -> usize {
        self.lines.len().saturating_sub(viewport.max(1))
    }
}

/// Where the browser is in the list.
///
/// Movement saturates rather than wrapping: at the last document, pressing
/// down again should leave the cursor visibly stuck rather than jump to the
/// top, which reads as a mis-press.
pub fn move_selection(selected: usize, delta: isize, count: usize) -> usize {
    if count == 0 {
        return 0;
    }
    let last = count - 1;
    if delta.is_negative() {
        selected.saturating_sub(delta.unsigned_abs())
    } else {
        selected.saturating_add(delta as usize).min(last)
    }
}

#[cfg(test)]
mod tests {
    use super::{OpenDoc, move_selection, parse_index};

    #[test]
    fn the_index_carries_path_and_title() {
        let entries = parse_index("quick-start.md\tQuick start\nguide/cli.md\tCLI guide\n");

        assert_eq!(entries.len(), 2);
        assert_eq!(entries[0].path, "quick-start.md");
        assert_eq!(entries[0].title, "Quick start");
        assert_eq!(entries[1].path, "guide/cli.md");
    }

    /// One unreadable line should cost that document, not the whole browser.
    #[test]
    fn a_malformed_index_line_is_skipped_not_fatal() {
        let entries = parse_index("good.md\tGood\nno-tab-here\n\t\nother.md\tOther\n");

        assert_eq!(entries.len(), 2, "got {entries:?}");
        assert_eq!(entries[0].title, "Good");
        assert_eq!(entries[1].title, "Other");
    }

    #[test]
    fn a_document_titles_itself_from_its_frontmatter() {
        let doc = OpenDoc::render(
            "index says this",
            "---\ntitle: Quick start\n---\n\n# Quick start\n",
            80,
        );
        assert_eq!(doc.title, "Quick start");

        let untitled = OpenDoc::render("index says this", "# No frontmatter\n", 80);
        assert_eq!(
            untitled.title, "index says this",
            "the index is the fallback"
        );
    }

    /// A document shorter than the viewport has nowhere to scroll; without the
    /// clamp the reader would scroll a full page of blank lines past its end.
    #[test]
    fn a_short_document_does_not_scroll() {
        let doc = OpenDoc::render("t", "one line\n", 80);
        assert_eq!(doc.max_offset(26), 0);
    }

    #[test]
    fn selection_saturates_at_both_ends() {
        assert_eq!(move_selection(0, -1, 3), 0);
        assert_eq!(move_selection(0, 1, 3), 1);
        assert_eq!(move_selection(2, 1, 3), 2);
        assert_eq!(
            move_selection(0, 1, 0),
            0,
            "an empty list has no selection to move"
        );
    }
}
