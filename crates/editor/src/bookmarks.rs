use std::ops::Range;

use crate::stubs::ProjectBufferExt;
use gpui::Entity;
use language::Buffer;
use multi_buffer::{Anchor, MultiBufferOffset, MultiBufferSnapshot, ToOffset as _};
use project::{Project, bookmark_store::BookmarkStore};
use rope::Point;
use text::Bias;
use ui::{Context, Window};
use util::ResultExt as _;
use workspace::{Workspace, searchable::Direction};

use crate::display_map::DisplayRow;
use crate::{
    EditBookmark, Editor, GoToNextBookmark, GoToPreviousBookmark, MultibufferSelectionMode,
    SelectionEffects, ToggleBookmark, ToggleBookmarkWithLabel, ViewBookmarks, scroll::Autoscroll,
};

#[derive(Clone, Debug)]
struct BookmarkTarget {
    buffer: Entity<Buffer>,
    anchor: Anchor,
    buffer_anchor: text::Anchor,
}

impl Editor {
    fn bookmark_exists_for_target(
        _bookmark_store: &Entity<BookmarkStore>,
        _target: &BookmarkTarget,
        _cx: &mut Context<Self>,
    ) -> bool {
        // 只读编辑器：bookmark_store 的 find_bookmark 方法不存在，返回 false。
        false
    }

    pub fn set_show_bookmarks(&mut self, show_bookmarks: bool, cx: &mut Context<Self>) {
        self.show_bookmarks = Some(show_bookmarks);
        cx.notify();
    }

    pub fn toggle_bookmark(
        &mut self,
        _: &ToggleBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_bookmark_impl(false, window, cx);
    }

    pub fn toggle_bookmark_with_label(
        &mut self,
        _: &ToggleBookmarkWithLabel,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.toggle_bookmark_impl(true, window, cx);
    }

    fn toggle_bookmark_impl(
        &mut self,
        with_label: bool,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(bookmark_store) = self.bookmark_store.clone() else {
            return;
        };
        let Some(project) = self.project() else {
            return;
        };

        let snapshot = self.snapshot(window, cx);
        let multi_buffer_snapshot = snapshot.buffer_snapshot();

        let mut selections = self.selections.all::<Point>(&snapshot.display_snapshot);
        selections.sort_by_key(|s| s.head());
        selections.dedup_by_key(|s| s.head().row);

        let mut exist_targets: Vec<BookmarkTarget> = vec![];
        let mut absent_targets: Vec<BookmarkTarget> = vec![];

        for selection in &selections {
            let head = selection.head();
            let multibuffer_anchor = multi_buffer_snapshot.anchor_before(Point::new(head.row, 0));

            if let Some((buffer_anchor, _)) =
                multi_buffer_snapshot.anchor_to_buffer_anchor(multibuffer_anchor)
            {
                let buffer_id = buffer_anchor.buffer_id;
                if let Some(buffer) = project.read(cx).buffer_for_id(buffer_id, cx) {
                    let target = BookmarkTarget {
                        buffer,
                        anchor: multibuffer_anchor,
                        buffer_anchor,
                    };

                    if Self::bookmark_exists_for_target(&bookmark_store, &target, cx) {
                        exist_targets.push(target);
                    } else {
                        absent_targets.push(target);
                    }
                }
            }
        }

        if absent_targets.is_empty() {
            // All cursors are on existing bookmarks, remove all bookmarks.
            self.toggle_bookmarks(exist_targets, String::new(), cx);
        } else if with_label {
            // Only add new ones (prompting for a label) and leave existing ones unchanged.
            self.add_toggle_bookmark_blocks(absent_targets, bookmark_store, window, cx);
        } else {
            // Only add new (unnamed) bookmarks and leave existing ones unchanged.
            self.toggle_bookmarks(absent_targets, String::new(), cx);
        }

        cx.notify();
    }

    pub fn toggle_bookmark_at_row(&mut self, row: DisplayRow, cx: &mut Context<Self>) {
        let display_snapshot = self.display_snapshot(cx);
        let point = display_snapshot.display_point_to_point(row.as_display_point(), Bias::Left);
        let buffer_snapshot = self.buffer.read(cx).snapshot(cx);
        let anchor = buffer_snapshot.anchor_before(point);

        self.toggle_bookmark_at_anchor(anchor, cx);
    }

    pub fn toggle_bookmark_at_anchor(&mut self, _anchor: Anchor, _cx: &mut Context<Self>) {
        // 只读编辑器：bookmark_store 的 toggle_bookmark 方法不存在，跳过。
        // bookmark_store.update(cx, |bookmark_store, cx| {
        //     bookmark_store.toggle_bookmark(buffer, position, String::new(), cx);
        // });
    }

    pub fn edit_bookmark(&mut self, _: &EditBookmark, window: &mut Window, cx: &mut Context<Self>) {
        let snapshot = self.snapshot(window, cx);
        let multi_buffer_snapshot = snapshot.buffer_snapshot();
        let selection = self
            .selections
            .newest::<Point>(&snapshot.display_snapshot)
            .head();
        let anchor = multi_buffer_snapshot.anchor_before(Point::new(selection.row, 0));
        self.edit_bookmark_at_anchor(anchor, window, cx);
    }

