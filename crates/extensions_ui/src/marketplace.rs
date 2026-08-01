// GPUI 扩展市场浏览器
// 来源: spec §16.11, Plan 28

use std::sync::Arc;

use anyhow::Context as _;
use extension_host::{ExtensionStore, marketplace::MarketplaceEntry};
use gpui::{
    App, Context, Entity, EventEmitter, Focusable, InteractiveElement, ParentElement, Render, Task,
    Window, actions,
};
use ui::{Button, ButtonSize, ButtonStyle, Headline, HeadlineSize, TintColor, prelude::*};
use workspace::{
    Workspace,
    item::{Item, ItemEvent},
};

actions!(
    marketplace,
    [OpenMarketplace, SearchExtensions, InstallFromMarketplace,]
);

/// 市场扩展条目 (包含安装状态)
#[derive(Clone, Debug)]
pub struct MarketplaceExtension {
    pub entry: MarketplaceEntry,
    pub installed: bool,
    pub installed_version: Arc<str>,
}

/// GPUI 市场浏览器页面
pub struct MarketplaceBrowser {
    _workspace: gpui::WeakEntity<Workspace>,
    focus_handle: gpui::FocusHandle,
    entries: Vec<MarketplaceExtension>,
    filtered_indices: Vec<usize>,
    is_fetching: bool,
    _subscriptions: [gpui::Subscription; 1],
}

impl MarketplaceBrowser {
    pub fn new(
        workspace: &Workspace,
        window: &mut Window,
        cx: &mut Context<Workspace>,
    ) -> Entity<Self> {
        cx.new(|cx| {
            let store = ExtensionStore::global(cx);
            let subscriptions = [cx.subscribe_in(
                &store,
                window,
                move |this: &mut Self, _, event, _window, cx| {
                    if let extension_host::Event::ExtensionsUpdated
                    | extension_host::Event::ExtensionInstalled(_) = event
                    {
                        this.refresh_extensions(cx);
                    }
                },
            )];

            let mut this = Self {
                _workspace: workspace.weak_handle(),
                focus_handle: cx.focus_handle(),
                entries: Vec::new(),
                filtered_indices: Vec::new(),
                is_fetching: false,
                _subscriptions: subscriptions,
            };
            this.refresh_extensions(cx);
            this
        })
    }

    fn refresh_extensions(&mut self, cx: &mut Context<Self>) {
        self.is_fetching = true;
        cx.notify();

        let store = ExtensionStore::global(cx);
        let installed_map = store.read(cx).extension_index.extensions.clone();
        let http_client = store.read(cx).http_client.clone();

        cx.spawn(async move |this, cx| {
            let registry = extension_host::marketplace::fetch_registry(
                &**http_client,
                "https://extensions.z3rm.dev/registry.json",
            )
            .await
            .context("failed to fetch marketplace registry");

            let entries = match registry {
                Ok(registry) => registry
                    .entries
                    .iter()
                    .map(|entry| {
                        let installed = installed_map.contains_key(entry.id.as_str());
                        let installed_version = installed_map
                            .get(entry.id.as_str())
                            .map(|e| e.manifest.version.clone())
                            .unwrap_or_else(|| Arc::from("not installed".to_string()));

                        MarketplaceExtension {
                            entry: entry.clone(),
                            installed,
                            installed_version,
                        }
                    })
                    .collect::<Vec<_>>(),
                Err(_) => Vec::new(),
            };

            this.update(cx, |this, cx| {
                this.entries = entries;
                this.filtered_indices = (0..this.entries.len()).collect();
                this.is_fetching = false;
                cx.notify();
            })
        })
        .detach_and_log_err(cx);
    }

    fn install_extension(&mut self, entry: &MarketplaceEntry, cx: &mut Context<Self>) {
        let store = ExtensionStore::global(cx);
        store.update(cx, |store, cx| {
            store.install_extension(
                Arc::from(entry.id.clone()),
                Arc::from(entry.version.to_string()),
                cx,
            );
        });
    }

    fn uninstall_extension(&mut self, extension_id: &str, cx: &mut Context<Self>) {
        let store = ExtensionStore::global(cx);
        store.update(cx, |store, cx| {
            store.uninstall_extension(Arc::from(extension_id), cx);
        });
    }
}

impl Render for MarketplaceBrowser {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl gpui::IntoElement {
        let theme = cx.theme();

