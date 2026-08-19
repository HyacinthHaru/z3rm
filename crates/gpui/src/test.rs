//! Test support for GPUI.
//!
//! GPUI provides first-class support for testing, which includes a macro to run test that rely on having a context,
//! and a test implementation of the `ForegroundExecutor` and `BackgroundExecutor` which ensure that your tests run
//! deterministically even in the face of arbitrary parallelism.
//!
//! The output of the `gpui::test` macro is understood by other rust test runners, so you can use it with `cargo test`
//! or `cargo-nextest`, or another runner of your choice.
//!
//! To make it possible to test collaborative user interfaces (like Zed) you can ask for as many different contexts
//! as you need.
//!
//! ## Example
//!
//! ```
//! use gpui;
//!
//! #[gpui::test]
//! async fn test_example(cx: &TestAppContext) {
//!   assert!(true)
//! }
//!
//! #[gpui::test]
//! async fn test_collaboration_example(cx_a: &TestAppContext, cx_b: &TestAppContext) {
//!   assert!(true)
//! }
//! ```
use crate::{Entity, Subscription, TestAppContext, TestDispatcher};
use futures::StreamExt as _;
use proptest::prelude::{Just, Strategy, any};
use std::{
    env,
    panic::{self, RefUnwindSafe, UnwindSafe},
    pin::Pin,
};

/// Strategy injected into `#[gpui::property_test]` tests to control the seed
/// given to the scheduler. Doesn't shrink, since all scheduler seeds are
/// equivalent in complexity. If `$SEED` is set, it always uses that value.
///
/// Note: this function is not intended to be used directly. Rather, it is
/// public so that it can be used from the `property_test` macro.
pub fn seed_strategy() -> impl Strategy<Value = u64> {
    match std::env::var("SEED") {
        Ok(val) => Just(val.parse().unwrap()).boxed(),
        Err(_) => any::<u64>().no_shrink().boxed(),
    }
}

/// Applies a fixed RNG seed to a proptest config so that case generation
/// is deterministic. Uses `$SEED` if set, otherwise defaults to `0`.
/// This bridges the GPUI `SEED` env var to proptest's RNG seed, so that
/// a single variable controls both the scheduler seed and case generation.
///
/// Note: this function is not intended to be used directly. Rather, it is
/// public so that it can be used from the `property_test` macro.
pub fn apply_seed_to_proptest_config(
    mut config: proptest::test_runner::Config,
) -> proptest::test_runner::Config {
    let seed = env::var("SEED")
        .ok()
        .and_then(|val| val.parse::<u64>().ok())
        .unwrap_or(0);
    config.rng_seed = proptest::test_runner::RngSeed::Fixed(seed);
    config
}

/// Similar to [`run_test`], but only runs the callback once, allowing
/// [`FnOnce`] callbacks. This is intended for use with the
/// `gpui::property_test` macro and generally should not be used directly.
///
/// Doesn't support many features of [`run_test`], since these are provided by
/// proptest.
pub fn run_test_once<R>(
    seed: u64,
    test_fn: Box<dyn UnwindSafe + FnOnce(TestDispatcher) -> R>,
) -> R {
    let result = panic::catch_unwind(|| {
        let dispatcher = TestDispatcher::new(seed);
        let scheduler = dispatcher.scheduler().clone();
        let res = test_fn(dispatcher);
        scheduler.end_test();
        res
    });

    match result {
        Ok(r) => r,
        Err(e) => panic::resume_unwind(e),
    }
}

/// Run the given test function with the configured parameters.
/// This is intended for use with the `gpui::test` macro
/// and generally should not be used directly.
pub fn run_test(
    num_iterations: usize,
    explicit_seeds: &[u64],
    max_retries: usize,
    test_fn: &mut (dyn RefUnwindSafe + Fn(TestDispatcher, u64)),
    on_fail_fn: Option<fn()>,
) {
    let (seeds, is_multiple_runs) = calculate_seeds(num_iterations as u64, explicit_seeds);

    for seed in seeds {
        let mut attempt = 0;
        loop {
            if is_multiple_runs {
                eprintln!("seed = {seed}");
            }
            let result = panic::catch_unwind(|| {
                let dispatcher = TestDispatcher::new(seed);
                let scheduler = dispatcher.scheduler().clone();
                test_fn(dispatcher, seed);
                scheduler.end_test();
            });

            match result {
                Ok(_) => break,
                Err(error) => {
                    if attempt < max_retries {
                        println!("attempt {} failed, retrying", attempt);
                        attempt += 1;
                        // The panic payload might itself trigger an unwind on drop:
                        // https://doc.rust-lang.org/std/panic/fn.catch_unwind.html#notes
                        std::mem::forget(error);
                    } else {
                        if is_multiple_runs {
                            eprintln!("failing seed: {seed}");
                            eprintln!(
                                "You can rerun from this seed by setting the environmental variable SEED to {seed}"
                            );
                        }
                        if let Some(on_fail_fn) = on_fail_fn {
                            on_fail_fn()
                        }
                        panic::resume_unwind(error);
                    }
                }
            }
        }
    }
}

