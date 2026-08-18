//! Accessibility support, provided by [AccessKit][accesskit].
//!
//! There are user-facing guide-level docs [here](crate::_accessibility).
//!
//! ## Architecture
//!
//! ```text
//!                              ┌────────────────────────────────┐   ┌─────────────────────┐
//!                           ┌─▶│ AccessKit Adapter (MacOS)      │◀─▶│ MacOS System APIs   │
//!                           │  └────────────────────────────────┘   └─────────────────────┘
//!                           │
//! ┌──────┐   ┌───────────┐  │  ┌────────────────────────────────┐   ┌─────────────────────┐
//! │ GPUI │◀─▶│ AccessKit │◀─┼─▶│ AccessKit Adapter (Windows)    │◀─▶│ Windows System APIs │
//! └──────┘   └───────────┘  │  └────────────────────────────────┘   └─────────────────────┘
//!                           │
//!                           │  ┌────────────────────────────────┐   ┌─────────────────────┐
//!                           └─▶│ AccessKit Adapter (Linux)      │◀─▶│ dbus                │
//!                              └────────────────────────────────┘   └─────────────────────┘
//! ```
//!
//! In order for GPUI apps to be usable for people using assistive technology,
//! we must do a few things:
//! - Inform the system when the UI changes meaningfully. This includes:
//!   - Reporting new/removed/changed UI elements
//!   - *Not* reporting irrelevant UI changes, e.g. an invisible `div()` being
//!     added.
//!   - Reporting the appearance and capabilities of each UI element. For example:
//!     - What does this piece of text say?
//!     - How far along is this progress bar?
//!     - Can this node be focused?
//!     - Can this node have a value directly assigned? (e.g. a slider)
//! - Allowing the system to interact with the UI by dispatching actions to
//!   nodes. Note that AccessKit has its own [`Action`] type, which is not the
//!   [`crate::Action`] trait.
//! - Activate and deactivate accessibility features when requested by the
//!   system.
//!
//! Activating and deactivating at the right time is trivial, so I won't go into
//! detail here. The other two are almost orthogonal in implementation.
//!
//! The state for both lives in the [`A11y`] struct in this module.
//!
//! ### Reporting UI changes
//!
//! Every frame, we build a [`TreeUpdate`] and send it to the platform-specific
//! adapter. A [`TreeUpdate`] is a representation of a subset of the UI tree.
//! When the adapter receives the update, it diffs it against the previous
//! update, and calls platform-specific APIs to inform screen readers about the
//! changes. Nodes may have been created, destroyed, or updated.
//!
//! Each node has an ID, and this ID *should* be stable across frames. If a
//! node's ID changes, then, from AccessKit's point of view, it is a different
//! node.
//!
//! We derive the node ID from the [`GlobalElementId`] in
//! [`GlobalElementId::accesskit_node_id`]. Nodes without [`GlobalElementId`]s
//! cannot produce an AccessKit [`NodeId`], and so are not included in the
//! accessibility tree. We try to warn when using accessibility APIs on
//! [`div()`] without setting an ID.
//!
//! This all happens in [`Drawable::prepaint`]. The [`A11y`] struct maintains a
//! stack of nodes during prepainting, which we can use to calculate the
//! [`NodeId`]s, and record parent-child relationships. Once all [`Element`]s in
//! a frame have been prepainted, we send the resulting [`TreeUpdate`] object to
//! the adapter and the screen reader can announce the changes.
//!
//! #### Synthetic children
//!
//! Additionally, some nodes can register "synthetic children" using
//! [`Element::a11y_synthetic_children`]. Normally, one accesskit node is pushed
//! for every [`Element`] with a role and id. However, sometimes a single
//! element may want to produce many accesskit nodes. These extra nodes are
//! referred to as "synthetic children" of the element providing a non-default
//! [`Element::a11y_synthetic_children`] implementation.
//!
//! The user is provided a builder-style API using [`A11ySubtreeBuilder`], which
//! allows them to create push nodes that are children of the current node, as
//! well as modify the current node itself.
//!
//! GPUI calls this callback *after* prepainting (and just before popping the
//! corresponding element), since this step may need prepaint information to be
//! available. In the future, we may want to add prepaint information more
//! generally to [`Element::write_a11y_info`], but for now that's not necessary.
//!
//! ### Responding to actions
//!
//! On adapter creation, we provide a callback to the adapter, which can be used
//! to dispatch actions. This callback forwards to [`A11y::action_listeners`], a
//! mapping from [`NodeId`]s to action handlers (basically just `Box<dyn
//! Fn()>`).
//!
//! This is populated in:
//! - [`Window::on_a11y_action`], which is called by:
//! - [`Interactivity::paint`], which is called by:
//! - [`StatefulInteractiveElement::on_a11y_action`], which is a public-facing API
//!
//! These are cleared at the start of a frame, and re-populated during painting.
//!
//! [`NodeId`]: accesskit::NodeId

use crate::*;

pub(crate) mod debug;

use crate::{App, Bounds, FocusId, Pixels, SharedString, Window};
use accesskit::{Action, NodeId, TreeUpdate};
use collections::{FxHashMap, FxHashSet};
use smallvec::SmallVec;
use std::hash::{Hash, Hasher};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};

/// The fixed AccessKit node ID used for the root of every window's a11y tree.
pub(crate) const ROOT_NODE_ID: NodeId = NodeId(0);

/// A listener for an accessibility action on a specific node.
pub(crate) type A11yActionListener =
    Box<dyn FnMut(Option<&accesskit::ActionData>, &mut Window, &mut App) + 'static>;

/// Per-window accessibility state.
///
/// Manages the AccessKit tree that is built each frame and the mappings
/// needed to dispatch incoming action requests back to the right elements.
pub(crate) struct A11y {
    /// Whether accessibility has been [forcibly disabled] for this window.
    ///
    /// [forcibly disabled]: crate::Application::new_inaccessible
    force_disabled: bool,
    /// Whether a11y features have been requested by the system.
    ///
    /// Updated by AccessKit using callbacks provided to the adapter. Can change
    /// halfway through a frame.
    active_flag: Arc<AtomicBool>,
    /// Whether a11y features are active for *this specific frame*.
    ///
    /// At the start of each frame, we load [`Self::active_flag`] (using
    /// [`Self::sync_active_flag`]) and use this to determine whether we
    /// should construct a [`TreeUpdate`] for this frame. It's important that
    /// this value is stable within a frame, because the builder API exposed by
    /// this type maintains a stack of nodes and each must be pushed and popped
    /// exactly once.
    ///
    /// At the end of the frame, we re-call [`Self::sync_active_flag`] to
    /// determine whether we should actually send the finished [`TreeUpdate`].
    active_this_frame: bool,
    pub(crate) nodes: A11yNodeBuilder,
    pub(crate) focus_ids: FxHashMap<NodeId, FocusId>,
    pub(crate) node_bounds: FxHashMap<NodeId, Bounds<Pixels>>,
    pub(crate) action_listeners: FxHashMap<NodeId, Vec<(Action, A11yActionListener)>>,
    /// The window's title, used to label the root node so assistive
    /// technology can tell windows apart.
    window_title: Option<SharedString>,
    /// The focus id we most recently reported as having no accessibility node,
    /// used to log at most once per focus change rather than every frame.
    last_focus_without_node: Option<FocusId>,
    /// Set for the frame in which focus was dropped for lack of a node, so the
    /// debug dump can explain an otherwise silent `gpui_focus: null`.
    focus_without_node_this_frame: Option<&'static str>,
    /// Whether the focused element registered its focus handle this frame,
    /// which distinguishes an element that never rendered from one that
    /// rendered and simply produced no node.
    focused_element_rendered_this_frame: bool,
    /// Whether a node claimed to be the active descendant this frame without
    /// the focused node being one of its ancestors, so the claim was dropped.
    /// Elements that set a role this frame but had no element id, so the role
    /// was discarded. This is the quietest way to lose a node: nothing is
    /// missing from the code, only from the tree.
    roles_without_id_this_frame: Vec<(accesskit::Role, Option<&'static std::panic::Location<'static>>)>,
    aria_without_role_this_frame: Vec<Option<&'static std::panic::Location<'static>>>,
    /// Elements that answer a click but carry no role, so they produce no node
    /// at all. Nothing else in the tree records them: a check can only reason
    /// about nodes that exist, and these do not.
    clickable_without_role_this_frame:
        Vec<(Option<&'static std::panic::Location<'static>>, accesskit::Rect)>,
    /// Retains the last tree update (and, in debug builds, per-node provenance)
    /// so it can be dumped via [`crate::Window::debug_a11y_tree_json`].
    debug: debug::A11yDebug,
    /// Maps a view's [`EntityId`] to its `Render` type name
    #[cfg(debug_assertions)]
    pub(crate) view_type_names: FxHashMap<EntityId, &'static str>,
}

