use std::{
    rc::Rc,
    time::{Duration, Instant},
};

use gpui::{
    AnyView, DismissEvent, Entity, EntityId, FocusHandle, ManagedView, MouseButton, Subscription,
    Task,
};
use ui::{animation::DefaultAnimations, prelude::*};
use zed_actions::toast;

use crate::Workspace;

const DEFAULT_TOAST_DURATION: Duration = Duration::from_secs(10);
const MINIMUM_RESUME_DURATION: Duration = Duration::from_millis(800);

pub fn init(cx: &mut App) {
    cx.observe_new(|workspace: &mut Workspace, _window, _cx| {
        workspace.register_action(|_workspace, _: &toast::RunAction, window, cx| {
            let workspace = cx.entity();
            let window = window.window_handle();
            cx.defer(move |cx| {
                let action = workspace
                    .read(cx)
                    .toast_layer
                    .read(cx)
                    .active_toast
                    .as_ref()
                    .and_then(|active_toast| active_toast.action.clone());

                if let Some(on_click) = action.and_then(|action| action.on_click) {
                    window
                        .update(cx, |_, window, cx| {
                            on_click(window, cx);
                        })
                        .ok();
                }
            });
        });
    })
    .detach();
}

pub trait ToastView: ManagedView {
    fn action(&self) -> Option<ToastAction>;

    /// The text announced when the toast appears.
    ///
    /// The layer cannot derive this from what the toast draws: macOS speaks a
    /// live region's own `value` and never looks at the subtree, so the text
    /// has to reach the layer as a string.
    fn announcement(&self, cx: &App) -> SharedString;

    fn auto_dismiss(&self) -> bool {
        true
    }
}

#[derive(Clone)]
pub struct ToastAction {
    pub id: ElementId,
    pub label: SharedString,
    pub on_click: Option<Rc<dyn Fn(&mut Window, &mut App) + 'static>>,
}

impl ToastAction {
    pub fn new(
        label: SharedString,
        on_click: Option<Rc<dyn Fn(&mut Window, &mut App) + 'static>>,
    ) -> Self {
        let id = ElementId::Name(label.clone());

        Self {
            id,
            label,
            on_click,
        }
    }
}

trait ToastViewHandle {
    fn view(&self) -> AnyView;
    fn announcement(&self, cx: &App) -> SharedString;
}

impl<V: ToastView> ToastViewHandle for Entity<V> {
    fn view(&self) -> AnyView {
        self.clone().into()
    }

    fn announcement(&self, cx: &App) -> SharedString {
        self.read(cx).announcement(cx)
    }
}

pub struct ActiveToast {
    id: EntityId,
    toast: Box<dyn ToastViewHandle>,
    action: Option<ToastAction>,
    _subscriptions: [Subscription; 1],
    focus_handle: FocusHandle,
}

struct DismissTimer {
    instant_started: Instant,
    _task: Task<()>,
}

pub struct ToastLayer {
    active_toast: Option<ActiveToast>,
    duration_remaining: Option<Duration>,
    dismiss_timer: Option<DismissTimer>,
}

impl Default for ToastLayer {
    fn default() -> Self {
        Self::new()
    }
}

impl ToastLayer {
    pub fn new() -> Self {
        Self {
            active_toast: None,
            duration_remaining: None,
            dismiss_timer: None,
        }
    }

    pub fn toggle_toast<V>(&mut self, cx: &mut Context<Self>, new_toast: Entity<V>)
    where
        V: ToastView,
    {
        if let Some(active_toast) = &self.active_toast {
            let show_new = active_toast.id != new_toast.entity_id();
            self.hide_toast(cx);
            if !show_new {
                return;
            }
        }
        self.show_toast(new_toast, cx);
    }