fn calculate_seeds(
    iterations: u64,
    explicit_seeds: &[u64],
) -> (impl Iterator<Item = u64> + '_, bool) {
    let iterations = env::var("ITERATIONS")
        .ok()
        .map(|var| var.parse().expect("invalid ITERATIONS variable"))
        .unwrap_or(iterations);

    let env_num = env::var("SEED")
        .map(|seed| seed.parse().expect("invalid SEED variable as integer"))
        .ok();

    let empty_range = || 0..0;

    let iter = {
        let env_range = if let Some(env_num) = env_num {
            env_num..env_num + 1
        } else {
            empty_range()
        };

        // if `iterations` is 1 and !(`explicit_seeds` is non-empty || `SEED` is set), then add     the run `0`
        // if `iterations` is 1 and  (`explicit_seeds` is non-empty || `SEED` is set), then discard the run `0`
        // if `iterations` isn't 1 and `SEED` is set, do `SEED..SEED+iterations`
        // otherwise, do `0..iterations`
        let iterations_range = match (iterations, env_num) {
            (1, None) if explicit_seeds.is_empty() => 0..1,
            (1, None) | (1, Some(_)) => empty_range(),
            (iterations, Some(env)) => env..env + iterations,
            (iterations, None) => 0..iterations,
        };

        // if `SEED` is set, ignore `explicit_seeds`
        let explicit_seeds = if env_num.is_some() {
            &[]
        } else {
            explicit_seeds
        };

        env_range
            .chain(iterations_range)
            .chain(explicit_seeds.iter().copied())
    };
    let is_multiple_runs = iter.clone().nth(1).is_some();
    (iter, is_multiple_runs)
}

/// A test struct for converting an observation callback into a stream.
pub struct Observation<T> {
    rx: Pin<Box<async_channel::Receiver<T>>>,
    _subscription: Subscription,
}

impl<T: 'static> futures::Stream for Observation<T> {
    type Item = T;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        self.rx.poll_next_unpin(cx)
    }
}

/// observe returns a stream of the change events from the given `Entity`
pub fn observe<T: 'static>(entity: &Entity<T>, cx: &mut TestAppContext) -> Observation<()> {
    let (tx, rx) = async_channel::unbounded();
    let _subscription = cx.update(|cx| {
        cx.observe(entity, move |_, _| {
            let _ = gpui::block_on(tx.send(()));
        })
    });
    let rx = Box::pin(rx);

    Observation { rx, _subscription }
}

/// Assertions over a [`crate::Window::debug_a11y_tree_json`] dump.
///
/// These live here rather than in each crate's tests because the defects they
/// catch are properties of the dump, not of any one screen: a role with no
/// name, a role that never became a node, a role outside the container that
/// gives it meaning. Five copies of the same role list had already started to
/// drift apart.
pub mod a11y_checks {
    /// Roles whose whole purpose is to be told apart from their siblings. A
    /// node with one of these and no name is announced as a bare "button" or
    /// "tree item".
    pub const ROLES_NEEDING_A_NAME: &[&str] = &[
        // A dialog is announced the moment it opens, before the user can
        // explore it, so an unnamed one is "dialog" and nothing else.
        "AlertDialog",
        "Button",
        "CheckBox",
        "Dialog",
        "ComboBox",
        "Link",
        "ListBoxOption",
        "MenuItem",
        "MenuItemCheckBox",
        "RadioButton",
        "SpinButton",
        "Switch",
        // A text input usually names itself with its placeholder; one with
        // neither a placeholder nor a label is announced as "edit text" and
        // nothing else, which says nothing about what to type into it.
        "TextInput",
        "Tab",
        "TreeItem",
        // Containers a reader is offered as a destination. A window with two
        // of the same kind — pinned tabs beside unpinned ones, a project tree
        // beside a session tree — offers them a choice it will not explain.
        "TabList",
        "Tree",
        "Table",
        // A password field announced as "secure text field" and nothing else
        // gives no clue what it is guarding.
        "PasswordInput",
    ];

    /// Roles that only mean anything inside a matching container. A screen
    /// reader derives "tab 2 of 5" and the arrow-key conventions from that
    /// containment, so an orphaned option or tab loses all of it.
    pub const ROLE_REQUIRES_CONTAINER: &[(&str, &str)] = &[
        ("ListBoxOption", "ListBox"),
        // A row outside a table is a row of nothing: no column headers to
        // relate its cells to, and no "row 3 of 40" to place it in.
        ("Row", "Table"),
        ("MenuItem", "Menu"),
        ("MenuItemCheckBox", "Menu"),
        ("Tab", "TabList"),
        ("TreeItem", "Tree"),
    ];

    fn nodes(tree: &serde_json::Value) -> &serde_json::Map<String, serde_json::Value> {
        tree["nodes"]
            .as_object()
            .expect("an a11y dump lists its nodes")
    }

    /// Panics if any node with an interactive role has nothing to announce.
    #[track_caller]
    pub fn assert_interactive_nodes_are_named(tree: &serde_json::Value, context: &str) {
        let unnamed: Vec<String> = nodes(tree)
            .values()
            .filter(|node| {
                node["aria"]["role"]
                    .as_str()
                    .is_some_and(|role| ROLES_NEEDING_A_NAME.contains(&role))
            })
            .filter(|node| {
                ["label", "value", "placeholder"]
                    .iter()
                    .all(|field| node["aria"][field].as_str().is_none_or(str::is_empty))
            })
            .map(|node| format!("{} ({})", node["aria"]["role"], node["element_id"]))
            .collect();
        assert!(
            unnamed.is_empty(),
            "{context}: these nodes are announced as a bare role: {unnamed:?}"
        );
    }

    /// Panics if a role was set on an element with no id, which produces no
    /// node at all — no warning, and no difference in the code that asked.
    #[track_caller]
    pub fn assert_no_role_was_discarded(tree: &serde_json::Value, context: &str) {
        let discarded = tree
            .get("frame")
            .and_then(|frame| frame.get("roles_without_id"))
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert!(
            discarded.is_empty(),
            "{context}: these roles never became nodes for lack of an element id: {discarded:?}"
        );
    }