impl A11y {
    pub(crate) fn new(
        active_flag: Arc<AtomicBool>,
        force_disabled: bool,
        window_title: Option<SharedString>,
    ) -> Self {
        Self {
            force_disabled,
            active_flag,
            active_this_frame: false,
            nodes: A11yNodeBuilder::new(),
            focus_ids: FxHashMap::default(),
            node_bounds: FxHashMap::default(),
            action_listeners: FxHashMap::default(),
            window_title,
            last_focus_without_node: None,
            focus_without_node_this_frame: None,
            focused_element_rendered_this_frame: false,
            roles_without_id_this_frame: Vec::new(),
            aria_without_role_this_frame: Vec::new(),
            clickable_without_role_this_frame: Vec::new(),
            debug: debug::A11yDebug::default(),
            #[cfg(debug_assertions)]
            view_type_names: FxHashMap::default(),
        }
    }

    /// Records that an element asked for a role but had no element id, so its
    /// node could not be created. Node ids are derived from the element id, so
    /// a role on its own produces nothing at all.
    pub(crate) fn note_role_without_id(
        &mut self,
        role: accesskit::Role,
        source_location: Option<&'static std::panic::Location<'static>>,
    ) {
        // Kept unformatted until the frame ends: this runs for every element
        // that asks for a role without an id, on every frame a screen reader is
        // attached.
        let site = (role, source_location);
        if !self.roles_without_id_this_frame.contains(&site) {
            self.roles_without_id_this_frame.push(site);
        }
    }

    /// Records that an element carries accessibility information but no role,
    /// so no node was built for it and the information went nowhere.
    pub(crate) fn note_aria_without_role(
        &mut self,
        source_location: Option<&'static std::panic::Location<'static>>,
    ) {
        if !self.aria_without_role_this_frame.contains(&source_location) {
            self.aria_without_role_this_frame.push(source_location);
        }
    }

    /// Records that an element takes a click but was given no role, so it
    /// produced no accessibility node and the action it offers is reachable by
    /// mouse only.
    /// The bounds travel with the site because they are what makes the report
    /// usable: an element with no node of its own is harmless if something
    /// inside it does have one, and only the geometry can say so.
    ///
    /// Every instance is kept, not one per source location. A `render_match`
    /// that draws fifty rows is one location with fifty rectangles, and a row
    /// with nothing in it is a defect whether or not its neighbours are fine.
    pub(crate) fn note_clickable_without_role(
        &mut self,
        source_location: Option<&'static std::panic::Location<'static>>,
        bounds: accesskit::Rect,
    ) {
        self.clickable_without_role_this_frame
            .push((source_location, bounds));
    }

    /// Logs (once per focus change) that the focused element is not exposed to
    /// assistive technology because it has no accessibility node. When this
    /// happens, screen readers fall back to announcing the whole window instead
    /// of the focused element. The fix is to give the element both an
    /// `.id(...)` and a `.role(...)`.
    pub(crate) fn note_focus_without_node(&mut self, focus_id: FocusId, reason: &'static str) {
        self.focus_without_node_this_frame = Some(reason);
        if self.last_focus_without_node != Some(focus_id) {
            self.last_focus_without_node = Some(focus_id);
            log::info!(
                "a11y: focused element ({focus_id:?}) has no accessibility node \
                 ({reason}); assistive technology will announce the whole window \
                 instead. Give it both an `.id(...)` and a `.role(...)` to expose it."
            );
        }
    }

    pub(crate) fn set_window_title(&mut self, title: impl Into<SharedString>) {
        self.window_title = Some(title.into());
    }

    /// Ensures that [`Self::is_active`] returns up to date information.
    ///
    /// See the docs for [`Self::active_flag`] and [`Self::active_this_frame`]
    /// for more commentary.
    pub(crate) fn sync_active_flag(&mut self) {
        self.active_this_frame = !self.force_disabled && self.active_flag.load(Ordering::SeqCst);
    }

    pub(crate) fn is_active(&self) -> bool {
        self.active_this_frame
    }

    /// Whether elements may attach nodes right now.
    ///
    /// Distinct from [`Self::is_active`]: an element can be laid out while no
    /// frame is open — `prepaint_as_root` from a measurement pass, for
    /// instance — and a node pushed then has no tree to join. Callers that ask
    /// "is a screen reader listening" (rather than "may I push") want
    /// [`Self::is_active`], which stays true between frames.
    pub(crate) fn is_building_frame(&self) -> bool {
        self.active_this_frame && self.nodes.frame_open
    }

    pub(crate) fn set_focusable(&mut self, node_id: NodeId, focus_id: FocusId) {
        self.focus_ids.insert(node_id, focus_id);
    }

    /// Report `node_id` as the currently-focused node, if it is present in the
    /// tree.
    ///
    /// Must only be called once per frame.
    pub(crate) fn set_focus(&mut self, node_id: NodeId) {
        // A focused node must have been registered as focusable this frame.
        if !self.focus_ids.contains_key(&node_id) {
            if cfg!(debug_assertions) {
                panic!("set_focus called for a node that was not registered with set_focusable");
            } else {
                log::warn!(
                    "a11y: set_focus called for a node that was not registered with \
                     set_focusable ({node_id:?})"
                );
            }
        }
        if self.nodes.has_node(node_id) {
            // The focused element is properly exposed; reset the dedup so a
            // later focus on a node-less element logs again.
            self.last_focus_without_node = None;
            let claiming_handle = self.focus_ids.get(&node_id).copied();
            let same_handle_as_previous = self
                .nodes
                .focus
                .and_then(|previous| self.focus_ids.get(&previous).copied())
                .zip(claiming_handle)
                .is_some_and(|(previous, claiming)| previous == claiming);
            self.nodes.set_focus(node_id, same_handle_as_previous);
        } else {
            // The element registered a focus handle and an id, but never got a
            // node because it has no role.
            if let Some(focus_id) = self.focus_ids.get(&node_id).copied() {
                self.note_focus_without_node(focus_id, "it has an id but no role");
            }
        }
    }

    pub(crate) fn set_active_descendant(&mut self, node_id: NodeId) {
        // Only recorded here. Where the claim lands depends on where focus is,
        // and the focused element may not be prepainted until later in the
        // frame, so deciding now would make the outcome depend on sibling
        // order. Resolved in `A11yNodeBuilder::finalize`.
        self.nodes.claim_active_descendant(node_id);
    }

    /// Clear per-frame state and push the root node to start a new frame.
    pub(crate) fn begin_frame(&mut self) {
        self.focus_ids.clear();
        self.node_bounds.clear();
        self.action_listeners.clear();
        self.nodes.begin_frame(self.window_title.as_ref());
    }

    /// Record that the window has a focused handle but no element claimed it
    /// this frame.
    ///
    /// Distinct from [`Self::note_focus_without_node`], which fires when the
    /// focused element *did* render and simply lacked an id or a role. Here the
    /// element was not rendered at all, so nothing reports anything and the
    /// dump would otherwise show a null focus with no explanation — the same
    /// silence that made this class of bug hard to see in the first place.
    /// Records that the focused element rendered, whether or not it produced a
    /// node. An element can register a focus handle without going through the
    /// interactivity path that reports a missing id, so without this the
    /// end-of-frame diagnostic would blame a render that did happen.
    pub(crate) fn note_focused_element_rendered(&mut self) {
        self.focused_element_rendered_this_frame = true;
    }

    pub(crate) fn note_focus_element_not_rendered(&mut self) {
        if self.nodes.focus.is_none() && self.focus_without_node_this_frame.is_none() {
            self.focus_without_node_this_frame = Some(if self.focused_element_rendered_this_frame {
                "its element rendered but produced no accessibility node"
            } else {
                "its element was not rendered this frame"
            });
        }
    }

