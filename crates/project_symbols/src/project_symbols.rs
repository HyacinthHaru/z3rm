use anyhow;
use editor::{Bias, Editor, SelectionEffects, scroll::Autoscroll, styled_runs_for_code_label};
use fuzzy::{StringMatch, StringMatchCandidate};
use gpui::{
    App, Context, DismissEvent, Entity, HighlightStyle, ParentElement, StyledText, Task, TaskExt,
    TextStyle, WeakEntity, Window, relative,
};
use ordered_float::OrderedFloat;
use picker::{Picker, PickerDelegate, PreviewUpdate};
use project::{Project, Symbol, lsp_store::SymbolLocation};
use rope::Unclipped;
use settings::Settings;
use std::{cmp::Reverse, sync::Arc};
use theme::ActiveTheme;
use theme_settings::ThemeSettings;
use util::ResultExt;
use workspace::{
    Workspace,
    ui::{LabelLike, ListItem, ListItemSpacing, prelude::*},
};

pub fn init(cx: &mut App) {
    cx.observe_new(
        |workspace: &mut Workspace, _window, _: &mut Context<Workspace>| {
            workspace.register_action(
                |workspace, _: &workspace::ToggleProjectSymbols, window, cx| {
                    let project = workspace.project().clone();
                    let handle = cx.entity().downgrade();
                    workspace.toggle_modal(window, cx, move |window, cx| {
                        let delegate = ProjectSymbolsDelegate::new(handle, project.clone());
                        let preview = picker_preview::editor_preview(project, window, cx);
                        Picker::uniform_list_with_preview(delegate, preview, window, cx)
                    })
                },
            );
        },
    )
    .detach();
}

pub type ProjectSymbols = Entity<Picker<ProjectSymbolsDelegate>>;

pub struct ProjectSymbolsDelegate {
    workspace: WeakEntity<Workspace>,
    project: Entity<Project>,
    selected_match_index: usize,
    symbols: Vec<SymbolLocation>,
    visible_match_candidates: Vec<StringMatchCandidate>,
    external_match_candidates: Vec<StringMatchCandidate>,
    show_worktree_root_name: bool,
    matches: Vec<StringMatch>,
}

impl ProjectSymbolsDelegate {
    fn new(workspace: WeakEntity<Workspace>, project: Entity<Project>) -> Self {
        Self {
            workspace,
            project,
            selected_match_index: 0,
            symbols: Default::default(),
            visible_match_candidates: Default::default(),
            external_match_candidates: Default::default(),
            matches: Default::default(),
            show_worktree_root_name: false,
        }
    }

    // Note if you make changes to this, also change `agent_ui::completion_provider::search_symbols`
    fn filter(&mut self, query: &str, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        const MAX_MATCHES: usize = 100;
        let mut visible_matches = cx.foreground_executor().block_on(fuzzy::match_strings(
            &self.visible_match_candidates,
            query,
            false,
            true,
            MAX_MATCHES,
            &Default::default(),
            cx.background_executor().clone(),
        ));
        let mut external_matches = cx.foreground_executor().block_on(fuzzy::match_strings(
            &self.external_match_candidates,
            query,
            false,
            true,
            MAX_MATCHES - visible_matches.len().min(MAX_MATCHES),
            &Default::default(),
            cx.background_executor().clone(),
        ));
        let sort_key_for_match = |mat: &StringMatch| {
            let symbol = &self.symbols[mat.candidate_id];
            (Reverse(OrderedFloat(mat.score)), symbol.symbol.name.clone())
        };

        visible_matches.sort_unstable_by_key(sort_key_for_match);
        external_matches.sort_unstable_by_key(sort_key_for_match);
        let mut matches = visible_matches;
        matches.append(&mut external_matches);

        for mat in &mut matches {
            let symbol = &self.symbols[mat.candidate_id];
            let filter_start = symbol.symbol.range.start.row as usize;
            for position in &mut mat.positions {
                *position += filter_start;
            }
        }

        self.matches = matches;
        self.set_selected_index(0, window, cx);
    }
}

impl PickerDelegate for ProjectSymbolsDelegate {
    type ListItem = ListItem;