    /// Panics if a live region carries content that its platform cannot
    /// announce.
    ///
    /// This is the one check here that is not about how a tree reads. It is
    /// about whether a change is spoken at all, and no two platforms agree on
    /// where the text comes from:
    ///
    /// * `accesskit_macos` raises an announcement only when `value().is_some()`
    ///   and speaks `node.value()`.
    /// * `accesskit_windows` raises `UIA_LiveRegionChangedEventId` only when
    ///   `name().is_some()`, and `name()` is the label for every role but
    ///   `Role::Label`.
    /// * `accesskit_atspi_common` emits `ObjectEvent::Announcement` with
    ///   `wrapper.name()`, on name change — same as Windows.
    ///
    /// So the announced text has to be in **both** the label and the value.
    /// Either one alone is silence on two platforms or one. `value()` falls
    /// back to a single-line text input's contents and to nothing else, and
    /// neither field is ever derived from the role, so nothing supplies the
    /// missing one.
    ///
    /// A live region with nothing in it is not a defect but the required
    /// shape: a region has to be in the tree before its content arrives, or
    /// there is no change for the reader to notice. Both fields being absent
    /// is how a region says it has nothing to announce yet.
    #[track_caller]
    pub fn assert_live_regions_can_speak(tree: &serde_json::Value, context: &str) {
        let nodes = nodes(tree);
        fn text_of<'a>(node: &'a serde_json::Value, field: &str) -> Option<&'a str> {
            node["aria"][field].as_str().filter(|text| !text.is_empty())
        }
        fn names_something(
            nodes: &serde_json::Map<String, serde_json::Value>,
            node: &serde_json::Value,
            depth: usize,
        ) -> bool {
            // Cycles are impossible in a tree, but a malformed dump should
            // fail the assertion rather than the stack.
            if depth > 64 {
                return false;
            }
            node["children"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|id| nodes.get(id.as_str()?))
                .any(|child| {
                    text_of(child, "label").is_some()
                        || text_of(child, "value").is_some()
                        || names_something(nodes, child, depth + 1)
                })
        }

        let mute: Vec<String> = nodes
            .values()
            .filter(|node| {
                node["aria"]["live"]
                    .as_str()
                    .is_some_and(|live| live != "Off")
            })
            .filter_map(|node| {
                let label = text_of(node, "label");
                let value = text_of(node, "value");
                let complaint = match (label, value) {
                    (Some(_), Some(_)) => return None,
                    // Nothing to announce yet, which is the shape a region has
                    // to be in before its content arrives.
                    (None, None) if !names_something(nodes, node, 0) => return None,
                    (None, None) => "its content is in child nodes, which no platform announces",
                    (Some(_), None) => "no value, so macOS announces nothing",
                    (None, Some(_)) => "no label, so Windows and Linux announce nothing",
                };
                Some(format!("{} ({complaint})", node["element_id"]))
            })
            .collect();
        assert!(
            mute.is_empty(),
            "{context}: these live regions cannot be announced: {mute:?}"
        );
    }

    /// Panics if an element carries accessibility information but no role, so
    /// no node was built for it and the information went nowhere.
    ///
    /// The mirror image of [`assert_no_role_was_discarded`]: a node needs both
    /// an id and a role, and neither half fails loudly on its own. A label, a
    /// live region or a placeholder set on an element with no role is dropped
    /// in silence, and the call site looks exactly like one that worked.
    #[track_caller]
    pub fn assert_no_aria_was_discarded(tree: &serde_json::Value, context: &str) {
        let discarded = tree
            .get("frame")
            .and_then(|frame| frame.get("aria_without_role"))
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default();
        assert!(
            discarded.is_empty(),
            "{context}: these elements carry accessibility information that \
             reached no node, for lack of a role: {discarded:?}"
        );
    }

    /// Panics if two places the keyboard can land carry the same role and name.
    ///
    /// The same argument [`assert_landmarks_are_distinguishable`] makes for
    /// landmarks, applied to everywhere focus can go. A focusable node is a
    /// destination, and a reader announces it by name on arrival; two of them
    /// sharing a name describe one destination where there are two, so the
    /// user tabs, hears the same words, and cannot tell whether they moved.
    ///
    /// Narrower than [`assert_names_are_distinguishable`] in what it looks at
    /// and wider in which roles: that check is about controls a user asks for
    /// by name, this one is about containers and surfaces they arrive at —
    /// two editors both called "Editor", two panes both called "Terminal".
    #[track_caller]
    pub fn assert_focusable_names_are_distinguishable(tree: &serde_json::Value, context: &str) {
        let nodes = nodes(tree);
        let mut parent_of: collections::FxHashMap<&str, &str> = collections::FxHashMap::default();
        for (id, node) in nodes {
            for child in node["children"].as_array().into_iter().flatten() {
                if let Some(child) = child.as_str() {
                    parent_of.insert(child, id.as_str());
                }
            }
        }

        let mut seen: collections::FxHashMap<(&str, &str, &str), Vec<&str>> =
            collections::FxHashMap::default();
        for (id, node) in nodes {
            let focusable = node["aria"]["on_action"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|action| action == "Focus");
            if !focusable {
                continue;
            }
            let Some(name) = node["aria"]["label"].as_str().filter(|name| !name.is_empty()) else {
                continue;
            };
            let role = node["aria"]["role"].as_str().unwrap_or_default();
            // Rows are told apart by the branch they hang from, which a reader
            // announces with them, so they are only ambiguous among siblings.
            // Two files called `main.rs` in different folders are not a defect.
            let scope = if matches!(role, "TreeItem" | "Row" | "ListBoxOption" | "ListItem") {
                parent_of.get(id.as_str()).copied().unwrap_or_default()
            } else {
                ""
            };
            seen.entry((role, name, scope)).or_default().push(id.as_str());
        }

        let is_ancestor = |ancestor: &str, node: &str| {
            let mut node = node;
            // Bounded by the tree's depth; a malformed dump should fail the
            // assertion rather than loop.
            for _ in 0..64 {
                match parent_of.get(node) {
                    Some(parent) if *parent == ancestor => return true,
                    Some(parent) => node = *parent,
                    None => return false,
                }
            }
            false
        };

        let mut clashes: Vec<String> = seen
            .into_iter()
            .filter_map(|((role, name, _), ids)| {
                // A node nested inside another of the same name is redundant
                // rather than ambiguous: you cannot be in one without being in
                // the other, so there are not two destinations to confuse. What
                // this check is about is the ones you tab between.
                let siblings: Vec<&str> = ids
                    .iter()
                    .copied()
                    .filter(|id| !ids.iter().any(|other| *other != *id && is_ancestor(other, id)))
                    .collect();
                (siblings.len() > 1)
                    .then(|| format!("{}x {role} named {name:?}", siblings.len()))
            })
            .collect();
        clashes.sort_unstable();
        assert!(
            clashes.is_empty(),
            "{context}: the keyboard lands on these, and they say the same thing: {clashes:?}"
        );
    }

    /// Landmark roles a reader offers as a way to jump around the window. More
    /// than one of the same landmark is only useful if they can be told apart.
    pub const LANDMARK_ROLES: &[&str] = &["Main", "Complementary", "Navigation", "Banner"];

    /// Panics if a window has more than one `Main`, or two landmarks of the
    /// same kind that a reader cannot tell apart.
    ///
    /// A landmark list reading "complementary, complementary" is worse than no
    /// landmarks: it offers destinations and refuses to say what they are.
    #[track_caller]
    pub fn assert_landmarks_are_distinguishable(tree: &serde_json::Value, context: &str) {
        let mut by_role: collections::FxHashMap<&str, Vec<&str>> = collections::FxHashMap::default();
        for node in nodes(tree).values() {
            let Some(role) = node["aria"]["role"].as_str() else {
                continue;
            };
            if !LANDMARK_ROLES.contains(&role) {
                continue;
            }
            by_role
                .entry(role)
                .or_default()
                .push(node["aria"]["label"].as_str().unwrap_or_default());
        }

        if let Some(mains) = by_role.get("Main") {
            assert!(
                mains.len() <= 1,
                "{context}: a window has one main region, found {}: {mains:?}",
                mains.len()
            );
        }

        for (role, mut names) in by_role {
            let total = names.len();
            names.sort_unstable();
            names.dedup();
            assert_eq!(
                names.len(),
                total,
                "{context}: {total} {role} landmarks that cannot be told apart: {names:?}"
            );
        }
    }

    /// Panics if two controls in the same container share a role and a name.
    ///
    /// A name that is present satisfies [`assert_interactive_nodes_are_named`]
    /// without being usable: a row of tabs whose close buttons all announce
    /// "Close Tab" gives a user no way to say which one they mean, and no way
    /// to tell where they are as they move along the row.
    #[track_caller]
    pub fn assert_names_are_distinguishable(tree: &serde_json::Value, context: &str) {
        // Across the whole frame rather than among direct siblings: the close
        // button of each tab is a child of its own tab, so a per-parent check
        // sees one of each and misses the row of identical "Close Tab"s that
        // the user actually hears.
        //
        // Tree rows are the exception. Two files called `main.rs` in different
        // folders are told apart by the branch they hang from, which a reader
        // announces along with the row, so they are only ambiguous when they
        // share a parent.
        let nodes = nodes(tree);
        let mut parent_of: collections::FxHashMap<&str, &str> = collections::FxHashMap::default();
        for (id, node) in nodes {
            for child in node["children"].as_array().into_iter().flatten() {
                if let Some(child) = child.as_str() {
                    parent_of.insert(child, id.as_str());
                }
            }
        }
        let mut seen: collections::FxHashMap<(&str, &str, &str), usize> =
            collections::FxHashMap::default();
        for (id, node) in nodes {
            let Some(role) = node["aria"]["role"].as_str() else {
                continue;
            };
            if !ROLES_NEEDING_A_NAME.contains(&role) {
                continue;
            }
            let name = node["aria"]["label"].as_str().unwrap_or_default();
            let scope = if role == "TreeItem" {
                parent_of.get(id.as_str()).copied().unwrap_or_default()
            } else {
                ""
            };
            *seen.entry((role, name, scope)).or_default() += 1;
        }
        let mut clashes: Vec<String> = seen
            .into_iter()
            .filter(|(_, count)| *count > 1)
            .map(|((role, name, _), count)| format!("{count} × {role} named {name:?}"))
            .collect();
        clashes.sort_unstable();
        assert!(
            clashes.is_empty(),
            "{context}: controls a user cannot tell apart or ask for by name: {clashes:?}"
        );
    }

    /// Panics if a container claimed an active descendant that was dropped.
    ///
    /// GPUI honours [`StatefulInteractiveElement::aria_active_descendant`] only
    /// while the focused node is one of the claiming node's ancestors, because
    /// it reports the descendant *as* the focus. A list filtered from a
    /// separate input cannot use it — focus is in the input, which is not an
    /// ancestor of the rows — and the claim is dropped without a word, leaving
    /// the highlighted row announced to nobody.
    #[track_caller]
    pub fn assert_active_descendant_is_honoured(tree: &serde_json::Value, context: &str) {
        let dropped = tree
            .get("frame")
            .and_then(|frame| frame.get("active_descendant_without_focus"))
            .and_then(serde_json::Value::as_bool)
            .unwrap_or(false);
        assert!(
            !dropped,
            "{context}: a container claimed the row it highlights while focus was outside it, \
             so the claim was dropped and the row announced to nobody"
        );
    }

    /// Panics if a control is announced with no area to click.
    ///
    /// GPUI answers an incoming `Click` by synthesizing a mouse press at the
    /// centre of the node's bounds. A node with no width or no height has no
    /// centre to press, so it reads as an operable control and does nothing
    /// when operated — the same dead end as a control with no name, reached a
    /// different way.
    #[track_caller]
    pub fn assert_controls_have_area(tree: &serde_json::Value, context: &str) {
        let mut empty = Vec::new();
        for node in nodes(tree).values() {
            let Some(role) = node["aria"]["role"].as_str() else {
                continue;
            };
            if !ROLES_NEEDING_A_NAME.contains(&role) {
                continue;
            }
            let bounds = &node["bounds"];
            let (Some(x0), Some(y0), Some(x1), Some(y1)) = (
                bounds["x0"].as_f64(),
                bounds["y0"].as_f64(),
                bounds["x1"].as_f64(),
                bounds["y1"].as_f64(),
            ) else {
                continue;
            };
            if x1 - x0 <= 0.0 || y1 - y0 <= 0.0 {
                empty.push(format!(
                    "{role} named {:?}",
                    node["aria"]["label"].as_str().unwrap_or_default()
                ));
            }
        }
        empty.sort_unstable();
        assert!(
            empty.is_empty(),
            "{context}: controls announced with nothing to click: {empty:?}"
        );
    }

    /// Panics if an element that answers a click contains no node at all.
    ///
    /// An id and a click handler are not enough — a node needs a role. The
    /// common shape is harmless: a clickable wrapper whose child carries the
    /// role, so assistive technology still finds something to operate at that
    /// spot. What is not harmless is a clickable element with nothing inside
    /// it that became a node, because then the action it offers is reachable
    /// by mouse and by nothing else — and no other check can see it, since
    /// they all reason about nodes and this one has none.
    #[track_caller]
    pub fn assert_clickable_elements_are_reachable(tree: &serde_json::Value, context: &str) {
        let node_rects: Vec<(f64, f64, f64, f64)> = nodes(tree)
            .values()
            .filter(|node| node["aria"]["role"].as_str().is_some())
            .filter_map(|node| {
                let bounds = &node["bounds"];
                Some((
                    bounds["x0"].as_f64()?,
                    bounds["y0"].as_f64()?,
                    bounds["x1"].as_f64()?,
                    bounds["y1"].as_f64()?,
                ))
            })
            .collect();

        let mut unreachable = Vec::new();
        for entry in tree
            .get("frame")
            .and_then(|frame| frame.get("clickable_without_role"))
            .and_then(serde_json::Value::as_array)
            .map(Vec::as_slice)
            .unwrap_or_default()
        {
            let bounds = &entry["bounds"];
            let (Some(x0), Some(y0), Some(x1), Some(y1)) = (
                bounds["x0"].as_f64(),
                bounds["y0"].as_f64(),
                bounds["x1"].as_f64(),
                bounds["y1"].as_f64(),
            ) else {
                continue;
            };
            // An empty rectangle cannot contain anything, and a control with no
            // area is already `assert_controls_have_area`'s business.
            if x1 - x0 <= 0.0 || y1 - y0 <= 0.0 {
                continue;
            }
            let contains_a_node = node_rects
                .iter()
                .any(|(nx0, ny0, nx1, ny1)| {
                    *nx0 >= x0 && *ny0 >= y0 && *nx1 <= x1 && *ny1 <= y1
                });
            if !contains_a_node {
                unreachable.push(
                    entry["source_location"]
                        .as_str()
                        .unwrap_or_default()
                        .to_string(),
                );
            }
        }
        unreachable.sort_unstable();
        unreachable.dedup();
        assert!(
            unreachable.is_empty(),
            "{context}: these elements answer a click and contain no node at all, so \
             the action they offer is reachable by mouse only: {unreachable:?}"
        );
    }

    /// Panics if the focused element produced no node.
    ///
    /// A focused element with an id but no role produces no accessibility node,
    /// so focus has nowhere to land and screen readers fall back to announcing
    /// the whole window. GPUI records why when that happens; the dump prints it.
    #[track_caller]
    pub fn assert_focus_reached_the_tree(tree: &serde_json::Value, context: &str) {
        let dropped = tree
            .get("frame")
            .and_then(|frame| frame.get("focus_without_node"))
            .and_then(|reason| reason.as_str());
        assert!(
            dropped.is_none(),
            "{context}: the focused element produced no accessibility node ({}), so assistive \
             technology announces the whole window instead of it",
            dropped.unwrap_or_default()
        );
    }

    /// Panics if a control that advertises `Click` would have the action land
    /// on one of its own descendants.
    ///
    /// GPUI answers an incoming `Click` by synthesizing a mouse press at the
    /// node's bounds centre. When a smaller clickable node sits at that centre
    /// — a close button inside a tab, say — the action reaches the wrong
    /// control, and the node reads as operable right up until someone tries.
    #[track_caller]
    pub fn assert_click_targets_are_reachable(tree: &serde_json::Value, context: &str) {
        let nodes = nodes(tree);
        let clickable = |node: &serde_json::Value| {
            node["aria"]["on_action"]
                .as_array()
                .into_iter()
                .flatten()
                .any(|action| action == "Click")
        };
        let bounds = |node: &serde_json::Value| {
            let bounds = node.get("bounds")?;
            Some((
                bounds["x0"].as_f64()?,
                bounds["y0"].as_f64()?,
                bounds["x1"].as_f64()?,
                bounds["y1"].as_f64()?,
            ))
        };

        let mut misdirected = Vec::new();
        for node in nodes.values() {
            if !clickable(node) {
                continue;
            }
            let Some((x0, y0, x1, y1)) = bounds(node) else {
                continue;
            };
            let (centre_x, centre_y) = ((x0 + x1) / 2.0, (y0 + y1) / 2.0);

            // Only descendants: an ancestor covering the centre is the normal
            // case and does not steal the click, since the press is dispatched
            // to the topmost element at that point.
            let mut stack: Vec<&str> = node["children"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|child| child.as_str())
                .collect();
            while let Some(descendant_id) = stack.pop() {
                let Some(descendant) = nodes.get(descendant_id) else {
                    continue;
                };
                stack.extend(
                    descendant["children"]
                        .as_array()
                        .into_iter()
                        .flatten()
                        .filter_map(|child| child.as_str()),
                );
                if !clickable(descendant) {
                    continue;
                }
                let Some((dx0, dy0, dx1, dy1)) = bounds(descendant) else {
                    continue;
                };
                if (dx0..=dx1).contains(&centre_x) && (dy0..=dy1).contains(&centre_y) {
                    misdirected.push(format!(
                        "{} ({}) would click {} ({}) instead",
                        node["aria"]["role"],
                        node["element_id"],
                        descendant["aria"]["role"],
                        descendant["element_id"]
                    ));
                }
            }
        }
        assert!(
            misdirected.is_empty(),
            "{context}: {misdirected:?}"
        );
    }

    /// Panics if a containment-dependent node has no matching container among
    /// its ancestors.
    #[track_caller]
    pub fn assert_roles_are_contained(tree: &serde_json::Value, context: &str) {
        let nodes = nodes(tree);
        let role_of = |id: &str| {
            nodes[id]["aria"]["role"]
                .as_str()
                .unwrap_or_default()
                .to_string()
        };

        let mut parent_of: collections::FxHashMap<&str, &str> = collections::FxHashMap::default();
        for (id, node) in nodes {
            for child in node["children"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|child| child.as_str())
            {
                parent_of.insert(child, id.as_str());
            }
        }

        let orphaned: Vec<String> = nodes
            .iter()
            .filter_map(|(id, node)| {
                let role = role_of(id);
                let (_, container) = ROLE_REQUIRES_CONTAINER
                    .iter()
                    .find(|(needle, _)| *needle == role)?;
                let mut ancestor = parent_of.get(id.as_str()).copied();
                while let Some(current) = ancestor {
                    if role_of(current) == *container {
                        return None;
                    }
                    ancestor = parent_of.get(current).copied();
                }
                Some(format!(
                    "{role} ({}) has no {container} ancestor",
                    node["element_id"]
                ))
            })
            .collect();
        assert!(
            orphaned.is_empty(),
            "{context}: {orphaned:?}"
        );
    }
}