        v_flex()
            .key_context("MarketplaceBrowser")
            .size_full()
            .bg(theme.colors().editor_background)
            .child(
                v_flex()
                    .gap_4()
                    .pt_4()
                    .px_4()
                    .bg(theme.colors().editor_background)
                    .child(Headline::new("Extension Marketplace").size(HeadlineSize::Large)),
            )
            .child(if self.is_fetching {
                div()
                    .py_4()
                    .child("Loading marketplace extensions...")
                    .into_any_element()
            } else if self.entries.is_empty() {
                div()
                    .py_4()
                    .child("Failed to load marketplace extensions")
                    .into_any_element()
            } else {
                v_flex()
                    .size_full()
                    .gap_2()
                    .px_4()
                    .overflow_y_hidden()
                    .children(self.filtered_indices.iter().map(|&idx| {
                        let entry = self.entries[idx].clone();
                        let installed = entry.installed;

                        div()
                            .border_1()
                            .rounded_md()
                            .p_3()
                            .border_color(theme.colors().border)
                            .child(
                                h_flex()
                                    .w_full()
                                    .justify_between()
                                    .items_center()
                                    .child(
                                        v_flex()
                                            .gap_1()
                                            .child(
                                                Headline::new(&entry.entry.name)
                                                    .size(HeadlineSize::Small),
                                            )
                                            .child(
                                                format!(
                                                    "{} · v{} · by {}",
                                                    entry.entry.id,
                                                    entry.entry.version,
                                                    entry.entry.author
                                                )
                                                .into_element(),
                                            )
                                            .child(entry.entry.description.clone().into_element()),
                                    )
                                    .child(if installed {
                                        let id = entry.entry.id.clone();
                                        Button::new(format!("uninstall-{id}"), "Uninstall")
                                            .style(ButtonStyle::Outlined)
                                            .size(ButtonSize::Medium)
                                            .on_click(cx.listener(move |this, _, _, cx| {
                                                this.uninstall_extension(&id, cx);
                                            }))
                                            .into_any_element()
                                    } else {
                                        let entry_clone = entry.entry.clone();
                                        Button::new(
                                            format!("install-{}", entry_clone.id),
                                            "Install",
                                        )
                                        .style(ButtonStyle::Tinted(TintColor::Accent))
                                        .size(ButtonSize::Medium)
                                        .on_click(cx.listener(move |this, _, _, cx| {
                                            this.install_extension(&entry_clone, cx);
                                        }))
                                        .into_any_element()
                                    }),
                            )
                            .into_any_element()
                    }))
                    .into_any_element()
            })
            .child(div().px_4().py_2().child(format!(
                "{} extension{} available",
                self.filtered_indices.len(),
                if self.filtered_indices.len() != 1 {
                    "s"
                } else {
                    ""
                }
            )))
    }
}

impl EventEmitter<ItemEvent> for MarketplaceBrowser {}

impl Focusable for MarketplaceBrowser {
    fn focus_handle(&self, _cx: &App) -> gpui::FocusHandle {
        self.focus_handle.clone()
    }
}

impl Item for MarketplaceBrowser {
    type Event = ItemEvent;

    fn tab_content_text(&self, _detail: usize, _cx: &App) -> gpui::SharedString {
        "Extension Marketplace".into()
    }

    fn to_item_events(event: &Self::Event, f: &mut dyn FnMut(ItemEvent)) {
        f(*event);
    }
}

pub fn init(cx: &mut App) {
    cx.observe_new(move |workspace: &mut Workspace, window, _cx| {
        let Some(window) = window else {
            return;
        };

        workspace.register_action(move |workspace, _: &OpenMarketplace, window, cx| {
            let existing = workspace
                .active_pane()
                .read(cx)
                .items()
                .find_map(|item| item.downcast::<MarketplaceBrowser>());

            if let Some(existing) = existing {
                workspace.activate_item(&existing, true, true, window, cx);
            } else {
                let browser = MarketplaceBrowser::new(workspace, window, cx);
                workspace.add_item_to_active_pane(Box::new(browser), None, true, window, cx);
            }
        });
    })
    .detach();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_marketplace_entry_filtering() {
        let entries = vec![
            MarketplaceExtension {
                entry: MarketplaceEntry {
                    id: "rust".into(),
                    name: "Rust".into(),
                    version: semver::Version::new(1, 0, 0),
                    description: "Rust language support".into(),
                    author: "z3rm".into(),
                    repository: None,
                    download_url: "https://example.com/rust.tar.gz".into(),
                    checksum: "abc".into(),
                },
                installed: true,
                installed_version: Arc::from("1.0.0".to_string()),
            },
            MarketplaceExtension {
                entry: MarketplaceEntry {
                    id: "python".into(),
                    name: "Python".into(),
                    version: semver::Version::new(2, 0, 0),
                    description: "Python language support".into(),
                    author: "z3rm".into(),
                    repository: None,
                    download_url: "https://example.com/python.tar.gz".into(),
                    checksum: "def".into(),
                },
                installed: false,
                installed_version: Arc::from("not installed".to_string()),
            },
        ];

        let query = "rust".to_lowercase();
        let filtered: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.entry.name.to_lowercase().contains(&query)
                    || entry.entry.description.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect();

        assert_eq!(filtered, vec![0]);

        let query = "language".to_lowercase();
        let filtered: Vec<usize> = entries
            .iter()
            .enumerate()
            .filter(|(_, entry)| {
                entry.entry.name.to_lowercase().contains(&query)
                    || entry.entry.description.to_lowercase().contains(&query)
            })
            .map(|(i, _)| i)
            .collect();

        assert_eq!(filtered, vec![0, 1]);
    }
}