    /// Finalize the tree and produce a [`TreeUpdate`] for the platform adapter.
    pub(crate) fn end_frame(&mut self, mut frame: debug::FrameDebugInfo) -> TreeUpdate {
        let update = self.nodes.finalize();
        frame.focus_without_node = self.focus_without_node_this_frame.take();
        frame.roles_without_id = self
            .roles_without_id_this_frame
            .drain(..)
            .map(|(role, location)| match location {
                Some(location) => format!("{role:?} at {location}"),
                None => format!("{role:?}"),
            })
            .collect();
        frame.aria_without_role = self
            .aria_without_role_this_frame
            .drain(..)
            .map(|location| match location {
                Some(location) => location.to_string(),
                None => "unknown location".to_string(),
            })
            .collect();
        frame.clickable_without_role = self
            .clickable_without_role_this_frame
            .drain(..)
            .map(|(location, bounds)| debug::ClickableWithoutRole {
                source_location: match location {
                    Some(location) => location.to_string(),
                    None => "<unknown source location>".to_string(),
                },
                bounds,
            })
            .collect();
        frame.active_descendant_without_focus =
            std::mem::take(&mut self.nodes.active_descendant_without_focus);
        self.focused_element_rendered_this_frame = false;
        self.debug.capture(
            &update,
            self.nodes.focus,
            self.nodes.active_descendant,
            self.window_title.as_ref(),
            frame,
        );
        #[cfg(debug_assertions)]
        self.debug.capture_node_info(&self.nodes.node_info);
        update
    }

    pub(crate) fn debug_tree_json(&self) -> Option<String> {
        self.debug.to_json()
    }
}

/// Builder API for synthetic children. See the docs for
/// [`Element::a11y_synthetic_children`].
pub struct A11ySubtreeBuilder<'a> {
    parent_id: NodeId,
    nodes: &'a mut A11yNodeBuilder,
    node_bounds: &'a mut FxHashMap<NodeId, Bounds<Pixels>>,
    scale_factor: f32,
    /// Provenance of the real element whose `a11y_synthetic_children` is
    /// running.
    #[cfg(debug_assertions)]
    creator: debug::NodeCreator,
}

impl<'a> A11ySubtreeBuilder<'a> {
    pub(crate) fn new(
        parent_id: NodeId,
        nodes: &'a mut A11yNodeBuilder,
        node_bounds: &'a mut FxHashMap<NodeId, Bounds<Pixels>>,
        scale_factor: f32,
    ) -> Self {
        Self {
            parent_id,
            nodes,
            node_bounds,
            scale_factor,
            #[cfg(debug_assertions)]
            creator: debug::NodeCreator::default(),
        }
    }

    #[cfg(debug_assertions)]
    pub(crate) fn with_creator(mut self, creator: debug::NodeCreator) -> Self {
        self.creator = creator;
        self
    }

    /// Derive a [`NodeId`] for a synthetic child.
    ///
    /// The generated ID is based on the hash of `key`, as well as the parent's
    /// ID. This means that `key`s must be unique within the same
    /// [`Element::a11y_synthetic_children`] call, but may be duplicated across
    /// different calls.
    pub fn synthetic_node_id(&self, key: impl Hash) -> NodeId {
        let mut hasher = std::hash::DefaultHasher::default();
        self.parent_id.0.hash(&mut hasher);
        key.hash(&mut hasher);
        NodeId(hasher.finish())
    }

    /// Append a synthetic leaf node as a child of this element's node.
    ///
    /// Returns `false` if a node with this id is already present in the tree,
    /// in which case the node is discarded.
    /// Push a synthetic child that occupies a place on screen.
    ///
    /// A synthetic node with bounds is a control as far as a reader is
    /// concerned — something to route a click to and to scroll into view — and
    /// this gives it the same treatment a real element gets: the bounds are
    /// written to the node for the platform, and registered so that GPUI's
    /// `Action::Click` fallback can synthesize a press at its centre. Without
    /// the registration the node is announced and cannot be operated.
    pub fn push_child_with_bounds(
        &mut self,
        id: NodeId,
        mut node: accesskit::Node,
        bounds: Bounds<Pixels>,
    ) -> bool {
        let scale = self.scale_factor;
        node.set_bounds(accesskit::Rect {
            x0: (bounds.origin.x.0 * scale) as f64,
            y0: (bounds.origin.y.0 * scale) as f64,
            x1: ((bounds.origin.x.0 + bounds.size.width.0) * scale) as f64,
            y1: ((bounds.origin.y.0 + bounds.size.height.0) * scale) as f64,
        });
        let pushed = self.push_child(id, node);
        if pushed {
            self.node_bounds.insert(id, bounds);
        }
        pushed
    }

    pub fn push_child(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        let pushed = self.nodes.push_leaf(id, node);
        #[cfg(debug_assertions)]
        if pushed {
            self.nodes.record_node_info(
                id,
                debug::NodeDebugInfo {
                    synthetic: true,
                    view: self.creator.view,
                    element_id: self.creator.element_id.clone(),
                    source_location: self.creator.source_location,
                },
            );
        }
        pushed
    }

    /// A mutable reference to the parent node.

    /// Expose a plain string as AccessKit text runs plus the selection for the
    /// given byte offsets, enabling the platform text pattern (caret tracking,
    /// review, typed-character echo) on a custom text control.
    ///
    /// Text is chunked so per-run character indices fit AccessKit's `u8`-indexed
    /// `word_starts`; an empty string still produces one run so the pattern stays
    /// supported when the control is empty.
    pub fn push_text_runs(&mut self, text: &str, selection_tail: usize, selection_head: usize) {
        let (runs, selection) =
            build_a11y_text_runs(text, selection_tail, selection_head, |chunk| {
                self.synthetic_node_id(chunk)
            });
        for (id, node) in runs {
            self.push_child(id, node);
        }
        self.parent_node().set_text_selection(selection);
    }

    /// The node of the element that owns this subtree, so callers can set
    /// properties that describe the synthetic children as a whole.
    pub fn parent_node(&mut self) -> &mut accesskit::Node {
        self.nodes
            .current_node_mut()
            .expect("A11ySubtreeBuilder exists only while its element's node is on the stack")
    }
}

pub(crate) struct A11yNodeBuilder {
    /// Whether a frame is currently being built. Nodes can only be attached
    /// between [`Self::begin_frame`] and [`Self::finalize`]; outside that
    /// window there is no tree for them to join.
    frame_open: bool,
    ids_stack: SmallVec<[NodeId; 16]>,
    nodes_stack: SmallVec<[accesskit::Node; 16]>,
    /// This is the exact type required by accesskit, so we can't just make it a
    /// `HashMap<NodeId, Node>` to remove the need for `seen_ids`
    all_nodes: Vec<(NodeId, accesskit::Node)>,
    seen_ids: FxHashSet<NodeId>,
    /// The node that GPUI considers focused. Note that this may be different to
    /// what is reported to accesskit - see [`Self::active_descendant`]
    focus: Option<NodeId>,
    /// If a node calls `.aria_active_descendant()`, AND an ancestor is focused,
    /// override it as the focused node. This supports the "active descendant"
    /// pattern, which allows a focused container to act as if a descendant is
    /// focused.
    active_descendant: Option<NodeId>,
    /// A claim made from outside the focused node's subtree — a list filtered
    /// from its own input is the usual shape. Focus stays where the keyboard
    /// is, and the focused node points at the highlighted row instead, which is
    /// what a reader needs to announce it.
    active_descendant_of_focus: Option<NodeId>,
    /// A claim and the ancestors of the node that made it, held until every
    /// node exists. Whether the claim becomes the reported focus or a pointer
    /// hung off the focused node depends on where focus turns out to be, which
    /// is not known while the frame is still being built.
    pending_active_descendant: Option<(NodeId, SmallVec<[NodeId; 16]>)>,
    /// Set in `finalize` when a claim had no focus anywhere to attach to.
    active_descendant_without_focus: bool,
    #[cfg(debug_assertions)]
    node_info: FxHashMap<NodeId, debug::NodeDebugInfo>,
}

impl A11yNodeBuilder {
    fn new() -> Self {
        Self {
            frame_open: false,
            ids_stack: SmallVec::new(),
            nodes_stack: SmallVec::new(),
            all_nodes: Vec::new(),
            seen_ids: FxHashSet::default(),
            focus: None,
            active_descendant: None,
            active_descendant_of_focus: None,
            pending_active_descendant: None,
            active_descendant_without_focus: false,
            #[cfg(debug_assertions)]
            node_info: FxHashMap::default(),
        }
    }

