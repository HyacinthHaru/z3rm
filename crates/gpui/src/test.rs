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
        "Button",
        "CheckBox",
        "Link",
        "ListBoxOption",
        "MenuItem",
        "MenuItemCheckBox",
        "RadioButton",
        "SpinButton",
        "Switch",
        "Tab",
        "TreeItem",
    ];

    /// Roles that only mean anything inside a matching container. A screen
    /// reader derives "tab 2 of 5" and the arrow-key conventions from that
    /// containment, so an orphaned option or tab loses all of it.
    pub const ROLE_REQUIRES_CONTAINER: &[(&str, &str)] = &[
        ("ListBoxOption", "ListBox"),
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