#[cfg(test)]
mod a11y_check_tests {
    use super::a11y_checks::{
        assert_active_descendant_is_honoured, assert_clickable_elements_are_reachable,
        assert_focusable_names_are_distinguishable,
        assert_controls_have_area, assert_focus_reached_the_tree,
        assert_interactive_nodes_are_named, assert_landmarks_are_distinguishable,
        assert_live_regions_can_speak, assert_names_are_distinguishable,
        assert_no_aria_was_discarded, assert_no_role_was_discarded, assert_roles_are_contained,
    };
    use serde_json::json;

    fn live_region(live: Option<&str>, label: Option<&str>, value: Option<&str>) -> serde_json::Value {
        let mut aria = serde_json::Map::new();
        aria.insert("role".into(), json!("Status"));
        if let Some(live) = live {
            aria.insert("live".into(), json!(live));
        }
        if let Some(label) = label {
            aria.insert("label".into(), json!(label));
        }
        if let Some(value) = value {
            aria.insert("value".into(), json!(value));
        }
        json!({ "nodes": { "0": { "element_id": "region", "aria": aria } } })
    }

    /// A live region whose content hangs off it as children, which is how the
    /// toast layer is built.
    fn live_region_with_child(child: serde_json::Value) -> serde_json::Value {
        json!({
            "nodes": {
                "0": {
                    "element_id": "region",
                    "aria": { "role": "Status", "live": "Polite" },
                    "children": ["1"],
                },
                "1": { "element_id": "content", "aria": child },
            }
        })
    }