    /// Records provenance for a node already pushed this frame. Debug builds only.
    #[cfg(debug_assertions)]
    pub(crate) fn record_node_info(&mut self, id: NodeId, info: debug::NodeDebugInfo) {
        self.node_info.insert(id, info);
    }

    #[must_use]
    fn can_push(&mut self, id: NodeId) -> bool {
        debug_assert!(!self.ids_stack.is_empty(), "node pushed before push_root");

        if !self.seen_ids.insert(id) {
            debug_assert!(
                false,
                "Duplicate a11y node id: {id:?}. In a release build, this node would be silently discarded from the a11y tree."
            );
            return false;
        }

        true
    }

    /// Push a new node onto the stack. It becomes a child of the current
    /// top-of-stack node.
    ///
    /// Returns `true` if the node was successfully pushed.
    pub(crate) fn push(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        if !self.can_push(id) {
            return false;
        }

        if let Some(parent) = self.nodes_stack.last_mut() {
            parent.push_child(id);
        }
        self.ids_stack.push(id);
        self.nodes_stack.push(node);
        true
    }

    /// Add a leaf node as a child of the current top-of-stack node, without
    /// pushing it onto the stack. Semantically equivalent to a [`Self::push`]
    /// followed by a [`Self::pop`].
    ///
    /// Returns `true` if the node was successfully pushed.
    pub(crate) fn push_leaf(&mut self, id: NodeId, node: accesskit::Node) -> bool {
        if !self.can_push(id) {
            return false;
        }

        if let Some(parent) = self.nodes_stack.last_mut() {
            parent.push_child(id);
        }
        self.all_nodes.push((id, node));
        true
    }

    pub(crate) fn current_node_mut(&mut self) -> Option<&mut accesskit::Node> {
        self.nodes_stack.last_mut()
    }

    /// The node on top of the stack, but only if it is `node_id`'s.
    ///
    /// An element that produced no node of its own leaves its parent's on top,
    /// and a caller identifying itself by id must not reach that instead.
    pub(crate) fn current_node_mut_if(&mut self, node_id: NodeId) -> Option<&mut accesskit::Node> {
        (self.ids_stack.last() == Some(&node_id)).then(|| self.nodes_stack.last_mut())?
    }

    /// Pop the current node off the stack and finalize it into the all_nodes
    /// list.
    pub(crate) fn pop(&mut self) {
        debug_assert!(self.ids_stack.len() > 1, "pop would remove the root node");

        if let (Some(id), Some(node)) = (self.ids_stack.pop(), self.nodes_stack.pop()) {
            self.all_nodes.push((id, node));
        }
    }

    /// Push the root node to start a new frame.
    fn begin_frame(&mut self, window_title: Option<&SharedString>) {
        self.frame_open = true;
        self.all_nodes.clear();
        self.ids_stack.clear();
        self.nodes_stack.clear();
        self.seen_ids.clear();
        #[cfg(debug_assertions)]
        self.node_info.clear();
        let mut root_node = accesskit::Node::new(accesskit::Role::Window);
        if let Some(title) = window_title {
            root_node.set_label(title.to_string());
        }

        self.ids_stack.push(ROOT_NODE_ID);
        self.nodes_stack.push(root_node);
        self.focus = None;
        self.active_descendant = None;
        self.active_descendant_of_focus = None;
        self.pending_active_descendant = None;
        self.active_descendant_without_focus = false;
    }

    /// Returns whether a node with the given ID has been pushed in this frame.
    pub(crate) fn has_node(&self, id: NodeId) -> bool {
        id == ROOT_NODE_ID || self.seen_ids.contains(&id)
    }

    /// Record a claim along with the claiming node's ancestors, to be resolved
    /// once the whole frame is known.
    pub(crate) fn claim_active_descendant(&mut self, id: NodeId) {
        if self
            .pending_active_descendant
            .as_ref()
            .is_some_and(|(existing, _)| *existing != id)
        {
            if cfg!(debug_assertions) {
                panic!("active descendant claimed by multiple nodes in one frame");
            } else {
                log::warn!(
                    "a11y: multiple nodes claimed the active descendant this frame; \
                     using last-wins ({id:?})"
                );
            }
        }
        // The claiming node is on top of the stack; everything below it is an
        // ancestor.
        let ancestor_count = self.ids_stack.len().saturating_sub(1);
        self.pending_active_descendant =
            Some((id, SmallVec::from_slice(&self.ids_stack[..ancestor_count])));
    }

    /// Decide where the frame's active-descendant claim lands, now that every
    /// node has been pushed and focus is known.
    fn resolve_active_descendant(&mut self) {
        let Some((target, ancestors)) = self.pending_active_descendant.take() else {
            return;
        };
        if !self.has_node(target) {
            return;
        }
        if self.focus == Some(target) {
            // The claim would report the node as focused via itself, which says
            // nothing and hides the mistake.
            if cfg!(debug_assertions) {
                panic!("set_active_descendant called on the focused node");
            } else {
                log::warn!("a11y: set_active_descendant called on the focused node ({target:?})");
            }
            return;
        }
        let Some(focus) = self.focus else {
            // Nothing is focused at all, so there is nothing to hang the claim
            // on and no keyboard position to describe. Recorded rather than
            // dropped in silence, which is how the picker's call stayed a
            // no-op for so long.
            self.active_descendant_without_focus = true;
            return;
        };
        if ancestors.contains(&focus) {
            self.active_descendant = Some(target);
        } else {
            // A highlight in a list the user filters from a separate input:
            // focus is in the input, which is not an ancestor of the row, so
            // reporting the row as focused would misstate where the keyboard
            // is. The focused node points at the row instead — the same shape
            // as a combo box — and the reader announces both.
            self.active_descendant_of_focus = Some(target);
        }
    }

    /// Report `id` as the focused node.
    ///
    /// `same_handle_as_previous` tells whether this claim comes from the same
    /// focus handle as the last one this frame. Nesting an element that tracks
    /// a handle inside another element tracking the same handle is a normal
    /// GPUI pattern — a terminal surface inside its labelled pane, say — and
    /// both report focus. The innermost claim wins, since it is the more
    /// specific target. Two *different* handles claiming focus in one frame is
    /// a real bug and still fails loudly.
    pub(crate) fn set_focus(&mut self, id: NodeId, same_handle_as_previous: bool) {
        if self.focus.is_some() && !same_handle_as_previous {
            if cfg!(debug_assertions) {
                panic!("set_focus called more than once in a single frame");
            } else {
                log::warn!(
                    "a11y: set_focus called more than once in a single frame; \
                     using last-wins ({id:?})"
                );
            }
        }
        self.focus = Some(id);
    }

    fn finalize(&mut self) -> TreeUpdate {
        self.frame_open = false;
        self.resolve_active_descendant();
        // Stack should contain only the root node
        debug_assert_eq!(self.ids_stack.len(), 1);
        debug_assert_eq!(self.ids_stack[0], ROOT_NODE_ID);

        if self.ids_stack.len() != 1 {
            log::error!(
                "a11y: Stack imbalance at end of frame: expected 1 (root), got {}. \
                 Some elements may have pushed without popping.",
                self.ids_stack.len()
            );
        }

        // Pop remaining nodes (should just be the root).
        while !self.ids_stack.is_empty() {
            if let (Some(id), Some(node)) = (self.ids_stack.pop(), self.nodes_stack.pop()) {
                self.all_nodes.push((id, node));
            }
        }

        let focus = match self.active_descendant {
            Some(id) if self.has_node(id) => id,
            Some(id) => {
                if cfg!(debug_assertions) {
                    panic!("active_descendant set to {id:?}, which is not in the tree");
                } else {
                    log::warn!("active_descendant set to {id:?}, which is not in the tree");
                    self.focus.unwrap_or(ROOT_NODE_ID)
                }
            }

            _ => self.focus.unwrap_or(ROOT_NODE_ID),
        };

        // Attached last, once every node exists: the focused node points at the
        // row it highlights, which is how a reader announces a list the user is
        // filtering from somewhere else.
        //
        // No platform adapter reads `active_descendant`; grepping all three for
        // it finds nothing, which makes this look like a property that goes
        // nowhere. It is resolved a layer earlier: `accesskit_consumer`'s
        // `Node::is_focused` answers true for the focused node's active
        // descendant and false for the focused node itself, so the adapters
        // announce the row through the focus machinery they already use.
        if let (Some(target), Some(focused_id)) = (self.active_descendant_of_focus, self.focus)
            && self.has_node(target)
            && let Some((_, node)) = self
                .all_nodes
                .iter_mut()
                .find(|(id, _)| *id == focused_id)
        {
            node.set_active_descendant(target);
        }

        let nodes = std::mem::take(&mut self.all_nodes);
        let update = TreeUpdate {
            nodes,
            tree: Some(accesskit::Tree::new(ROOT_NODE_ID)),
            tree_id: accesskit::TreeId::ROOT,
            focus,
        };

        Self::repair_tree_update(update)
    }