    pub fn show_toast<V>(&mut self, new_toast: Entity<V>, cx: &mut Context<Self>)
    where
        V: ToastView,
    {
        let action = new_toast.read(cx).action();
        let auto_dismiss = new_toast.read(cx).auto_dismiss();
        let focus_handle = cx.focus_handle();

        self.active_toast = Some(ActiveToast {
            _subscriptions: [cx.subscribe(&new_toast, |this, _, _: &DismissEvent, cx| {
                this.hide_toast(cx);
            })],
            id: new_toast.entity_id(),
            toast: Box::new(new_toast),
            action,
            focus_handle,
        });

        if auto_dismiss {
            self.start_dismiss_timer(DEFAULT_TOAST_DURATION, cx);
        }

        cx.notify();
    }

    pub fn hide_toast(&mut self, cx: &mut Context<Self>) {
        self.active_toast.take();
        cx.notify();
    }

    pub fn active_toast<V>(&self) -> Option<Entity<V>>
    where
        V: 'static,
    {
        let active_toast = self.active_toast.as_ref()?;
        active_toast.toast.view().downcast::<V>().ok()
    }

    pub fn has_active_toast(&self) -> bool {
        self.active_toast.is_some()
    }

    fn pause_dismiss_timer(&mut self) {
        let Some(dismiss_timer) = self.dismiss_timer.take() else {
            return;
        };
        let Some(duration_remaining) = self.duration_remaining.as_mut() else {
            return;
        };
        *duration_remaining =
            duration_remaining.saturating_sub(dismiss_timer.instant_started.elapsed());
        if *duration_remaining < MINIMUM_RESUME_DURATION {
            *duration_remaining = MINIMUM_RESUME_DURATION;
        }
    }

    /// Starts a timer to automatically dismiss the toast after the specified duration
    pub fn start_dismiss_timer(&mut self, duration: Duration, cx: &mut Context<Self>) {
        self.clear_dismiss_timer(cx);

        let instant_started = std::time::Instant::now();
        let task = cx.spawn(async move |this, cx| {
            cx.background_executor().timer(duration).await;

            if let Some(this) = this.upgrade() {
                this.update(cx, |this, cx| this.hide_toast(cx));
            }
        });

        self.duration_remaining = Some(duration);
        self.dismiss_timer = Some(DismissTimer {
            instant_started,
            _task: task,
        });
        cx.notify();
    }

    /// Restarts the dismiss timer with a new duration
    pub fn restart_dismiss_timer(&mut self, cx: &mut Context<Self>) {
        let Some(duration) = self.duration_remaining else {
            return;
        };
        self.start_dismiss_timer(duration, cx);
        cx.notify();
    }

    /// Clears the dismiss timer if one exists
    pub fn clear_dismiss_timer(&mut self, cx: &mut Context<Self>) {
        self.dismiss_timer.take();
        cx.notify();
    }
}

impl Render for ToastLayer {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // The container is rendered even with no toast in it. A live region
        // announces changes made *inside* it, so one that appears at the same
        // moment as the toast has nothing to compare against; it has to already
        // be in the tree when the toast arrives.
        let Some(active_toast) = &self.active_toast else {
            return div().child(
                div()
                    .id("toast-layer-container")
                    .role(gpui::Role::Status)
                    .aria_live(gpui::accesskit::Live::Polite),
            );
        };
        // What a reader hears when the toast arrives. The toast's own text is
        // drawn by child elements, and a live region announces its `value` and
        // nothing below it, so the text has to be lifted onto the region.
        let announcement = active_toast.toast.announcement(cx);

