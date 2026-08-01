use std::{
    collections::{BTreeMap, HashMap},
    ops::Range,
    path::Path,
    sync::Arc,
};

use anyhow::Result;
use gpui::{App, AsyncApp, Context, Entity, Task};
use language::Buffer;
use text::{BufferSnapshot, Point};

use crate::{ProjectPath, buffer_store::BufferStore, worktree_store::WorktreeStore};

/// A text anchor retained by the bookmark store.
#[derive(Copy, Clone, Debug, Hash, PartialEq, Eq)]
pub struct BookmarkAnchor(text::Anchor);

impl BookmarkAnchor {
    pub fn anchor(&self) -> text::Anchor {
        self.0
    }
}

/// The row representation used by workspace persistence and editor tests.
#[derive(Clone, Debug, Hash, PartialEq, Eq, PartialOrd, Ord)]
pub struct SerializedBookmark {
    pub row: u32,
    pub label: String,
}

#[derive(Clone, Debug)]
struct Bookmark {
    anchor: BookmarkAnchor,
    label: String,
}

#[derive(Debug)]
struct BufferBookmarks {
    buffer: Entity<Buffer>,
    bookmarks: Vec<Bookmark>,
}

impl BufferBookmarks {
    fn new(buffer: Entity<Buffer>) -> Self {
        Self {
            buffer,
            bookmarks: Vec::new(),
        }
    }
}

/// Project-local bookmark state keyed by the live buffer entity.
///
/// Anchors move with buffer edits, so bookmarks remain attached to their
/// logical rows without rewriting them on every edit. Persistence is exposed
/// as row numbers for the existing workspace/editor boundary.
pub struct BookmarkStore {
    #[allow(dead_code)]
    buffer_store: Entity<BufferStore>,
    #[allow(dead_code)]
    worktree_store: Entity<WorktreeStore>,
    buffers: HashMap<gpui::EntityId, BufferBookmarks>,
}

impl BookmarkStore {
    pub fn new(
        worktree_store: Entity<WorktreeStore>,
        buffer_store: Entity<BufferStore>,
    ) -> Self {
        Self {
            buffer_store,
            worktree_store,
            buffers: HashMap::default(),
        }
    }

    pub fn abs_path_from_buffer(buffer: &Entity<Buffer>, cx: &App) -> Option<Arc<Path>> {
        worktree::File::from_dyn(buffer.read(cx).file())
            .map(|file| file.worktree.read(cx).absolutize(&file.path))
            .map(Arc::<Path>::from)
    }

    fn buffer_bookmarks_mut(
        &mut self,
        buffer: &Entity<Buffer>,
    ) -> &mut BufferBookmarks {
        self.buffers
            .entry(buffer.entity_id())
            .or_insert_with(|| BufferBookmarks::new(buffer.clone()))
    }

    fn buffer_bookmarks(&self, buffer: &Entity<Buffer>) -> Option<&BufferBookmarks> {
        self.buffers.get(&buffer.entity_id())
    }

    /// Toggle a bookmark on the anchor's row. The label is retained for later
    /// editing and is empty for an unnamed bookmark.
    pub fn toggle_bookmark(
        &mut self,
        buffer: Entity<Buffer>,
        anchor: text::Anchor,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let snapshot = buffer.read(cx).text_snapshot();
        let row = anchor.summary::<Point>(&snapshot).row;
        let entry = self.buffer_bookmarks_mut(&buffer);
        if let Some(index) = entry
            .bookmarks
            .iter()
            .position(|bookmark| bookmark.anchor.0.summary::<Point>(&snapshot).row == row)
        {
            entry.bookmarks.remove(index);
        } else {
            entry.bookmarks.push(Bookmark {
                anchor: BookmarkAnchor(anchor),
                label,
            });
        }
        if entry.bookmarks.is_empty() {
            self.buffers.remove(&buffer.entity_id());
        }
        cx.notify();
    }