    /// Accesskit panics on invalid [`TreeUpdate`]s. This function defensively
    /// checks invariants that accesskit panics on, and tries to fix them.
    fn repair_tree_update(mut update: TreeUpdate) -> TreeUpdate {
        let node_ids: FxHashSet<NodeId> = update.nodes.iter().map(|(id, _)| *id).collect();

        // Focus must point to a node in the tree.
        if !node_ids.contains(&update.focus) {
            log::error!(
                "a11y: Focused node {:?} is not in the tree ({} nodes). \
                 Falling back to root. This is a bug in the a11y tree builder.",
                update.focus,
                update.nodes.len()
            );
            update.focus = ROOT_NODE_ID;
        }

        // Every child reference must point to a node in the update.
        for (id, node) in &mut update.nodes {
            let has_invalid_child = node
                .children()
                .iter()
                .any(|child_id| !node_ids.contains(child_id));
            if has_invalid_child {
                let children = node.children();
                let invalid_count = children
                    .iter()
                    .filter(|child_id| !node_ids.contains(child_id))
                    .count();
                log::error!(
                    "a11y: Node {:?} references {} children not present in the tree. \
                     Stripping invalid child references.",
                    id,
                    invalid_count
                );
                let valid: Vec<NodeId> = children
                    .iter()
                    .copied()
                    .filter(|child_id| node_ids.contains(child_id))
                    .collect();
                node.set_children(valid);
            }
        }

        update
    }
}

#[cfg(test)]
mod tests {
    /// Nesting an element that tracks a focus handle inside another element
    /// tracking the same handle is a normal GPUI pattern — a terminal surface
    /// inside its labelled pane — and both report focus. The inner one is the
    /// more specific target, so it wins rather than tripping the invariant.
    #[test]
    fn nested_elements_sharing_a_focus_handle_report_the_innermost() {
        let mut builder = new_builder();

        let outer = NodeId(1);
        let inner = NodeId(2);
        builder.push(outer, test_node());
        builder.push(inner, test_node());

        builder.set_focus(outer, false);
        builder.set_focus(inner, true);

        assert_eq!(
            builder.focus,
            Some(inner),
            "the innermost element tracking the handle is what should be announced"
        );
    }

    /// An element can be laid out while no frame is open — a measurement pass
    /// calling `prepaint_as_root`, for instance. A node pushed then has no
    /// tree to join, and pushing anyway trips the "node pushed before
    /// push_root" invariant.
    #[test]
    fn pushes_are_closed_between_frames() {
        let mut a11y = super::A11y::new(std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)), false, None);
        a11y.sync_active_flag();

        assert!(a11y.is_active(), "the flag is on");
        assert!(
            !a11y.is_building_frame(),
            "no frame has begun, so nothing may be attached yet"
        );

        a11y.begin_frame();
        assert!(a11y.is_building_frame());

        let _ = a11y.end_frame(super::debug::FrameDebugInfo::default());
        assert!(
            a11y.is_active(),
            "the screen reader is still listening between frames"
        );
        assert!(
            !a11y.is_building_frame(),
            "but the frame is closed, so pushes must not be attempted"
        );
    }

    // Import specific items rather than glob-importing `super`, which would pull
    // in gpui's own `test` attribute macro and shadow the standard one.
    use super::{A11y, A11yNodeBuilder, ROOT_NODE_ID};
    use crate::FocusId;
    use accesskit::{NodeId, Role};
    use std::sync::{Arc, atomic::AtomicBool};

    fn test_node() -> accesskit::Node {
        accesskit::Node::new(Role::GenericContainer)
    }

    fn new_builder() -> A11yNodeBuilder {
        let mut builder = A11yNodeBuilder::new();
        builder.begin_frame(None);
        builder
    }

    fn new_a11y() -> A11y {
        let mut a11y = A11y::new(Arc::new(AtomicBool::new(true)), false, None);
        a11y.begin_frame();
        a11y
    }

    #[test]
    fn active_descendant_honored_when_container_focused() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container, false);
        assert!(builder.push(item, test_node()));

        // The item is on top of the stack; the focused container is its
        // ancestor, so the claim is honored.
        builder.claim_active_descendant(item);

        builder.pop(); // item
        builder.pop(); // container
        let update = builder.finalize();
        assert_eq!(update.focus, item);
    }

    #[test]
    fn active_descendant_honored_for_deep_descendant() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let group = NodeId(2);
        let item = NodeId(3);

        assert!(builder.push(container, test_node()));
        builder.set_focus(container, false);
        assert!(builder.push(group, test_node()));
        assert!(builder.push(item, test_node()));

        // The item is a grandchild of the focused container; depth doesn't
        // matter, the focused ancestor is still on the stack.
        builder.claim_active_descendant(item);

        builder.pop(); // item
        builder.pop(); // group
        builder.pop(); // container
        let update = builder.finalize();
        assert_eq!(update.focus, item);
    }

    /// The list-plus-filter shape: the keyboard is in the input, the highlight
    /// is in a list beside it. Focus has to stay on the input — saying the row
    /// is focused would be a lie about where typing goes — so the input points
    /// at the row instead, and a reader announces both.
    #[test]
    fn a_claim_from_another_subtree_lands_on_the_focused_node() {
        let mut builder = new_builder();
        let input = NodeId(1);
        let list = NodeId(2);
        let row = NodeId(3);

        assert!(builder.push(input, test_node()));
        builder.set_focus(input, false);
        builder.pop();

        assert!(builder.push(list, test_node()));
        assert!(builder.push(row, test_node()));
        builder.claim_active_descendant(row);
        builder.pop();
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, input, "the keyboard is still in the input");
        let focused_node = update
            .nodes
            .iter()
            .find(|(id, _)| *id == input)
            .map(|(_, node)| node)
            .expect("the focused node is in the tree");
        assert_eq!(
            focused_node.active_descendant(),
            Some(row),
            "the input has to point at the row it is highlighting"
        );
    }

    /// The same shape as the test above, with the two subtrees swapped: the
    /// list is prepainted before the input that focus lands in. Resolving the
    /// claim where it was made saw no focus yet and threw it away, so whether a
    /// picker announced its highlighted row came down to sibling order.
    #[test]
    fn a_claim_made_before_focus_exists_still_lands() {
        let mut builder = new_builder();
        let list = NodeId(1);
        let row = NodeId(2);
        let input = NodeId(3);

        assert!(builder.push(list, test_node()));
        assert!(builder.push(row, test_node()));
        builder.claim_active_descendant(row);
        builder.pop();
        builder.pop();

        assert!(builder.push(input, test_node()));
        builder.set_focus(input, false);
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, input, "the keyboard is still in the input");
        assert!(
            !builder.active_descendant_without_focus,
            "the focus arrived later in the frame, which is not the same as never"
        );
        let focused_node = update
            .nodes
            .iter()
            .find(|(id, _)| *id == input)
            .map(|(_, node)| node)
            .expect("the focused node is in the tree");
        assert_eq!(
            focused_node.active_descendant(),
            Some(row),
            "the input has to point at the row it is highlighting"
        );
    }

    #[test]
    fn active_descendant_ignored_when_focus_in_other_subtree() {
        let mut builder = new_builder();
        let focused_container = NodeId(1);
        let focused_leaf = NodeId(2);
        let other_container = NodeId(3);
        let other_item = NodeId(4);

        // First subtree holds real focus.
        assert!(builder.push(focused_container, test_node()));
        assert!(builder.push(focused_leaf, test_node()));
        builder.set_focus(focused_leaf, false);
        builder.pop(); // focused_leaf
        builder.pop(); // focused_container

        // Second subtree: its item would claim the active descendant, but the
        // focus is not on any of its ancestors, so the gate rejects it.
        assert!(builder.push(other_container, test_node()));
        assert!(builder.push(other_item, test_node()));
        builder.pop(); // other_item
        builder.pop(); // other_container

        let update = builder.finalize();
        assert_eq!(update.focus, focused_leaf);
    }

    #[test]
    fn active_descendant_ignored_when_nothing_focused() {
        let mut builder = new_builder();
        let container = NodeId(1);
        let item = NodeId(2);

        assert!(builder.push(container, test_node()));
        assert!(builder.push(item, test_node()));

        // Nothing is focused, so there is no keyboard position for the claim
        // to describe and nowhere to hang it.
        builder.claim_active_descendant(item);
        builder.pop();
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, ROOT_NODE_ID);
        assert!(
            builder.active_descendant_without_focus,
            "a claim with no focus anywhere has to be reported, not dropped"
        );
    }

    #[test]
    fn regular_focus_used_when_no_active_descendant() {
        let mut builder = new_builder();
        let focused = NodeId(1);

        assert!(builder.push(focused, test_node()));
        builder.set_focus(focused, false);
        builder.pop();

        let update = builder.finalize();
        assert_eq!(update.focus, focused);
    }

    // The double-claim guard panics only in debug builds; in release it falls
    // back to last-wins with a warning.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "active descendant claimed by multiple nodes")
    )]
    fn multiple_active_descendant_claims_panic_in_debug() {
        let mut builder = new_builder();
        builder.claim_active_descendant(NodeId(1));
        builder.claim_active_descendant(NodeId(2));
    }

    // Setting focus twice in one frame means two elements both claimed window
    // focus; that panics in debug and falls back to last-wins in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "set_focus called more than once")
    )]
    fn setting_focus_twice_panics_in_debug() {
        let mut builder = new_builder();
        builder.set_focus(NodeId(1), false);
        builder.set_focus(NodeId(2), false);
    }

    // Focusing a node that was never registered as focusable is a bug: panic in
    // debug, warn in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "was not registered with set_focusable")
    )]
    fn set_focus_without_set_focusable() {
        let mut a11y = new_a11y();
        let node = NodeId(1);
        assert!(a11y.nodes.push(node, test_node()));
        // set_focusable was never called for `node`.
        a11y.set_focus(node);
    }

    // The focused node cannot also be its own active descendant: panic in
    // debug, warn in release.
    #[test]
    #[cfg_attr(debug_assertions, should_panic(expected = "on the focused node"))]
    fn set_active_descendant_on_focused_node() {
        let mut a11y = new_a11y();
        let node = NodeId(1);
        assert!(a11y.nodes.push(node, test_node()));
        a11y.set_focusable(node, FocusId::default());
        a11y.set_focus(node);
        a11y.set_active_descendant(node);
        a11y.nodes.finalize();
    }

    // Two sibling children of a focused container both claim the active
    // descendant (both pass the focus gate). The second claim is a bug: panic
    // in debug, last-wins + warn in release.
    #[test]
    #[cfg_attr(
        debug_assertions,
        should_panic(expected = "active descendant claimed by multiple nodes")
    )]
    fn two_siblings_claiming_active_descendant() {
        let mut a11y = new_a11y();
        let container = NodeId(1);
        let first = NodeId(2);
        let second = NodeId(3);

        assert!(a11y.nodes.push(container, test_node()));
        a11y.set_focusable(container, FocusId::default());
        a11y.set_focus(container);

        assert!(a11y.nodes.push(first, test_node()));
        a11y.set_active_descendant(first);
        a11y.nodes.pop(); // first

        assert!(a11y.nodes.push(second, test_node()));
        a11y.set_active_descendant(second);
        a11y.nodes.pop(); // second

        a11y.nodes.pop(); // container
    }

    // Node A is focused; node C (a child of the unfocused node B) claims the
    // active descendant. The final tree must still report A as focused.
    #[test]
    fn active_descendant_in_unfocused_subtree_keeps_real_focus() {
        let mut a11y = new_a11y();
        let a = NodeId(1);
        let b = NodeId(2);
        let c = NodeId(3);

        assert!(a11y.nodes.push(a, test_node()));
        a11y.set_focusable(a, FocusId::default());
        a11y.set_focus(a);
        a11y.nodes.pop(); // a

        assert!(a11y.nodes.push(b, test_node()));
        assert!(a11y.nodes.push(c, test_node()));
        a11y.set_active_descendant(c);
        a11y.nodes.pop(); // c
        a11y.nodes.pop(); // b

        let update = a11y.end_frame(Default::default());
        assert_eq!(update.focus, a);
    }
}