    /// The shape every live region in the app had, and the shape they were
    /// briefly moved to. Each is silent on a different set of platforms.
    #[test]
    #[should_panic(expected = "macOS announces nothing")]
    fn a_label_without_a_value_is_reported() {
        let tree = live_region(Some("Polite"), Some("12 matches"), None);
        assert_live_regions_can_speak(&tree, "picker");
    }

    #[test]
    #[should_panic(expected = "Windows and Linux announce nothing")]
    fn a_value_without_a_label_is_reported() {
        let tree = live_region(Some("Polite"), None, Some("12 matches"));
        assert_live_regions_can_speak(&tree, "picker");
    }

    /// macOS speaks the value, Windows and Linux the name, so the announced
    /// text has to be in both.
    #[test]
    fn a_region_carrying_the_text_in_both_fields_can_speak() {
        let tree = live_region(Some("Polite"), Some("12 matches"), Some("12 matches"));
        assert_live_regions_can_speak(&tree, "picker");
        let tree = live_region(Some("Assertive"), Some("Disconnected"), Some("Disconnected"));
        assert_live_regions_can_speak(&tree, "connection");
    }

    /// A region that is deliberately not live has nothing to announce, so the
    /// check has no business asking it for either field.
    #[test]
    fn a_region_that_is_not_live_is_left_alone() {
        let tree = live_region(Some("Off"), Some("Overridden by your organization"), None);
        assert_live_regions_can_speak(&tree, "settings");
        let tree = live_region(None, Some("Failed to load"), None);
        assert_live_regions_can_speak(&tree, "markdown");
    }