    pub fn edit_bookmark(
        &mut self,
        buffer: &Entity<Buffer>,
        anchor: text::Anchor,
        label: String,
        cx: &mut Context<Self>,
    ) {
        let snapshot = buffer.read(cx).text_snapshot();
        let row = anchor.summary::<Point>(&snapshot).row;
        if let Some(entry) = self.buffers.get_mut(&buffer.entity_id())
            && let Some(bookmark) = entry
                .bookmarks
                .iter_mut()
                .find(|bookmark| bookmark.anchor.0.summary::<Point>(&snapshot).row == row)
        {
            bookmark.label = label;
            cx.notify();
        }
    }

    pub fn bookmarks_for_buffer(
        &mut self,
        buffer: Entity<Buffer>,
        range: Range<text::Anchor>,
        buffer_snapshot: &BufferSnapshot,
        _cx: &mut Context<Self>,
    ) -> Vec<BookmarkAnchor> {
        let Some(entry) = self.buffer_bookmarks(&buffer) else {
            return Vec::new();
        };
        entry
            .bookmarks
            .iter()
            .filter_map(|bookmark| {
                let anchor = bookmark.anchor.anchor();
                if !buffer_snapshot.can_resolve(&anchor)
                    || anchor.cmp(&range.start, buffer_snapshot).is_lt()
                    || anchor.cmp(&range.end, buffer_snapshot).is_gt()
                {
                    None
                } else {
                    Some(bookmark.anchor)
                }
            })
            .collect()
    }

    pub fn find_bookmark(
        store: Entity<BookmarkStore>,
        buffer: Entity<Buffer>,
        point: Point,
        cx: &App,
    ) -> Option<text::Anchor> {
        let snapshot = buffer.read(cx).text_snapshot();
        store.read(cx).buffer_bookmarks(&buffer)?.bookmarks.iter().find_map(|bookmark| {
            let anchor = bookmark.anchor.anchor();
            (anchor.summary::<Point>(&snapshot).row == point.row).then_some(anchor)
        })
    }

    pub fn all_bookmark_locations(
        this: Entity<BookmarkStore>,
        cx: &mut AsyncApp,
    ) -> Task<Result<HashMap<Entity<Buffer>, Vec<Range<Point>>>>> {
        let result = this.read_with(cx, |store, cx| {
            let mut locations = HashMap::new();
            for entry in store.buffers.values() {
                let snapshot = entry.buffer.read(cx).snapshot();
                let ranges = entry
                    .bookmarks
                    .iter()
                    .filter_map(|bookmark| {
                        let anchor = bookmark.anchor.anchor();
                        snapshot
                            .can_resolve(&anchor)
                            .then(|| anchor.summary::<Point>(&snapshot).row)
                            .map(|row| Point::row_range(row..row))
                    })
                    .collect::<Vec<_>>();
                if !ranges.is_empty() {
                    locations.insert(entry.buffer.clone(), ranges);
                }
            }
            locations
        });
        Task::ready(Ok(result))
    }

    pub fn all_serialized_bookmarks(
        &self,
        cx: &App,
    ) -> BTreeMap<Arc<Path>, Vec<SerializedBookmark>> {
        let mut result = BTreeMap::new();
        for entry in self.buffers.values() {
            let Some(path) = Self::abs_path_from_buffer(&entry.buffer, cx) else {
                continue;
            };
            let snapshot = entry.buffer.read(cx).snapshot();
            let mut rows = entry
                .bookmarks
                .iter()
                .filter_map(|bookmark| {
                    let anchor = bookmark.anchor.anchor();
                    snapshot.can_resolve(&anchor).then(|| SerializedBookmark {
                        row: snapshot.summary_for_anchor::<Point>(&anchor).row,
                        label: bookmark.label.clone(),
                    })
                })
                .collect::<Vec<_>>();
            rows.sort();
            rows.dedup();
            if !rows.is_empty() {
                result.insert(path, rows);
            }
        }
        result
    }

    pub fn clear_bookmarks(&mut self, cx: &mut Context<Self>) {
        self.buffers.clear();
        cx.notify();
    }
}