/// AccessKit's `word_starts` uses `u8` indices, so a single text run cannot
/// exceed this many characters. Longer text is split into multiple runs.
const MAX_CHARS_PER_TEXT_RUN: usize = 255;

fn is_word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_'
}

fn char_index_for_byte(text: &str, byte_offset: usize) -> usize {
    text.char_indices()
        .take_while(|(byte_ix, _)| *byte_ix < byte_offset)
        .count()
}

/// Convert a character index into an AccessKit text position, accounting for
/// text that is split into multiple runs.
///
/// `synthetic_node_id` maps a chunk index to the run's node id (in practice
/// [`A11ySubtreeBuilder::synthetic_node_id`]); it is a parameter so this
/// arithmetic can be property-tested without constructing a builder.
fn a11y_text_position(
    char_index: usize,
    synthetic_node_id: impl Fn(u64) -> accesskit::NodeId,
) -> accesskit::TextPosition {
    // A position landing exactly on a chunk boundary refers to the end of the
    // previous chunk rather than the start of the next one.
    let chunk_index = if char_index > 0 && char_index.is_multiple_of(MAX_CHARS_PER_TEXT_RUN) {
        char_index / MAX_CHARS_PER_TEXT_RUN - 1
    } else {
        char_index / MAX_CHARS_PER_TEXT_RUN
    };
    accesskit::TextPosition {
        node: synthetic_node_id(chunk_index as u64),
        character_index: char_index - chunk_index * MAX_CHARS_PER_TEXT_RUN,
    }
}

/// Split `text` into AccessKit text runs (chunked small enough that per-run
/// character indices fit AccessKit's `u8`-indexed `word_starts`), and compute
/// the text selection for the given byte offsets.
///
/// `synthetic_node_id` maps a chunk index to that run's node id. Returns the
/// runs in order plus the selection, leaving it to the caller to push them —
/// this keeps the logic free of [`A11ySubtreeBuilder`] so it can be
/// property-tested against arbitrary strings.
///
/// `selection_tail` and `selection_head` are byte offsets into `text`.
fn build_a11y_text_runs(
    text: &str,
    selection_tail: usize,
    selection_head: usize,
    synthetic_node_id: impl Fn(u64) -> accesskit::NodeId,
) -> (
    Vec<(accesskit::NodeId, accesskit::Node)>,
    accesskit::TextSelection,
) {
    let chars: Vec<char> = text.chars().collect();
    let total_chars = chars.len();
    // Build at least one (possibly empty) run so the text pattern remains
    // supported when the field is empty.
    let num_chunks = total_chars.div_ceil(MAX_CHARS_PER_TEXT_RUN).max(1);

    let mut word_starts = Vec::new();
    let mut was_word_char = false;
    for (ix, c) in chars.iter().enumerate() {
        let is_word = is_word_char(*c);
        if is_word && !was_word_char {
            word_starts.push(ix);
        }
        was_word_char = is_word;
    }

    let mut runs = Vec::with_capacity(num_chunks);
    for chunk_index in 0..num_chunks {
        let char_start = chunk_index * MAX_CHARS_PER_TEXT_RUN;
        let char_end = (char_start + MAX_CHARS_PER_TEXT_RUN).min(total_chars);
        let chunk_chars = &chars[char_start..char_end];

        let mut node = accesskit::Node::new(accesskit::Role::TextRun);
        node.set_text_direction(accesskit::TextDirection::LeftToRight);
        node.set_value(chunk_chars.iter().collect::<String>());
        node.set_character_lengths(
            chunk_chars
                .iter()
                .map(|c| c.len_utf8() as u8)
                .collect::<Vec<u8>>(),
        );
        node.set_word_starts(
            word_starts
                .iter()
                .filter(|&&word_start| word_start >= char_start && word_start < char_end)
                .map(|&word_start| (word_start - char_start) as u8)
                .collect::<Vec<u8>>(),
        );
        if chunk_index > 0 {
            node.set_previous_on_line(synthetic_node_id(chunk_index as u64 - 1));
        }
        if chunk_index + 1 < num_chunks {
            node.set_next_on_line(synthetic_node_id(chunk_index as u64 + 1));
        }

        runs.push((synthetic_node_id(chunk_index as u64), node));
    }

    let anchor = a11y_text_position(
        char_index_for_byte(text, selection_tail),
        &synthetic_node_id,
    );
    let focus = a11y_text_position(
        char_index_for_byte(text, selection_head),
        &synthetic_node_id,
    );
    (runs, accesskit::TextSelection { anchor, focus })
}