    /// An empty string is the same silence as an absent field: `is_some()`
    /// passes and the announcement raised says nothing.
    #[test]
    #[should_panic(expected = "macOS announces nothing")]
    fn an_empty_value_is_reported_too() {
        let tree = live_region(Some("Polite"), Some("Recording"), Some(""));
        assert_live_regions_can_speak(&tree, "keystroke input");
    }

    /// The required shape, not a defect: the region has to be in the tree
    /// before its content arrives or there is no change to notice. Empty, it
    /// has nothing to say and neither field is the honest answer.
    #[test]
    fn an_empty_live_region_waiting_for_content_is_fine() {
        let tree = live_region(Some("Polite"), None, None);
        assert_live_regions_can_speak(&tree, "toast layer");
    }

    /// Content hanging off the region as a child node is still content the
    /// user is meant to hear, and no platform reads a live region's subtree.
    #[test]
    #[should_panic(expected = "child nodes")]
    fn a_live_region_whose_content_is_a_child_is_reported() {
        let tree = live_region_with_child(json!({ "label": "Project saved" }));
        assert_live_regions_can_speak(&tree, "toast layer");
    }

    /// Every check here is only worth its call sites if it can fail. One of
    /// them shipped inert — `assert_no_aria_was_discarded` reported nothing at
    /// all because the trait method it rests on was defaulted and never
    /// forwarded — and it passed everywhere, which is what an inert check looks
    /// like from the outside. So each check gets an input it has to reject.
    #[test]
    #[should_panic(expected = "bare role")]
    fn the_unnamed_control_check_can_fail() {
        let tree = json!({ "nodes": { "0": {
            "element_id": "save", "aria": { "role": "Button" }
        }}});
        assert_interactive_nodes_are_named(&tree, "probe");
    }