        div().absolute().size_full().bottom_0().left_0().child(
            v_flex()
                .id("toast-layer-container")
                // A toast is transient status the user never navigates to, so
                // it is only ever perceived if it is announced.
                .role(gpui::Role::Status)
                .aria_live(gpui::accesskit::Live::Polite)
                .aria_value(announcement)
                .absolute()
                .w_full()
                .bottom(px(0.))
                .flex()
                .flex_col()
                .items_center()
                .track_focus(&active_toast.focus_handle)
                .child(
                    // Keyed by the toast so a replacement is a new element and
                    // plays the entrance animation; the region around it stays
                    // the same node so it can announce the change.
                    h_flex()
                        .id(("active-toast-container", active_toast.id))
                        .occlude()
                        .on_hover(cx.listener(|this, hover_start, _window, cx| {
                            if *hover_start {
                                this.pause_dismiss_timer();
                            } else {
                                this.restart_dismiss_timer(cx);
                            }
                            cx.stop_propagation();
                        }))
                        .on_click(|_, _, cx| {
                            cx.stop_propagation();
                        })
                        .on_mouse_down(
                            MouseButton::Middle,
                            cx.listener(|this, _, _, cx| {
                                this.hide_toast(cx);
                            }),
                        )
                        .child(active_toast.toast.view())
                        .animate_in(AnimationDirection::FromBottom, true),
                ),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gpui::{Context, EventEmitter, Focusable, Render, TestAppContext, Window};

    struct TestToast {
        focus_handle: FocusHandle,
    }

    impl EventEmitter<DismissEvent> for TestToast {}

    impl Focusable for TestToast {
        fn focus_handle(&self, _cx: &App) -> FocusHandle {
            self.focus_handle.clone()
        }
    }

    impl Render for TestToast {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child("Project saved")
        }
    }

    impl ToastView for TestToast {
        fn action(&self) -> Option<ToastAction> {
            None
        }

        fn announcement(&self, _cx: &App) -> SharedString {
            SharedString::new_static("Project saved")
        }
    }

    /// A toast is on screen for ten seconds and is never focused, so a reader
    /// perceives it only if it is announced. macOS announces a live region's
    /// own value and never reads its subtree, so the toast drawing its text as
    /// a child is not enough on its own.
    #[gpui::test]
    async fn a_toast_announces_its_text_through_the_region(cx: &mut TestAppContext) {
        cx.update(|cx| {
            let settings_store = settings::SettingsStore::test(cx);
            cx.set_global(settings_store);
            theme::init(theme::LoadThemes::JustBase, cx);
        });

        let window = cx.add_window(|_, _| ToastLayer::new());
        cx.activate_a11y(window.into());

        let region = |cx: &mut TestAppContext| -> (String, String) {
            let json = cx
                .update_window(window.into(), |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.debug_a11y_tree_json()
                })
                .expect("the harness window is still open")
                .expect("activation makes the debug tree available");
            let tree: serde_json::Value =
                serde_json::from_str(&json).expect("the dump is valid JSON");
            gpui::a11y_checks::assert_live_regions_can_speak(&tree, "toast layer");
            let node = tree["nodes"]
                .as_object()
                .expect("the dump lists nodes")
                .values()
                .find(|node| {
                    node["element_id"]
                        .as_str()
                        .is_some_and(|id| id.contains("toast-layer-container"))
                })
                .unwrap_or_else(|| panic!("the toast region has to exist: {json}"))
                .clone();
            (
                node["aria"]["live"].as_str().unwrap_or_default().to_string(),
                node["aria"]["value"]
                    .as_str()
                    .unwrap_or_default()
                    .to_string(),
            )
        };

        // The region has to be in the tree before the toast is, or the change
        // that follows has nothing to diff against.
        assert_eq!(
            region(cx),
            ("Polite".to_string(), String::new()),
            "the region is present and silent before there is a toast"
        );

        window
            .update(cx, |toast_layer, _, cx| {
                let toast = cx.new(|cx| TestToast {
                    focus_handle: cx.focus_handle(),
                });
                toast_layer.toggle_toast(cx, toast);
            })
            .expect("the harness window is still open");

        assert_eq!(
            region(cx),
            ("Polite".to_string(), "Project saved".to_string()),
            "the toast's text has to be the region's value, which is what macOS speaks"
        );
    }
}