#[cfg(test)]
mod a11y_text_run_tests {
    use super::build_a11y_text_runs;
    use crate::accesskit::NodeId;
    use proptest::strategy::Strategy;

    /// A strategy producing strings with a deliberate mix of character
    /// categories — ASCII, Latin accents, Cyrillic, Arabic, CJK, emoji, and
    /// arbitrary scalars — so run-splitting is exercised across scripts and
    /// byte widths (1–4 UTF-8 bytes). Lengths reach past one chunk (255 chars).
    fn arbitrary_text() -> impl Strategy<Value = String> {
        let character = proptest::prop_oneof![
            proptest::char::range(' ', '~'), // ASCII printable
            proptest::char::range('\u{00A1}', '\u{00FF}'), // Latin-1 (accents)
            proptest::char::range('\u{0100}', '\u{024F}'), // Latin Extended-A/B
            proptest::char::range('\u{0400}', '\u{04FF}'), // Cyrillic
            proptest::char::range('\u{0600}', '\u{06FF}'), // Arabic
            proptest::char::range('\u{4E00}', '\u{9FFF}'), // CJK Unified Ideographs
            proptest::char::range('\u{1F300}', '\u{1FAFF}'), // emoji & pictographs
            proptest::char::any(),           // anything else
        ];
        proptest::collection::vec(character, 0..600)
            .prop_map(|chars| chars.into_iter().collect::<String>())
    }

    /// Splitting an arbitrary string into AccessKit text runs must never panic,
    /// for any text and any byte selection offsets — including empty text, text
    /// spanning multiple chunks, multi-byte characters, and offsets past the end.
    #[crate::property_test]
    fn building_text_runs_never_panics(
        #[strategy = arbitrary_text()] text: String,
        selection_tail: usize,
        selection_head: usize,
    ) {
        let _ = build_a11y_text_runs(&text, selection_tail, selection_head, NodeId);
    }
}

#[cfg(test)]
mod activation_tests {
    use crate::{
        App, AppContext as _, Context, Entity, InteractiveElement, IntoElement, ParentElement,
        Render, StatefulInteractiveElement as _, StyleRefinement, TestAppContext, Window, div,
    };