    fn bare(role: &str) -> serde_json::Value {
        json!({ "nodes": { "0": { "element_id": "container", "aria": { "role": role } } } })
    }

    /// The container roles are in that list too, and they are the ones whose
    /// absence is quiet: a `TabBar` sets `TabList` whether or not it was given
    /// a name, so an unnamed one is a node announced as a bare "tab list"
    /// rather than no node at all.
    ///
    /// One test each. A loop over the three would panic on the first and prove
    /// nothing about the other two.
    #[test]
    #[should_panic(expected = "bare role")]
    fn an_unnamed_tab_list_is_reported() {
        assert_interactive_nodes_are_named(&bare("TabList"), "probe");
    }

    #[test]
    #[should_panic(expected = "bare role")]
    fn an_unnamed_tree_is_reported() {
        assert_interactive_nodes_are_named(&bare("Tree"), "probe");
    }

    #[test]
    #[should_panic(expected = "bare role")]
    fn an_unnamed_table_is_reported() {
        assert_interactive_nodes_are_named(&bare("Table"), "probe");
    }

    #[test]
    #[should_panic(expected = "bare role")]
    fn an_unnamed_password_field_is_reported() {
        assert_interactive_nodes_are_named(&bare("PasswordInput"), "probe");
    }

    #[test]
    #[should_panic(expected = "never became nodes")]
    fn the_discarded_role_check_can_fail() {
        let tree = json!({
            "nodes": {},
            "frame": { "roles_without_id": ["Button at crates/example.rs:1:1"] },
        });
        assert_no_role_was_discarded(&tree, "probe");
    }

    #[test]
    #[should_panic(expected = "reached no node")]
    fn the_discarded_aria_check_can_fail() {
        let tree = json!({
            "nodes": {},
            "frame": { "aria_without_role": ["crates/example.rs:1:1"] },
        });
        assert_no_aria_was_discarded(&tree, "probe");
    }

    #[test]
    #[should_panic(expected = "one main region")]
    fn the_landmark_check_can_fail() {
        let tree = json!({ "nodes": {
            "0": { "element_id": "a", "aria": { "role": "Main" } },
            "1": { "element_id": "b", "aria": { "role": "Main" } },
        }});
        assert_landmarks_are_distinguishable(&tree, "probe");
    }

    #[test]
    #[should_panic(expected = "cannot tell apart")]
    fn the_name_clash_check_can_fail() {
        let tree = json!({ "nodes": {
            "0": { "element_id": "a", "aria": { "role": "Button", "label": "Close" } },
            "1": { "element_id": "b", "aria": { "role": "Button", "label": "Close" } },
        }});
        assert_names_are_distinguishable(&tree, "probe");
    }

    #[test]
    #[should_panic(expected = "announced to nobody")]
    fn the_active_descendant_check_can_fail() {
        let tree = json!({
            "nodes": {},
            "frame": { "active_descendant_without_focus": true },
        });
        assert_active_descendant_is_honoured(&tree, "probe");
    }

    #[test]
    #[should_panic(expected = "nothing to click")]
    fn the_control_area_check_can_fail() {
        let tree = json!({ "nodes": { "0": {
            "element_id": "save",
            "aria": { "role": "Button", "label": "Save" },
            "bounds": { "x0": 10.0, "y0": 10.0, "x1": 10.0, "y1": 20.0 },
        }}});
        assert_controls_have_area(&tree, "probe");
    }

    #[test]
    #[should_panic(expected = "no accessibility node")]
    fn the_focus_check_can_fail() {
        let tree = json!({
            "nodes": {},
            "frame": { "focus_without_node": "the focused element never rendered" },
        });
        assert_focus_reached_the_tree(&tree, "probe");
    }

    #[test]
    #[should_panic(expected = "has no ListBox ancestor")]
    fn the_containment_check_can_fail() {
        let tree = json!({ "nodes": {
            "0": { "element_id": "group", "aria": { "role": "Group" }, "children": ["1"] },
            "1": { "element_id": "row", "aria": { "role": "ListBoxOption", "label": "a.rs" } },
        }});
        assert_roles_are_contained(&tree, "probe");
    }