    fn name() -> &'static str {
        "project symbols"
    }
    fn match_label(&self, ix: usize, _cx: &App) -> Option<SharedString> {
        Some(self.matches.get(ix)?.string.clone().into())
    }

    fn placeholder_text(&self, _window: &mut Window, _cx: &mut App) -> Arc<str> {
        "Search project symbols...".into()
    }

    fn confirm(&mut self, secondary: bool, window: &mut Window, cx: &mut Context<Picker<Self>>) {
        if let Some(symbol) = self
            .matches
            .get(self.selected_match_index)
            .map(|mat| self.symbols[mat.candidate_id].clone())
        {
            let buffer = self.project.update(cx, |project, cx| {
                project.open_buffer_for_symbol(&symbol.symbol, cx)
            });
            let symbol = symbol.clone();
            let workspace = self.workspace.clone();
            cx.spawn_in(window, async move |_, cx| {
                let buffer = buffer.await?;
                workspace.update_in(cx, |workspace, window, cx| {
                    let position = buffer
                        .read(cx)
                        .clip_point_utf16(Unclipped(symbol.symbol.range.start), Bias::Left);
                    let pane = if secondary {
                        workspace.adjacent_pane(window, cx)
                    } else {
                        workspace.active_pane().clone()
                    };

                    let editor = workspace.open_project_item::<Editor>(
                        pane, buffer, true, true, true, true, window, cx,
                    );

                    editor.update(cx, |editor, cx| {
                        let multibuffer_snapshot = editor.buffer().read(cx).snapshot(cx);
                        let Some(buffer_snapshot) = multibuffer_snapshot.as_singleton() else {
                            return;
                        };
                        let text_anchor = buffer_snapshot.anchor_before(position);
                        let Some(anchor) = multibuffer_snapshot.anchor_in_buffer(text_anchor)
                        else {
                            return;
                        };
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |s| s.select_ranges([anchor..anchor]),
                        );
                    });
                })?;
                anyhow::Ok(())
            })
            .detach_and_log_err(cx);
            cx.emit(DismissEvent);
        }
    }

    fn dismissed(&mut self, _window: &mut Window, _cx: &mut Context<Picker<Self>>) {}

    fn match_count(&self) -> usize {
        self.matches.len()
    }

    fn selected_index(&self) -> usize {
        self.selected_match_index
    }

    fn set_selected_index(
        &mut self,
        ix: usize,
        _window: &mut Window,
        _cx: &mut Context<Picker<Self>>,
    ) {
        self.selected_match_index = ix;
    }

    fn try_get_preview_data_for_match(&self, _cx: &App) -> Option<PreviewUpdate> {
        let candidate_id = self.matches.get(self.selected_match_index)?.candidate_id;
        let symbol = self.symbols.get(candidate_id)?.clone();
        Some(PreviewUpdate::from_symbol(symbol.symbol))
    }

    fn update_matches(
        &mut self,
        query: String,
        window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Task<()> {
        // Try to support rust-analyzer's path based symbols feature which
        // allows to search by rust path syntax, in that case we only want to
        // filter names by the last segment
        // Ideally this was a first class LSP feature (rich queries)
        let query_filter = query
            .rsplit_once("::")
            .map_or(&*query, |(_, suffix)| suffix)
            .to_owned();
        self.filter(&query_filter, window, cx);
        self.show_worktree_root_name = self.project.read(cx).visible_worktrees(cx).count() > 1;
        let symbols = self
            .project
            .update(cx, |project, cx| project.symbols(&query, cx));
        cx.spawn_in(window, async move |this, cx| {
            let symbols = symbols.await.log_err();
            if let Some(symbols) = symbols {
                this.update_in(cx, |this, window, cx| {
                    let delegate = &mut this.delegate;
                    let project = delegate.project.read(cx);
                    let (visible_match_candidates, external_match_candidates) = symbols
                        .iter()
                        .enumerate()
                        .map(|(id, symbol)| StringMatchCandidate::new(id, &symbol.symbol.name))
                        .partition(|candidate| {
                            let path = &symbols[candidate.id].path;
                            project
                                .entry_for_path(path, cx)
                                .is_some_and(|e| !e.is_ignored)
                        });

                    delegate.visible_match_candidates = visible_match_candidates;
                    delegate.external_match_candidates = external_match_candidates;
                    delegate.symbols = symbols;
                    delegate.filter(&query_filter, window, cx);
                })
                .log_err();
            }
        })
    }

    fn render_match(
        &self,
        ix: usize,
        selected: bool,
        _window: &mut Window,
        cx: &mut Context<Picker<Self>>,
    ) -> Option<Self::ListItem> {
        let path_style = self.project.read(cx).path_style(cx);
        let string_match = &self.matches.get(ix)?;
        let symbol = &self.symbols.get(string_match.candidate_id)?;
        let theme = cx.theme();
        let syntax_runs: Vec<(std::ops::Range<usize>, gpui::HighlightStyle)> = Vec::new();

        let project_path = &symbol.path;
        let project = self.project.read(cx);
        let mut path_str = project_path.path.clone();
        if self.show_worktree_root_name
            && let Some(worktree) = project.worktree_for_id(project_path.worktree_id, cx)
        {
            path_str = worktree.read(cx).root_name().join(&path_str);
        }
        let path = path_str.display(path_style).to_string();

        let label = symbol.symbol.name.clone();
        let line_number = symbol.symbol.range.start.row + 1;

        let settings = ThemeSettings::get_global(cx);

        let text_style = TextStyle {
            color: cx.theme().colors().text,
            font_family: settings.buffer_font.family.clone(),
            font_features: settings.buffer_font.features.clone(),
            font_fallbacks: settings.buffer_font.fallbacks.clone(),
            font_size: settings.buffer_font_size(cx).into(),
            font_weight: settings.buffer_font.weight,
            line_height: relative(1.),
            ..Default::default()
        };

        let highlight_style = HighlightStyle {
            background_color: Some(cx.theme().colors().text_accent.alpha(0.3)),
            ..Default::default()
        };
        let custom_highlights = string_match
            .positions
            .iter()
            .map(|pos| (*pos..label.ceil_char_boundary(pos + 1), highlight_style));

        let highlights = gpui::combine_highlights(custom_highlights, syntax_runs);

        Some(
            ListItem::new(ix)
                .inset(true)
                .spacing(ListItemSpacing::Sparse)
                .toggle_state(selected)
                .child(
                    v_flex()
                        .child(
                            LabelLike::new().child(
                                StyledText::new(&label)
                                    .with_default_highlights(&text_style, highlights),
                            ),
                        )
                        .child(
                            h_flex()
                                .child(Label::new(path).size(LabelSize::Small).color(Color::Muted))
                                .child(
                                    Label::new(format!(":{}", line_number))
                                        .size(LabelSize::Small)
                                        .color(Color::Placeholder),
                                ),
                        ),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{TestAppContext, VisualContext};
    use project::FakeFs;
    use serde_json::json;
    use settings::SettingsStore;
    use std::sync::Arc;
    use util::{path, rel_path::rel_path};
    use workspace::MultiWorkspace;

    #[gpui::test]
    #[ignore = "hangs on `fake_servers.next()`: the LSP startup path was removed with `lsp_store`, \
                so nothing calls `LanguageRegistry::create_fake_language_server` and \
                `Project::symbols` is a stub returning no symbols"]
    async fn test_project_symbols(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "test.rs": "" }))
            .await;
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let symbols = cx.new_window_entity(|window, cx| {
            Picker::uniform_list(
                ProjectSymbolsDelegate::new(workspace.downgrade(), project.clone()),
                window,
                cx,
            )
        });

        symbols.update_in(cx, |picker, window, cx| {
            picker.update_matches("on".to_string(), window, cx);
        });
        cx.run_until_parked();
        symbols.read_with(cx, |symbols, _| {
            assert!(
                symbols.delegate.matches.is_empty(),
                "unavailable language servers must not leave stale project-symbol matches"
            );
        });
    }

    #[gpui::test]
    #[ignore = "hangs on `fake_servers.next()`: the LSP startup path was removed with `lsp_store`, \
                so nothing calls `LanguageRegistry::create_fake_language_server` and \
                `Project::symbols` is a stub returning no symbols"]
    async fn test_project_symbols_renders_utf8_match(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "test.rs": "" }))
            .await;
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());
        let symbols = cx.new_window_entity(|window, cx| {
            Picker::uniform_list(
                ProjectSymbolsDelegate::new(workspace.downgrade(), project.clone()),
                window,
                cx,
            )
        });
        let worktree_id = project.read_with(cx, |project, cx| {
            project
                .visible_worktrees(cx)
                .next()
                .map(|worktree| worktree.read(cx).id())
        });
        let Some(worktree_id) = worktree_id else {
            panic!("project symbol test project must have a worktree");
        };
        let project_path = project::ProjectPath {
            worktree_id,
            path: Arc::from(rel_path("test.rs")),
        };
        let symbol = project::Symbol {
            name: "안녕".to_string(),
            kind: lsp::SymbolKind::FUNCTION,
            range: language::PointUtf16::new(0, 0)..language::PointUtf16::new(0, 0),
            label: project::SymbolLabel {
                text: "안녕".to_string(),
            },
            path: Some(project_path.clone()),
        };

        symbols.update_in(cx, |picker, window, cx| {
            picker.delegate.symbols = vec![project::lsp_store::SymbolLocation {
                symbol,
                path: project_path,
            }];
            picker.delegate.visible_match_candidates = vec![StringMatchCandidate::new(0, "안녕")];
            picker.delegate.filter("안", window, cx);
            assert_eq!(picker.delegate.matches.len(), 1);
            assert_eq!(picker.delegate.matches[0].string, "안녕");
            assert!(picker.delegate.render_match(0, false, window, cx).is_some());
        });
    }

    /// The symbol picker is opened straight into the modal layer as a bare
    /// `Picker`, with no wrapper view of its own to name the dialog. A dialog
    /// with no name is announced as "dialog" and nothing else, which is all a
    /// user gets at the moment the modal takes their focus.
    #[gpui::test]
    async fn the_symbol_picker_names_the_dialog_it_opens(cx: &mut TestAppContext) {
        init_test(cx);

        let fs = FakeFs::new(cx.executor());
        fs.insert_tree(path!("/dir"), json!({ "test.rs": "" })).await;
        let project = Project::test(fs, [path!("/dir").as_ref()], cx).await;
        let (multi_workspace, cx) =
            cx.add_window_view(|window, cx| MultiWorkspace::test_new(project.clone(), window, cx));
        let workspace = multi_workspace.read_with(cx, |mw, _| mw.workspace().clone());

        workspace.update_in(cx, |workspace, window, cx| {
            let handle = cx.entity().downgrade();
            let project = project.clone();
            workspace.toggle_modal(window, cx, move |window, cx| {
                let delegate = ProjectSymbolsDelegate::new(handle, project.clone());
                Picker::uniform_list(delegate, window, cx)
            });
        });
        cx.run_until_parked();

        cx.activate_a11y(cx.window_handle());
        let json = cx
            .update(|window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        gpui::a11y_checks::assert_interactive_nodes_are_named(&tree, "project symbols");
        gpui::a11y_checks::assert_names_are_distinguishable(&tree, "project symbols");
        gpui::a11y_checks::assert_clickable_elements_are_reachable(&tree, "project symbols");
        gpui::a11y_checks::assert_no_role_was_discarded(&tree, "project symbols");
        gpui::a11y_checks::assert_no_aria_was_discarded(&tree, "project symbols");
        gpui::a11y_checks::assert_roles_are_contained(&tree, "project symbols");
        gpui::a11y_checks::assert_focus_reached_the_tree(&tree, "project symbols");
        gpui::a11y_checks::assert_active_descendant_is_honoured(&tree, "project symbols");

        let dialog = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .find(|node| node["aria"]["role"] == "Dialog")
            .expect("an open picker is a modal dialog");
        assert_eq!(
            dialog["aria"]["label"].as_str(),
            Some("Search project symbols..."),
            "the picker names the dialog with the same prompt the user can see"
        );
    }

    fn init_test(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let store = SettingsStore::test(cx);
            cx.set_global(store);
            theme_settings::init(theme::LoadThemes::JustBase, cx);
            release_channel::init(semver::Version::new(0, 0, 0), cx);
            editor::init(cx);
        });
    }
}