    struct Child;
    impl Render for Child {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div()
                .id("cached-child")
                .role(crate::accesskit::Role::Button)
                .aria_label("Cached child")
        }
    }

    struct Root(Entity<Child>);
    impl Render for Root {
        fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
            div().child(self.0.clone().cached(StyleRefinement::default()))
        }
    }

    /// A screen reader usually attaches to a window that has already been
    /// drawn, so it meets a warm view cache. A cached view replays its recorded
    /// prepaint instead of running it again, and only a real prepaint pushes
    /// nodes — so the first frame after activation would otherwise report an
    /// empty tree for exactly the content the user is already looking at.
    #[crate::test]
    fn activating_mid_session_still_reports_cached_views(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, cx| Root(cx.new(|_| Child)));
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("the window is open");

        cx.activate_a11y(window.into());
        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the window is open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        assert!(
            tree["nodes"]
                .as_object()
                .expect("the dump lists nodes")
                .values()
                .any(|node| node["aria"]["label"].as_str() == Some("Cached child")),
            "a cached subtree must reach the tree on the first frame after activation: {json}"
        );
    }

    /// The tree is rebuilt from scratch every frame, so a cached view must
    /// keep contributing on frames where nothing about it changed. Otherwise
    /// any unrelated redraw — a blinking cursor, a hover — would delete the
    /// stable content from under the reader.
    #[crate::test]
    fn a_cached_view_keeps_reporting_on_later_frames(cx: &mut TestAppContext) {
        let window = cx.add_window(|_, cx| Root(cx.new(|_| Child)));
        cx.update_window(window.into(), |_, window, cx| {
            window.draw(cx).clear(cx);
        })
        .expect("the window is open");
        cx.activate_a11y(window.into());

        for frame in 1..=3 {
            let json = cx
                .update_window(window.into(), |_, window, cx| {
                    window.draw(cx).clear(cx);
                    window.debug_a11y_tree_json()
                })
                .expect("the window is open")
                .expect("activation makes the debug tree available");
            let tree: serde_json::Value =
                serde_json::from_str(&json).expect("the dump is valid JSON");
            assert!(
                tree["nodes"]
                    .as_object()
                    .expect("the dump lists nodes")
                    .values()
                    .any(|node| node["aria"]["label"].as_str() == Some("Cached child")),
                "the cached subtree vanished on frame {frame}: {json}"
            );
        }
    }

    /// Focus can point at an element that never rendered — a collapsed panel, a
    /// pane behind a zoomed one. The dump has to say so: a null focus with no
    /// reason is indistinguishable from having no focus at all.
    #[crate::test]
    fn focus_on_an_element_that_never_rendered_says_so(cx: &mut TestAppContext) {
        struct Hidden {
            focus_handle: crate::FocusHandle,
        }
        impl Render for Hidden {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                // Deliberately renders nothing that tracks the handle.
                div()
            }
        }

        let window = cx.add_window(|_, cx| Hidden {
            focus_handle: cx.focus_handle(),
        });
        let focus_handle = window
            .read_with(cx, |view, _| view.focus_handle.clone())
            .expect("the window is open");
        cx.activate_a11y(window.into());

        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.focus(&focus_handle, cx);
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the window is open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        assert_eq!(tree["gpui_focus"].as_str(), None);
        assert_eq!(
            tree["frame"]["focus_without_node"].as_str(),
            Some("its element was not rendered this frame"),
            "a null focus has to come with the reason for it"
        );
    }

    /// A synthetic child that occupies a place on screen has to be operable,
    /// not merely announced. GPUI answers `Action::Click` by synthesizing a
    /// press at the node's registered bounds, and only real elements used to
    /// register any — so a text run standing in for a link inside a paragraph
    /// could be read out and never followed.
    #[crate::test]
    fn a_synthetic_child_with_bounds_can_be_clicked(cx: &mut TestAppContext) {
        struct LinkInText;

        impl crate::Element for LinkInText {
            type RequestLayoutState = ();
            type PrepaintState = ();

            fn id(&self) -> Option<crate::ElementId> {
                Some(crate::ElementId::Name("paragraph".into()))
            }

            fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
                None
            }

            fn request_layout(
                &mut self,
                _: Option<&crate::GlobalElementId>,
                _: Option<&crate::InspectorElementId>,
                window: &mut Window,
                cx: &mut App,
            ) -> (crate::LayoutId, ()) {
                let mut style = crate::Style::default();
                style.size.width = crate::px(400.).into();
                style.size.height = crate::px(200.).into();
                (window.request_layout(style, [], cx), ())
            }

            fn prepaint(
                &mut self,
                _: Option<&crate::GlobalElementId>,
                _: Option<&crate::InspectorElementId>,
                _: crate::Bounds<crate::Pixels>,
                _: &mut (),
                _: &mut Window,
                _: &mut App,
            ) {
            }

            fn paint(
                &mut self,
                _: Option<&crate::GlobalElementId>,
                _: Option<&crate::InspectorElementId>,
                _: crate::Bounds<crate::Pixels>,
                _: &mut (),
                _: &mut (),
                _: &mut Window,
                _: &mut App,
            ) {
            }

            fn a11y_role(&self) -> Option<accesskit::Role> {
                Some(accesskit::Role::Group)
            }

            fn a11y_synthetic_children(
                &mut self,
                _: &mut (),
                builder: &mut crate::A11ySubtreeBuilder,
            ) {
                let mut node = accesskit::Node::new(accesskit::Role::Link);
                node.set_label("example.com");
                node.add_action(accesskit::Action::Click);
                builder.push_child_with_bounds(
                    builder.synthetic_node_id(0),
                    node,
                    crate::Bounds {
                        origin: crate::point(crate::px(100.), crate::px(50.)),
                        size: crate::size(crate::px(60.), crate::px(20.)),
                    },
                );
            }
        }

        impl IntoElement for LinkInText {
            type Element = Self;

            fn into_element(self) -> Self {
                self
            }
        }

        let pressed: std::rc::Rc<std::cell::Cell<Option<crate::Point<crate::Pixels>>>> =
            Default::default();

        struct Host(std::rc::Rc<std::cell::Cell<Option<crate::Point<crate::Pixels>>>>);
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                let pressed = self.0.clone();
                div()
                    .id("page")
                    .on_mouse_down(crate::MouseButton::Left, move |event, _, _| {
                        pressed.set(Some(event.position));
                    })
                    .child(LinkInText)
            }
        }

        let window = cx.add_window(|_, _| Host(pressed.clone()));
        cx.activate_a11y(window.into());
        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the window is open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        let link = tree["nodes"]
            .as_object()
            .expect("the dump lists nodes")
            .values()
            .find(|node| node["aria"]["role"] == "Link")
            .unwrap_or_else(|| panic!("the synthetic link reaches the tree: {json}"));
        let link_id = accesskit::NodeId(
            link["accesskit_id"]
                .as_str()
                .expect("the node carries an accesskit id")
                .parse()
                .expect("the id is a u64"),
        );

        cx.update_window(window.into(), |_, window, cx| {
            window.handle_a11y_action(
                accesskit::ActionRequest {
                    target_tree: accesskit::TreeId::ROOT,
                    target_node: link_id,
                    action: accesskit::Action::Click,
                    data: None,
                },
                cx,
            );
        })
        .expect("the window is open");

        let position = pressed
            .get()
            .expect("clicking the link has to reach the page as a press");
        // The centre of the bounds the element asked for, which is the point
        // that lands inside the link rather than beside it.
        assert_eq!(position.x, crate::px(130.));
        assert_eq!(position.y, crate::px(60.));
    }

    /// A custom element can return a role while returning no id — the role is
    /// then discarded with no node, no warning, and no visible difference in
    /// the code that asked for it. That is the quietest way to lose a node, so
    /// the dump has to name the site.
    #[crate::test]
    fn a_role_without_an_element_id_is_reported(cx: &mut TestAppContext) {
        struct RolefulButIdless;

        impl crate::Element for RolefulButIdless {
            type RequestLayoutState = ();
            type PrepaintState = ();

            fn id(&self) -> Option<crate::ElementId> {
                None
            }

            fn source_location(&self) -> Option<&'static std::panic::Location<'static>> {
                None
            }

            fn request_layout(
                &mut self,
                _: Option<&crate::GlobalElementId>,
                _: Option<&crate::InspectorElementId>,
                window: &mut Window,
                cx: &mut App,
            ) -> (crate::LayoutId, ()) {
                (window.request_layout(crate::Style::default(), [], cx), ())
            }

            fn prepaint(
                &mut self,
                _: Option<&crate::GlobalElementId>,
                _: Option<&crate::InspectorElementId>,
                _: crate::Bounds<crate::Pixels>,
                _: &mut (),
                _: &mut Window,
                _: &mut App,
            ) {
            }

            fn paint(
                &mut self,
                _: Option<&crate::GlobalElementId>,
                _: Option<&crate::InspectorElementId>,
                _: crate::Bounds<crate::Pixels>,
                _: &mut (),
                _: &mut (),
                _: &mut Window,
                _: &mut App,
            ) {
            }

            fn a11y_role(&self) -> Option<accesskit::Role> {
                Some(accesskit::Role::Button)
            }
        }

        impl IntoElement for RolefulButIdless {
            type Element = Self;

            fn into_element(self) -> Self {
                self
            }
        }

        struct Host;
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div().child(RolefulButIdless)
            }
        }

        let window = cx.add_window(|_, _| Host);
        cx.activate_a11y(window.into());
        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the window is open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        let discarded = tree["frame"]["roles_without_id"]
            .as_array()
            .expect("the dump lists discarded roles");
        assert_eq!(
            discarded.len(),
            1,
            "the discarded role must be named, not silently dropped: {json}"
        );
        assert!(
            discarded[0].as_str().is_some_and(|site| site.contains("Button")),
            "the report has to say which role was lost: {discarded:?}"
        );
    }

    /// The other half of the same trap. A node needs an id *and* a role, and
    /// an element with an id and no role builds nothing, so a name or a live
    /// region set on it is dropped as quietly as a role without an id — with
    /// the call site looking exactly like one that worked.
    #[gpui::test]
    fn aria_on_an_element_with_no_role_is_reported(cx: &mut TestAppContext) {
        struct Host;
        impl Render for Host {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .id("named-but-roleless")
                    .aria_label("Save")
                    // A sibling doing it right, so the report cannot be passing
                    // by flagging everything.
                    .child(div().id("proper").role(accesskit::Role::Button).aria_label("Cancel"))
            }
        }

        let window = cx.add_window(|_, _| Host);
        cx.activate_a11y(window.into());
        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the window is open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        let discarded = tree["frame"]["aria_without_role"]
            .as_array()
            .expect("the dump lists discarded accessibility information");
        assert_eq!(
            discarded.len(),
            1,
            "the labelled element with no role is the only one reported: {json}"
        );
        assert!(
            discarded[0]
                .as_str()
                .is_some_and(|site| site.contains("a11y.rs")),
            "the report has to say where the information was lost: {discarded:?}"
        );
    }

    /// A control whose centre is covered by a smaller clickable child is the
    /// shape that makes an advertised `Click` land somewhere else — a close
    /// button inside a tab is the real case. Proven end to end so the check
    /// cannot quietly become vacuous.
    #[crate::test]
    #[should_panic(expected = "would click")]
    fn a_click_target_covered_by_its_own_child_is_reported(cx: &mut TestAppContext) {
        use crate::Styled as _;

        struct Nested;
        impl Render for Nested {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .id("outer")
                    .role(accesskit::Role::Button)
                    .aria_label("Outer")
                    .w(crate::px(100.0))
                    .h(crate::px(100.0))
                    .flex()
                    .items_center()
                    .justify_center()
                    .on_click(|_, _, _| {})
                    .child(
                        div()
                            .id("inner")
                            .role(accesskit::Role::Button)
                            .aria_label("Inner")
                            .w(crate::px(40.0))
                            .h(crate::px(40.0))
                            .on_click(|_, _, _| {}),
                    )
            }
        }

        let window = cx.add_window(|_, _| Nested);
        cx.activate_a11y(window.into());
        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the window is open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        crate::test::a11y_checks::assert_click_targets_are_reachable(&tree, "nested click targets");
    }

    /// Two landmarks of the same kind with the same name — or none — offer a
    /// reader destinations and then refuse to say what they are.
    #[crate::test]
    #[should_panic(expected = "cannot be told apart")]
    fn landmarks_that_cannot_be_told_apart_are_reported(cx: &mut TestAppContext) {
        struct TwoPanels;
        impl Render for TwoPanels {
            fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
                div()
                    .child(div().id("left").role(accesskit::Role::Complementary))
                    .child(div().id("right").role(accesskit::Role::Complementary))
            }
        }

        let window = cx.add_window(|_, _| TwoPanels);
        cx.activate_a11y(window.into());
        let json = cx
            .update_window(window.into(), |_, window, cx| {
                window.draw(cx).clear(cx);
                window.debug_a11y_tree_json()
            })
            .expect("the window is open")
            .expect("activation makes the debug tree available");
        let tree: serde_json::Value = serde_json::from_str(&json).expect("the dump is valid JSON");

        crate::test::a11y_checks::assert_landmarks_are_distinguishable(&tree, "two panels");
        crate::test::a11y_checks::assert_names_are_distinguishable(&tree, "two panels");
        crate::test::a11y_checks::assert_focusable_names_are_distinguishable(&tree, "two panels");
        crate::test::a11y_checks::assert_clickable_elements_are_reachable(&tree, "two panels");
        crate::test::a11y_checks::assert_no_role_was_discarded(&tree, "two panels");
        crate::test::a11y_checks::assert_no_aria_was_discarded(&tree, "two panels");
        crate::test::a11y_checks::assert_roles_are_contained(&tree, "two panels");
        crate::test::a11y_checks::assert_controls_have_area(&tree, "two panels");
        crate::test::a11y_checks::assert_active_descendant_is_honoured(&tree, "two panels");
    }
}