    fn focusable(id: &str, role: &str, label: &str, children: &[&str]) -> serde_json::Value {
        json!({
            "element_id": id,
            "aria": { "role": role, "label": label, "on_action": ["Focus"] },
            "children": children,
        })
    }

    /// Two panes running the same program, or two editors both called
    /// "Editor": the keyboard lands on each and a reader says the same words.
    #[test]
    #[should_panic(expected = "say the same thing")]
    fn two_focusable_siblings_with_one_name_are_reported() {
        let tree = json!({ "nodes": {
            "0": focusable("left", "Group", "Terminal", &[]),
            "1": focusable("right", "Group", "Terminal", &[]),
        }});
        assert_focusable_names_are_distinguishable(&tree, "panes");
    }

    /// A pane named after the item inside it. Redundant to hear twice, but not
    /// two destinations — you cannot be in one without being in the other, so
    /// there is nothing to confuse.
    #[test]
    fn a_focusable_child_repeating_its_parents_name_is_fine() {
        let tree = json!({ "nodes": {
            "0": focusable("pane", "Group", "edited", &["1"]),
            "1": focusable("item", "Group", "edited", &[]),
        }});
        assert_focusable_names_are_distinguishable(&tree, "pane and item");
    }

    /// Rows are told apart by the branch they hang from, and a reader
    /// announces that with them. Two `main.rs` in different folders are how a
    /// project looks, not a defect.
    #[test]
    fn rows_with_one_name_under_different_parents_are_fine() {
        let tree = json!({ "nodes": {
            "0": focusable("src", "Group", "src", &["2"]),
            "1": focusable("tests", "Group", "tests", &["3"]),
            "2": focusable("a", "TreeItem", "main.rs", &[]),
            "3": focusable("b", "TreeItem", "main.rs", &[]),
        }});
        assert_focusable_names_are_distinguishable(&tree, "tree");
    }

    /// …but two of them in the same folder cannot be told apart at all.
    #[test]
    #[should_panic(expected = "say the same thing")]
    fn rows_with_one_name_under_one_parent_are_reported() {
        let tree = json!({ "nodes": {
            "0": focusable("src", "Group", "src", &["1", "2"]),
            "1": focusable("a", "TreeItem", "main.rs", &[]),
            "2": focusable("b", "TreeItem", "main.rs", &[]),
        }});
        assert_focusable_names_are_distinguishable(&tree, "tree");
    }

    /// Nothing the keyboard can land on, so nothing to tell apart.
    #[test]
    fn unfocusable_nodes_sharing_a_name_are_left_alone() {
        let tree = json!({ "nodes": {
            "0": { "element_id": "a", "aria": { "role": "Group", "label": "Terminal" } },
            "1": { "element_id": "b", "aria": { "role": "Group", "label": "Terminal" } },
        }});
        assert_focusable_names_are_distinguishable(&tree, "decorative");
    }

    fn tree(node_rects: &[(f64, f64, f64, f64)], clickable: &[(f64, f64, f64, f64)]) -> serde_json::Value {
        let nodes: serde_json::Map<String, serde_json::Value> = node_rects
            .iter()
            .enumerate()
            .map(|(ix, (x0, y0, x1, y1))| {
                (
                    ix.to_string(),
                    json!({
                        "element_id": format!("node-{ix}"),
                        "aria": { "role": "Button", "label": format!("node {ix}") },
                        "bounds": { "x0": x0, "y0": y0, "x1": x1, "y1": y1 },
                    }),
                )
            })
            .collect();
        json!({
            "nodes": nodes,
            "frame": {
                "clickable_without_role": clickable
                    .iter()
                    .map(|(x0, y0, x1, y1)| json!({
                        "source_location": "crates/example.rs:1:1",
                        "bounds": { "x0": x0, "y0": y0, "x1": x1, "y1": y1 },
                    }))
                    .collect::<Vec<_>>(),
            },
        })
    }

    /// A clickable wrapper whose child carries the role is the ordinary shape:
    /// assistive technology clicks by bounds centre and lands inside it.
    #[test]
    fn a_clickable_wrapper_around_a_node_is_fine() {
        let tree = tree(&[(10., 10., 20., 20.)], &[(0., 0., 100., 100.)]);
        assert_clickable_elements_are_reachable(&tree, "wrapper");
    }

    /// The shape that shipped the prompt's buttons: a clickable rectangle with
    /// nothing in it that ever became a node.
    #[test]
    #[should_panic(expected = "reachable by mouse only")]
    fn a_clickable_element_containing_nothing_is_reported() {
        let tree = tree(&[(500., 500., 600., 600.)], &[(0., 0., 100., 100.)]);
        assert_clickable_elements_are_reachable(&tree, "empty");
    }

    /// One source location draws many rows. A row with nothing in it is a
    /// defect whether or not the row above it is fine, so the check looks at
    /// every instance rather than the first one it saw.
    #[test]
    #[should_panic(expected = "reachable by mouse only")]
    fn one_empty_row_among_full_ones_is_still_reported() {
        let tree = tree(
            &[(10., 10., 20., 20.)],
            &[(0., 0., 100., 100.), (0., 200., 100., 300.)],
        );
        assert_clickable_elements_are_reachable(&tree, "rows");
    }

    /// An element with no area cannot contain anything, and a control with no
    /// area is already `assert_controls_have_area`'s business.
    #[test]
    fn an_element_with_no_area_is_left_to_the_other_check() {
        let tree = tree(&[(500., 500., 600., 600.)], &[(0., 0., 0., 0.)]);
        assert_clickable_elements_are_reachable(&tree, "empty rect");
    }
}