    pub fn edit_bookmark_at_anchor(
        &mut self,
        _anchor: Anchor,
        _window: &mut Window,
        _cx: &mut Context<Self>,
    ) {
        // 只读编辑器：bookmark_store 的 find_bookmark 方法不存在，跳过。
    }

    fn add_edit_bookmark_block(
        &mut self,
        target: BookmarkTarget,
        label: &str,
        bookmark_store: Entity<BookmarkStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.add_edit_block(
            target.anchor,
            label,
            "Enter bookmark label (Optional)",
            Some(Box::new(move |label, _, cx| {
                bookmark_store.update(cx, |store, cx| {
                    store.edit_bookmark(&target.buffer, target.buffer_anchor, label, cx)
                });
            })),
            None,
            window,
            cx,
        );
    }

    fn add_toggle_bookmark_blocks(
        &mut self,
        targets: Vec<BookmarkTarget>,
        bookmark_store: Entity<BookmarkStore>,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        for target in targets {
            let bookmark_store = bookmark_store.clone();
            self.add_edit_block(
                target.anchor,
                "",
                "Enter bookmark label (Optional)",
                Some(Box::new(move |label: String, _, cx| {
                    bookmark_store.update(cx, |store, cx| {
                        BookmarkStore::toggle_bookmark(
                            target.buffer,
                            target.buffer_anchor,
                            label,
                            cx,
                        );
                    });
                })),
                None,
                window,
                cx,
            );
        }
    }

    fn toggle_bookmarks(
        &mut self,
        targets: Vec<BookmarkTarget>,
        label: String,
        cx: &mut Context<Self>,
    ) {
        if let Some(bookmark_store) = self.bookmark_store.clone() {
            bookmark_store.update(cx, |store, cx| {
                for target in targets {
                    BookmarkStore::toggle_bookmark(
                        target.buffer,
                        target.buffer_anchor,
                        label.clone(),
                        cx,
                    );
                }
            });
        }
    }

    pub fn go_to_next_bookmark(
        &mut self,
        _: &GoToNextBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.go_to_bookmark_impl(Direction::Next, window, cx);
    }

    pub fn go_to_previous_bookmark(
        &mut self,
        _: &GoToPreviousBookmark,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.go_to_bookmark_impl(Direction::Prev, window, cx);
    }

    fn go_to_bookmark_impl(
        &mut self,
        direction: Direction,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let Some(project) = &self.project else {
            return;
        };
        let Some(bookmark_store) = &self.bookmark_store else {
            return;
        };

        let selection = self
            .selections
            .newest::<MultiBufferOffset>(&self.display_snapshot(cx));
        let multi_buffer_snapshot = self.buffer.read(cx).snapshot(cx);

        let mut all_bookmarks = Self::bookmarks_in_range(
            MultiBufferOffset(0)..multi_buffer_snapshot.len(),
            &multi_buffer_snapshot,
            project,
            bookmark_store,
            cx,
        );
        all_bookmarks.sort_by_key(|a| a.to_offset(&multi_buffer_snapshot));

        let anchor = match direction {
            Direction::Next => all_bookmarks
                .iter()
                .find(|anchor| anchor.to_offset(&multi_buffer_snapshot) > selection.head())
                .or_else(|| all_bookmarks.first()),
            Direction::Prev => all_bookmarks
                .iter()
                .rfind(|anchor| anchor.to_offset(&multi_buffer_snapshot) < selection.head())
                .or_else(|| all_bookmarks.last()),
        }
        .cloned();

        if let Some(anchor) = anchor {
            self.unfold_ranges(&[anchor..anchor], true, false, cx);
            self.change_selections(
                SelectionEffects::scroll(Autoscroll::center()),
                window,
                cx,
                |s| {
                    s.select_anchor_ranges([anchor..anchor]);
                },
            );
        }
    }

    pub fn view_bookmarks(
        workspace: &mut Workspace,
        _: &ViewBookmarks,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) {
        let bookmark_store = workspace.project().read(cx).bookmark_store();
        cx.spawn_in(window, async move |workspace, cx| {
            let Some(locations) = BookmarkStore::all_bookmark_locations(bookmark_store, cx)
                .await
                .log_err()
            else {
                return;
            };

            workspace
                .update_in(cx, |workspace, window, cx| {
                    Editor::open_locations_in_multibuffer(
                        workspace,
                        locations,
                        "Bookmarks".into(),
                        false,
                        false,
                        MultibufferSelectionMode::First,
                        window,
                        cx,
                    );
                })
                .log_err();
        })
        .detach();
    }

    fn bookmarks_in_range(
        _range: Range<MultiBufferOffset>,
        _multi_buffer_snapshot: &MultiBufferSnapshot,
        _project: &Entity<Project>,
        _bookmark_store: &Entity<BookmarkStore>,
        _cx: &mut Context<Self>,
    ) -> Vec<Anchor> {
        // 只读编辑器：bookmark_store 的 bookmarks_for_buffer 方法不存在，返回空。
        Vec::new()
    }
}
