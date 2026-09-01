//! Headless screenshot + accessibility regression for z3rm's own UI.
//!
//! Two things are verified per scenario from a single rendered frame:
//!
//! 1. **Pixels** — the frame is rendered on a real GPU through
//!    [`gpui::HeadlessAppContext`] + `gpui_platform::current_headless_renderer`,
//!    then checked for *structural* properties (correct raster size, the frame
//!    is not blank, an expected accent color actually reaches the framebuffer).
//!    Exact per-pixel baselines are deliberately avoided: glyph rasterization
//!    differs across macOS versions and GPUs, so a byte-comparison baseline
//!    would be red on every machine but the one that recorded it. Every frame
//!    is still written to `target/ui_screenshots/` for human inspection.
//!
//! 2. **Accessibility tree** — `Z3RM_A11Y_BUILD_HEADLESS=1` activates the
//!    in-memory AccessKit builder so `Window::debug_a11y_tree_json` returns the
//!    frame's tree. This is the stable, machine-checkable answer to "was the
//!    element actually rendered", and it is what the assertions lean on.
//!
//! Run with:
//!
//! ```sh
//! cargo test -p z3rm --test ui_screenshot_regression --features gpui_platform/runtime_shaders
//! ```
//!
//! See `docs/development/ui-regression-testing.md`.

#![cfg(all(unix, any(target_os = "macos", target_os = "linux")))]

use anyhow::{Context as _, Result};
use assets::Assets;
use extension_host::vdom_bridge::{DrawOp, VDomNode, VDomPalette, VDomRenderer};
use extension_host::vdom_bridge::CommandInvocation;
use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, HeadlessAppContext, IntoElement,
    ParentElement as _, Render, Styled as _, WeakEntity, Window, WindowHandle, div, px, size,
};
use image::RgbaImage;
use mux::MuxDomain;
use mux_protocol::{
    Cell, CellStyle, CursorState, Envelope, FetchGridUpdateResponse, FetchScrollbackResponse,
    FullGridSnapshot, Request, Response, RowChange, envelope::Payload as EnvelopePayload,
    fetch_grid_update_response::Update as FetchUpdate, request::Body as RequestBody,
    response::Body as ResponseBody,
};
use settings::SettingsStore;
use std::io::{ErrorKind, Read as _, Write as _};
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};
use terminal_view::mux_pane::MuxPaneView;
use workspace::notifications::simple_message_notification::MessageNotification;

// ============================================================================
// Harness
// ============================================================================

/// How long a scenario waits for asynchronous state (a mux fetch round trip) to
/// land in the rendered frame. Real socket I/O on a real thread is involved, so
/// this is wall-clock rather than simulated time.
const CONVERGE_TIMEOUT: Duration = Duration::from_secs(15);

/// Process-global setup that must happen before any window is created.
///
/// `Z3RM_A11Y_BUILD_HEADLESS` is read by `TestWindow::a11y_init`, and
/// `Z3RM_STATELESS` keeps settings/db initialization away from the developer's
/// real config directories. Both are set exactly once and never unset, so
/// parallel test threads observe a stable value.
fn init_process_env() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        // SAFETY: run exactly once, before this binary opens any window or
        // spawns any thread that reads these variables.
        unsafe {
            std::env::set_var("Z3RM_A11Y_BUILD_HEADLESS", "1");
            std::env::set_var("Z3RM_STATELESS", "1");
        }
    });
}

/// Build a headless app with the shipping platform text system and embedded
/// fonts. macOS uses the Metal renderer; Linux uses the deterministic software
/// renderer so screenshots are reproducible without a display server.
fn headless_app() -> Result<HeadlessAppContext> {
    init_process_env();

    // The platform is constructed only to borrow its text system; the app
    // itself runs on `TestPlatform` for deterministic scheduling.
    let platform = gpui_platform::current_platform(true);
    let text_system = platform.text_system();

    let mut cx = HeadlessAppContext::with_platform(text_system, Arc::new(Assets), || {
        gpui_platform::current_headless_renderer()
    });

    cx.update(|cx| -> Result<()> {
        Assets.load_fonts(cx).context("load embedded fonts")?;
        let settings_store = SettingsStore::test(cx);
        cx.set_global(settings_store);
        theme_settings::init(theme::LoadThemes::JustBase, cx);
        Ok(())
    })?;

    Ok(cx)
}

/// Draw one frame and return both artifacts produced by it.
fn draw_frame(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
) -> Result<(RgbaImage, serde_json::Value)> {
    let a11y_json = cx
        .update_window(window, |_, window, cx| {
            window.draw(cx).clear(cx);
            window.debug_a11y_tree_json()
        })?
        .context(
            "debug_a11y_tree_json returned None; Z3RM_A11Y_BUILD_HEADLESS must be set before \
             the window is opened",
        )?;
    let tree: serde_json::Value =
        serde_json::from_str(&a11y_json).context("a11y tree must be valid JSON")?;
    let image = cx
        .capture_screenshot(window)
        .context("capture screenshot")?;
    Ok((image, tree))
}

fn screenshot_dir() -> PathBuf {
    let manifest = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    manifest.join("../../target/ui_screenshots")
}

fn save_screenshot(name: &str, image: &RgbaImage) -> Result<PathBuf> {
    let dir = screenshot_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{name}.png"));
    image
        .save(&path)
        .with_context(|| format!("write {}", path.display()))?;
    eprintln!("screenshot: {}", path.display());
    Ok(path)
}

/// Persist the a11y dump next to the screenshot. The pair is what a human
/// needs to judge an intentional UI change.
fn save_a11y_tree(name: &str, tree: &serde_json::Value) -> Result<PathBuf> {
    let dir = screenshot_dir();
    std::fs::create_dir_all(&dir).with_context(|| format!("create {}", dir.display()))?;
    let path = dir.join(format!("{name}.a11y.json"));
    std::fs::write(&path, serde_json::to_string_pretty(tree)?)
        .with_context(|| format!("write {}", path.display()))?;
    eprintln!("a11y tree: {}", path.display());
    Ok(path)
}

/// Write both artifacts for a scenario.
/// The checks every captured frame has to pass, whether or not it is saved.
fn check_a11y(tree: &serde_json::Value, name: &str) {
    gpui::a11y_checks::assert_interactive_nodes_are_named(tree, name);
    gpui::a11y_checks::assert_roles_are_contained(tree, name);
    gpui::a11y_checks::assert_no_role_was_discarded(tree, name);
    gpui::a11y_checks::assert_click_targets_are_reachable(tree, name);
    gpui::a11y_checks::assert_focus_reached_the_tree(tree, name);
    gpui::a11y_checks::assert_landmarks_are_distinguishable(tree, name);
    gpui::a11y_checks::assert_names_are_distinguishable(tree, name);
    gpui::a11y_checks::assert_clickable_elements_are_reachable(tree, name);
    gpui::a11y_checks::assert_controls_have_area(tree, name);
    gpui::a11y_checks::assert_active_descendant_is_honoured(tree, name);
}

fn save_frame(name: &str, image: &RgbaImage, tree: &serde_json::Value) -> Result<()> {
    save_screenshot(name, image)?;
    save_a11y_tree(name, tree)?;
    // Checked here rather than per scenario so a new scenario cannot forget it.
    check_a11y(tree, name);
    Ok(())
}

fn image_digest(image: &RgbaImage) -> u64 {
    let mut digest = 0xcbf29ce484222325u64;
    for byte in image.as_raw() {
        digest ^= u64::from(*byte);
        digest = digest.wrapping_mul(0x100000001b3);
    }
    digest
}

/// Number of distinct RGB triples in the frame. A blank or single-fill frame
/// collapses to 1-2, which is the failure mode this guards against.
fn distinct_colors(image: &RgbaImage) -> usize {
    let mut seen = std::collections::HashSet::new();
    for pixel in image.pixels() {
        seen.insert((pixel.0[0], pixel.0[1], pixel.0[2]));
    }
    seen.len()
}

/// Count pixels within `tolerance` of `rgb` on every channel.
fn count_near_color(image: &RgbaImage, rgb: [u8; 3], tolerance: u8) -> usize {
    image
        .pixels()
        .filter(|pixel| (0..3).all(|channel| pixel.0[channel].abs_diff(rgb[channel]) <= tolerance))
        .count()
}

/// All nodes in a `debug_a11y_tree_json` dump, as `(role, node)` pairs.
fn a11y_nodes(tree: &serde_json::Value) -> Vec<(String, &serde_json::Value)> {
    tree.get("nodes")
        .and_then(|nodes| nodes.as_object())
        .map(|nodes| {
            nodes
                .values()
                .map(|node| {
                    let role = node
                        .get("aria")
                        .and_then(|aria| aria.get("role"))
                        .and_then(|role| role.as_str())
                        .unwrap_or_default()
                        .to_string();
                    (role, node)
                })
                .collect()
        })
        .unwrap_or_default()
}

fn a11y_nodes_with_role<'a>(tree: &'a serde_json::Value, role: &str) -> Vec<&'a serde_json::Value> {
    a11y_nodes(tree)
        .into_iter()
        .filter(|(node_role, _)| node_role == role)
        .map(|(_, node)| node)
        .collect()
}

fn a11y_string_field(node: &serde_json::Value, field: &str) -> Option<String> {
    node.get("aria")
        .and_then(|aria| aria.get(field))
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

/// Every `Role::TextRun` value in the tree, in dump order.
fn a11y_text_run_values(tree: &serde_json::Value) -> Vec<String> {
    a11y_nodes_with_role(tree, "TextRun")
        .into_iter()
        .filter_map(|node| a11y_string_field(node, "value"))
        .collect()
}

/// Roles present in the frame, sorted, for diagnostics in failure messages.
fn a11y_role_summary(tree: &serde_json::Value) -> Vec<String> {
    let mut roles: Vec<String> = a11y_nodes(tree)
        .into_iter()
        .map(|(role, _)| role)
        .filter(|role| !role.is_empty())
        .collect();
    roles.sort();
    roles.dedup();
    roles
}

/// Pump the GPUI scheduler and redraw until `converged` observes the frame it
/// is waiting for, or `CONVERGE_TIMEOUT` elapses.
///
/// The wait is wall-clock because the state being waited on is produced by a
/// real background thread doing real socket I/O; `advance_clock` cannot make
/// that thread run faster. `run_until_parked` alone returns immediately when
/// the response has not arrived yet.
fn draw_until(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
    converged: impl Fn(&serde_json::Value) -> bool,
) -> Result<(RgbaImage, serde_json::Value)> {
    let deadline = Instant::now() + CONVERGE_TIMEOUT;
    loop {
        cx.run_until_parked();
        let (image, tree) = draw_frame(cx, window)?;
        if converged(&tree) {
            return Ok((image, tree));
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "frame never converged within {:?}; roles seen: {:?}",
                CONVERGE_TIMEOUT,
                a11y_role_summary(&tree)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}


/// Like [`draw_until`], but the convergence test looks at pixels.
///
/// The framebuffer is the ground truth for the async grid-update path: the
/// updated grid's accent row only reaches the screen after the dirty signal
/// triggered a follow-up fetch, so the frame that converges is the frame that
/// proves the pull happened.
fn draw_until_pixels(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
    converged: impl Fn(&RgbaImage) -> bool,
) -> Result<(RgbaImage, serde_json::Value)> {
    let deadline = Instant::now() + CONVERGE_TIMEOUT;
    loop {
        cx.run_until_parked();
        // MuxPaneView coalesces the PaneOutput dirty signal behind a background
        // timer; without advancing the clock that timer never fires and the
        // follow-up grid fetch is never scheduled.
        cx.advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        let (image, tree) = draw_frame(cx, window)?;
        if converged(&image) {
            return Ok((image, tree));
        }
        if Instant::now() >= deadline {
            anyhow::bail!(
                "frame never converged within {:?}; distinct colors: {}, roles seen: {:?}",
                CONVERGE_TIMEOUT,
                distinct_colors(&image),
                a11y_role_summary(&tree)
            );
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

// ============================================================================
// Mock mux server (§3.3 grid sync)
// ============================================================================

/// The grid the mock server serves. Rows are rendered left-aligned and padded
/// with spaces; `accent_rows` get an explicit cell background so the frame has
/// a color that can be located in the framebuffer independently of glyph
/// rasterization.
struct MockGrid {
    cols: u32,
    rows: u32,
    lines: Vec<String>,
    accent_row: u32,
    accent_background: u32,
    accent_foreground: u32,
    generation: u64,
    history: Vec<String>,
    history_version: u64,
}

impl MockGrid {
    fn snapshot(&self) -> FullGridSnapshot {
        let mut cells = Vec::with_capacity((self.cols * self.rows) as usize);
        for row in 0..self.rows {
            let line: Vec<char> = self
                .lines
                .get(row as usize)
                .map(|line| line.chars().collect())
                .unwrap_or_default();
            for col in 0..self.cols {
                let character = line.get(col as usize).copied().unwrap_or(' ');
                let accent = row == self.accent_row;
                cells.push(Cell {
                    char: character.to_string(),
                    style: accent.then(|| CellStyle {
                        bold: true,
                        ..Default::default()
                    }),
                    foreground: if accent {
                        self.accent_foreground
                    } else {
                        0xd0d0d0
                    },
                    background: if accent { self.accent_background } else { 0 },
                    zerowidth: String::new(),
                    hyperlink: None,
                });
            }
        }

        FullGridSnapshot {
            cols: self.cols,
            rows: self.rows,
            cells,
            cursor: Some(CursorState {
                col: 4,
                row: 1,
                // 1 = block cursor
                style: 1,
                visible: true,
                blinking: false,
            }),
            alternate_screen: false,
            display_offset: 0,
            history_size: u32::try_from(self.history.len()).expect("history fits u32"),
            history_version: self.history_version,
            modes: None,
        }
    }
}

/// Serve mux requests on `stream` until the peer disconnects or `stop` is set.
///
/// `FetchGridUpdate` returns the current full snapshot when the caller is stale
/// and `NoChange` at the current generation, including the history-checkpoint
/// confirmation fetch. Scrollback requests page through the matching history
/// fixture. Every other request gets an empty success response so nothing the
/// view does at startup is left hanging.
fn serve_mock_mux(
    stream: UnixStream,
    grid: MockGrid,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    serve_mock_mux_with_output(stream, grid, None, Vec::new(), stop)
}

/// Same as [`serve_mock_mux`], plus the post-fetch story that exercises the
/// §3.1 push-signal / §3.3 pull-data contract: one nonempty `PaneOutputChunk`
/// (sequence 1) is pushed right after the first grid fetch, and `updated_grid`
/// — when given — becomes the authoritative full grid for a stale follow-up
/// fetch, the way a real server reports its post-output state.
///
/// PaneOutput is a lossy dirty signal only: the client never parses the byte
/// payload, so the mock never has to make that payload meaningful. The renderer
/// changes exclusively through the structured grid snapshot path.
fn serve_mock_mux_with_output(
    mut stream: UnixStream,
    grid: MockGrid,
    updated_grid: Option<MockGrid>,
    pane_output: Vec<u8>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| format!("set mock mux read timeout: {error}"))?;

    let snapshot = grid.snapshot();
    let updated = updated_grid.as_ref().map(MockGrid::snapshot);
    let updated_generation = updated_grid.as_ref().map_or(0, |grid| grid.generation);
    let mut fetches_served = 0u64;
    let mut pane_output_sent = false;
    let mut buffered: Vec<u8> = Vec::new();
    let mut chunk = [0u8; 8192];

    while !stop.load(Ordering::SeqCst) {
        match stream.read(&mut chunk) {
            Ok(0) => return Ok(()),
            Ok(read) => buffered.extend_from_slice(&chunk[..read]),
            Err(error)
                if matches!(
                    error.kind(),
                    ErrorKind::WouldBlock | ErrorKind::TimedOut | ErrorKind::Interrupted
                ) =>
            {
                continue;
            }
            Err(error) => return Err(format!("mock mux read: {error}")),
        }

        while let Some(envelope) = take_frame(&mut buffered)? {
            let Some(EnvelopePayload::Request(request)) = envelope.payload else {
                continue;
            };
            // The real server pushes PaneOutput to every attached client; it
            // does not wait for a SubscribePaneOutput request, and the client
            // never sends one. The first grid fetch is the point where the view
            // is known to be listening.
            let is_grid_fetch = matches!(&request.body, Some(RequestBody::FetchGridUpdate(_)));
            // The first fetch is answered with the initial grid (generation 9,
            // output fence 0 — no chunk is part of that state yet). The stale
            // follow-up fetch triggered by the chunk is answered with the
            // authoritative updated grid (generation 10, output fence 1).
            let (served_snapshot, served_history, served_generation, served_fence) =
                match (&updated, updated_grid.as_ref(), fetches_served) {
                    (Some(_), Some(_), 0) => (&snapshot, grid.history.as_slice(), grid.generation, 0),
                    (Some(updated), Some(updated_grid), _) => (
                        updated,
                        updated_grid.history.as_slice(),
                        updated_generation,
                        1,
                    ),
                    _ => (&snapshot, grid.history.as_slice(), grid.generation, 0),
                };
            let response = mock_response(
                &request,
                served_snapshot,
                served_history,
                served_generation,
                served_fence,
            );
            let bytes = mux_protocol::frame(&Envelope {
                version: Some(mux_protocol::PROTOCOL_VERSION),
                payload: Some(EnvelopePayload::Response(response)),
            })
            .map_err(|error| format!("encode mock mux response: {error}"))?;
            if let Err(error) = stream.write_all(&bytes) {
                // The client hanging up mid-write is a normal shutdown race.
                if error.kind() == ErrorKind::BrokenPipe {
                    return Ok(());
                }
                return Err(format!("mock mux write: {error}"));
            }

            if is_grid_fetch {
                fetches_served += 1;
            }

            if is_grid_fetch && fetches_served == 1 && !pane_output_sent && !pane_output.is_empty() {
                pane_output_sent = true;
                let notification = Envelope {
                    version: Some(mux_protocol::PROTOCOL_VERSION),
                    payload: Some(EnvelopePayload::Notification(
                        mux_protocol::Notification {
                            event: Some(mux_protocol::notification::Event::PaneOutput(
                                mux_protocol::PaneOutputChunk {
                                    pane_id: MOCK_PANE_ID.to_string(),
                                    data: pane_output.clone(),
                                    // First emitted chunk for this pane, so the
                                    // per-pane monotonic sequence starts at 1.
                                    output_sequence: 1,
                                },
                            )),
                        },
                    )),
                };
                let bytes = mux_protocol::frame(&notification)
                    .map_err(|error| format!("encode mock pane output: {error}"))?;
                if let Err(error) = stream.write_all(&bytes) {
                    if error.kind() == ErrorKind::BrokenPipe {
                        return Ok(());
                    }
                    return Err(format!("mock mux write: {error}"));
                }
            }
        }
    }
    Ok(())
}

fn mock_response(
    request: &Request,
    snapshot: &FullGridSnapshot,
    history: &[String],
    generation: u64,
    output_sequence: u64,
) -> Response {
    let body = match &request.body {
        Some(RequestBody::FetchGridUpdate(fetch)) => {
            Some(ResponseBody::GridUpdate(FetchGridUpdateResponse {
                from_generation: fetch.since_generation,
                to_generation: generation,
                // Highest PaneOutputChunk.output_sequence incorporated into the
                // returned grid state: 0 before any chunk was emitted, 1 once
                // the pane's first chunk is part of the state.
                output_sequence,
                update: (fetch.since_generation != generation)
                    .then(|| FetchUpdate::FullSnapshot(snapshot.clone())),
            }))
        }
        Some(RequestBody::FetchScrollback(fetch)) => {
            let from = fetch.from_line as usize;
            let count = fetch.count as usize;
            let indices = if history.is_empty() || count == 0 || from >= history.len() {
                0..0
            } else if fetch.direction == 0 {
                from.saturating_sub(count.saturating_sub(1))..from.saturating_add(1)
            } else {
                from..from.saturating_add(count).min(history.len())
            };
            let lines = indices
                .map(|row| mock_history_row(row, &history[row], snapshot.cols))
                .collect();
            Some(ResponseBody::Scrollback(FetchScrollbackResponse {
                lines,
                total_lines: u32::try_from(history.len()).unwrap_or(u32::MAX),
                scrollback_version: snapshot.history_version,
            }))
        }
        // §3.3 The OSC 133 markers the jump navigates by. Two commands, both
        // in history, so a jump has somewhere to land and a second one has a
        // boundary to stop at.
        Some(RequestBody::ListCommands(_)) => Some(ResponseBody::Commands(
            mux_protocol::ListCommandsResponse {
                commands: vec![
                    mock_command(1, -4, Some(0)),
                    mock_command(2, -2, Some(1)),
                ],
                history_size: u32::try_from(history.len()).unwrap_or(u32::MAX),
                recorded_markers: 2,
            },
        )),
        // Empty body = success. Anything the view issues during startup that is
        // not answered would leave a task waiting forever.
        _ => None,
    };
    Response {
        request_id: request.request_id,
        body,
    }
}

/// One OSC 133 command: a prompt at `prompt_line` and an exit status.
fn mock_command(id: u64, prompt_line: i64, exit_code: Option<i32>) -> mux_protocol::CommandRange {
    mux_protocol::CommandRange {
        id,
        prompt: Some(mux_protocol::CommandMarker {
            line: Some(prompt_line),
            column: 0,
        }),
        command_end: Some(mux_protocol::CommandMarker {
            line: Some(prompt_line + 1),
            column: 0,
        }),
        exit_code,
        ..Default::default()
    }
}

fn mock_history_row(row: usize, text: &str, cols: u32) -> RowChange {
    let cells = text
        .chars()
        .chain(std::iter::repeat(' '))
        .take(cols as usize)
        .map(|character| Cell {
            char: character.to_string(),
            foreground: 0xd0d0d0,
            ..Default::default()
        })
        .collect();
    RowChange {
        row: u32::try_from(row).unwrap_or(u32::MAX),
        cells,
    }
}

/// Pull one complete frame out of `buffered`, if one is fully buffered.
fn take_frame(buffered: &mut Vec<u8>) -> Result<Option<Envelope>, String> {
    let Some((raw_len, prefix_len)) = mux_protocol::parse_len_prefix(buffered)
        .map_err(|error| format!("parse mock mux frame prefix: {error}"))?
    else {
        return Ok(None);
    };
    let payload_len = mux_protocol::check_frame_len(raw_len)
        .map_err(|error| format!("validate mock mux frame length: {error}"))?;
    let frame_len = prefix_len + payload_len;
    if buffered.len() < frame_len {
        return Ok(None);
    }
    let (envelope, consumed) = mux_protocol::unframe(&buffered[..frame_len])
        .map_err(|error| format!("decode mock mux frame: {error}"))?;
    buffered.drain(..consumed);
    Ok(Some(envelope))
}

/// Owns the mock server thread and shuts it down on drop so a failing
/// assertion never leaks a thread into the rest of the test binary.
struct MockMuxServer {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<Result<(), String>>>,
}

impl MockMuxServer {
    fn start(grid: MockGrid) -> Result<(Arc<MuxDomain>, Self)> {
        Self::start_with_output(grid, None, Vec::new())
    }

    /// Like [`Self::start`], plus the optional post-fetch story: an updated
    /// authoritative grid served on follow-up fetches and a `PaneOutputChunk`
    /// pushed after the first fetch. See [`serve_mock_mux_with_output`].
    fn start_with_output(
        grid: MockGrid,
        updated_grid: Option<MockGrid>,
        pane_output: Vec<u8>,
    ) -> Result<(Arc<MuxDomain>, Self)> {
        let (client, server) = UnixStream::pair().context("create mux socket pair")?;
        client
            .set_nonblocking(true)
            .context("set mux client nonblocking")?;
        let domain = Arc::new(
            MuxDomain::connect_with_blocking_stream(client).context("connect mock mux domain")?,
        );
        let stop = Arc::new(AtomicBool::new(false));
        let thread = std::thread::spawn({
            let stop = stop.clone();
            move || serve_mock_mux_with_output(server, grid, updated_grid, pane_output, stop)
        });
        Ok((
            domain,
            Self {
                stop,
                thread: Some(thread),
            },
        ))
    }
}

impl Drop for MockMuxServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(thread) = self.thread.take() {
            match thread.join() {
                Ok(Ok(())) => {}
                Ok(Err(error)) => eprintln!("mock mux server error: {error}"),
                Err(_) => eprintln!("mock mux server panicked"),
            }
        }
    }
}

// ============================================================================
// §3.3 / §16.4 terminal pane
// ============================================================================

const TERMINAL_MARKER: &str = "Z3RM-HEADLESS-GRID";

/// Pane id shared by the mock server and the view under test; a `PaneOutputChunk`
/// addressed to any other pane is ignored by the client.
const MOCK_PANE_ID: &str = "headless-pane";
const TERMINAL_ACCENT_BG: u32 = 0x1e6fd9;
const TERMINAL_ACCENT_FG: u32 = 0xffe680;

fn terminal_grid() -> MockGrid {
    MockGrid {
        cols: 60,
        rows: 12,
        lines: vec![
            format!("{TERMINAL_MARKER} row0"),
            "second line with cursor".to_string(),
            "third line 0123456789".to_string(),
            String::new(),
            "tail line".to_string(),
        ],
        accent_row: 2,
        accent_background: TERMINAL_ACCENT_BG,
        accent_foreground: TERMINAL_ACCENT_FG,
        generation: 9,
        history: Vec::new(),
        history_version: 0,
    }
}

fn terminal_grid_with_history() -> MockGrid {
    // The headless window is taller than a 12-row grid, and alacritty pulls
    // history into the screen when the viewport grows, which would mask the
    // semantic scroll. Serve more rows than the window can display so the
    // history stays above the viewport until an action scrolls to it.
    let mut lines = vec![format!("{TERMINAL_MARKER} row0")];
    for row in 1..24 {
        lines.push(format!("active row {row:02}"));
    }
    MockGrid {
        cols: 60,
        rows: 24,
        lines,
        accent_row: 2,
        accent_background: TERMINAL_ACCENT_BG,
        accent_foreground: TERMINAL_ACCENT_FG,
        generation: 9,
        history: vec![
            "HIST-0 oldest".to_string(),
            "HIST-1".to_string(),
            "HIST-2".to_string(),
            "HIST-3 newest".to_string(),
        ],
        history_version: 7,
    }
}

fn open_mux_pane(
    cx: &mut HeadlessAppContext,
    domain: Arc<MuxDomain>,
) -> Result<WindowHandle<MuxPaneView>> {
    cx.open_window(size(px(720.0), px(320.0)), |window, cx| {
        cx.new(|cx| {
            MuxPaneView::new(
                MOCK_PANE_ID.to_string(),
                domain,
                WeakEntity::new_invalid(),
                WeakEntity::new_invalid(),
                window,
                cx,
            )
        })
    })
}

fn mux_pane_renders_terminal_grid_and_exposes_a11y_tree() -> Result<()> {
    let mut cx = headless_app()?;
    let (domain, _server) = MockMuxServer::start(terminal_grid())?;
    cx.allow_parking();

    let window = open_mux_pane(&mut cx, domain)?;
    let (image, tree) = draw_until(&mut cx, window.into(), |tree| {
        a11y_text_run_values(tree)
            .iter()
            .any(|value| value.contains(TERMINAL_MARKER))
    })?;
    save_frame("mux_pane_terminal_grid", &image, &tree)?;

    #[cfg(target_os = "linux")]
    assert_eq!(
        image_digest(&image),
        0x9295b034f2e44153,
        "Linux mux screenshot baseline changed; inspect target/ui_screenshots/mux_pane_terminal_grid.png"
    );

    // --- a11y structure (§16.4) ---
    let terminals = a11y_nodes_with_role(&tree, "Terminal");
    assert!(
        !terminals.is_empty(),
        "MuxPaneView must expose a Role::Terminal node, roles seen: {:?}",
        a11y_role_summary(&tree)
    );
    // The terminal's own title, not a fixed string. This is the node focus
    // lands on when a pane is entered, so a constant here meant every terminal
    // in the window announced identically. The mock server's pane has no PTY,
    // so the title is the default.
    assert!(
        terminals
            .iter()
            .any(|node| a11y_string_field(node, "label").as_deref() == Some("Terminal")),
        "the TerminalElement surface must be labelled with what it is running"
    );
    assert!(
        terminals
            .iter()
            .any(|node| a11y_string_field(node, "role_description").as_deref() == Some("terminal")),
        "and has to say it is a terminal, which `AXTextArea` does not"
    );

    let text_runs = a11y_text_run_values(&tree);
    assert!(
        text_runs
            .iter()
            .any(|value| value.contains(TERMINAL_MARKER)),
        "a TextRun must carry the served grid text; got {text_runs:?}"
    );
    assert!(
        text_runs
            .iter()
            .any(|value| value.contains("second line with cursor")),
        "every non-empty visible row must produce a TextRun; got {text_runs:?}"
    );
    assert!(
        text_runs.len() >= 3,
        "expected one TextRun per non-empty visible row, got {} ({text_runs:?})",
        text_runs.len()
    );

    // The served snapshot puts the cursor at row 1, column 4. Without a caret
    // on the Terminal node the text runs describe content but not position, so
    // assistive technology cannot follow where typing lands.
    let caret = terminals
        .first()
        .and_then(|node| node.get("aria"))
        .and_then(|aria| aria.get("text_selection"))
        .and_then(|selection| selection.get("focus"))
        .expect("the Terminal node must expose a caret");
    assert_eq!(
        caret.get("character_index").and_then(|ix| ix.as_u64()),
        Some(4),
        "the caret column must match the served cursor state"
    );
    let caret_run = caret
        .get("node")
        .and_then(|id| id.as_str())
        .and_then(|id| tree.get("nodes").and_then(|nodes| nodes.get(id)))
        .and_then(|node| a11y_string_field(node, "value"));
    assert_eq!(
        caret_run.as_deref(),
        Some("second line with cursor"),
        "the caret must point at the run for the cursor's row"
    );
    // Every TextRun must be parented by the Terminal node, otherwise assistive
    // technology cannot associate the lines with the pane.
    let terminal_children = terminals
        .iter()
        .filter_map(|node| node.get("children"))
        .filter_map(|children| children.as_array())
        .map(Vec::len)
        .max()
        .unwrap_or(0);
    assert!(
        terminal_children >= text_runs.len(),
        "TextRun lines must hang off the Terminal node: {terminal_children} children \
         for {} runs",
        text_runs.len()
    );

    assert!(
        a11y_nodes(&tree)
            .iter()
            .any(|(_, node)| { a11y_string_field(node, "label").as_deref() == Some("Terminal") }),
        "the mux pane root should expose its accessible terminal title"
    );
    assert_eq!(
        tree.get("frame")
            .and_then(|frame| frame.get("tab_stop_count"))
            .and_then(serde_json::Value::as_u64),
        Some(0),
        "the mux pane contributes no keyboard tab stop today; if that changed, \
         update this test and the docs"
    );

    // --- pixels ---
    let (window_width, window_height) = (720u32, 320u32);
    let scale = image.width() as f32 / window_width as f32;
    assert!(
        (scale - image.height() as f32 / window_height as f32).abs() < f32::EPSILON,
        "screenshot aspect must match the window: {}x{}",
        image.width(),
        image.height()
    );
    assert_eq!(
        (image.width(), image.height()),
        (
            (window_width as f32 * scale) as u32,
            (window_height as f32 * scale) as u32
        ),
        "screenshot must cover the whole window"
    );

    let colors = distinct_colors(&image);
    assert!(
        colors > 8,
        "terminal frame looks blank: only {colors} distinct colors"
    );

    let accent = [
        ((TERMINAL_ACCENT_BG >> 16) & 0xff) as u8,
        ((TERMINAL_ACCENT_BG >> 8) & 0xff) as u8,
        (TERMINAL_ACCENT_BG & 0xff) as u8,
    ];
    let accent_pixels = count_near_color(&image, accent, 6);
    assert!(
        accent_pixels > 200,
        "the accented grid row background ({accent:?}) must reach the framebuffer, \
         found {accent_pixels} matching pixels out of {}",
        image.width() * image.height()
    );

    Ok(())
}

fn mux_pane_a11y_tree_survives_repeated_frames() -> Result<()> {
    // §16.4: a repainting pane must keep producing a well-formed tree. A
    // regression here shows up as the terminal silently dropping out of the
    // a11y tree after the first frame (stale synthetic-child ids).
    let mut cx = headless_app()?;
    let (domain, _server) = MockMuxServer::start(terminal_grid())?;
    cx.allow_parking();

    let window = open_mux_pane(&mut cx, domain)?;
    draw_until(&mut cx, window.into(), |tree| {
        a11y_text_run_values(tree)
            .iter()
            .any(|value| value.contains(TERMINAL_MARKER))
    })?;

    for frame in 0..20 {
        let (_, tree) = draw_frame(&mut cx, window.into())?;
        assert!(
            !a11y_nodes_with_role(&tree, "Terminal").is_empty(),
            "frame {frame}: Role::Terminal disappeared from the a11y tree"
        );
        assert!(
            a11y_text_run_values(&tree)
                .iter()
                .any(|value| value.contains(TERMINAL_MARKER)),
            "frame {frame}: grid text disappeared from the a11y tree"
        );
    }
    Ok(())
}

// ============================================================================
// §5.4 extension chrome (VDOM bridge)
// ============================================================================

const CHROME_BAR_BG: u32 = 0x101828;
const CHROME_BUTTON_BG: u32 = 0x2f7d32;
const CHROME_METER_FILL: u32 = 0xd94f4f;

/// Renders a VDOM tree through the real `extension_host` bridge, exactly the
/// way the status-bar extension's chrome reaches the screen.
struct ChromeHarness {
    renderer: VDomRenderer,
    node: VDomNode,
}
impl ChromeHarness {
    fn new(node: VDomNode, display_list: Vec<(&'static str, Vec<DrawOp>)>) -> Self {
        Self::new_with_dispatch(node, display_list, None)
    }

    fn new_with_dispatch(
        node: VDomNode,
        display_list: Vec<(&'static str, Vec<DrawOp>)>,
        dispatch: Option<extension_host::vdom_bridge::CommandDispatch>,
    ) -> Self {
        let mut renderer = VDomRenderer::new();
        renderer.set_palette(VDomPalette {
            text: gpui::white(),
            muted_text: gpui::opaque_grey(0.6, 1.0),
            background: gpui::rgb(CHROME_BAR_BG).into(),
            selected_background: gpui::rgb(CHROME_BUTTON_BG).into(),
            border: gpui::opaque_grey(0.5, 1.0),
        });
        for (region, ops) in display_list {
            renderer.set_display_list(region, ops);
        }
        if let Some(dispatch) = dispatch {
            renderer.set_dispatch(dispatch);
        }
        Self { renderer, node }
    }
}

impl Render for ChromeHarness {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let element = self.renderer.render(&self.node, cx);
        div()
            .size_full()
            .bg(gpui::rgb(0x000000))
            .child(div().w_full().h(px(40.0)).child(element))
    }
}

fn status_bar_vdom() -> Result<VDomNode> {
    // Mirrors the shape extensions/z3rm-status-bar returns: a flex row of
    // labelled spans, an interactive button, a controlled input, a spacer that
    // pushes the clock right, and a display-list region for the meter.
    let json = serde_json::json!({
        "type": "div",
        "props": { "id": "status-bar" },
        "style": {
            "flexDirection": "row",
            "alignItems": "center",
            "gap": "8px",
            "padding": "6px",
            "height": "40px",
            "background": format!("#{CHROME_BAR_BG:06x}"),
            "color": "#e6e6e6"
        },
        "children": [
            { "type": "span", "props": { "id": "session-name" }, "children": ["session: main"] },
            {
                "type": "button",
                "props": { "id": "split-button", "onClick": "z3rm.pane.split" },
                "style": {
                    "background": format!("#{CHROME_BUTTON_BG:06x}"),
                    "padding": "6px",
                    "fontWeight": "bold"
                },
                "children": ["Split"]
            },
            {
                "type": "input",
                "props": {
                    "id": "filter-input",
                    "value": "",
                    "placeholder": "filter panes",
                    "onChange": "z3rm.status-bar.filter"
                },
                "style": { "width": "140px", "height": "24px" }
            },
            { "type": "spacer" },
            {
                "type": "display-list",
                "props": { "id": "cpu-meter" },
                "style": { "width": "90px", "height": "24px" }
            }
        ]
    });
    extension_host::vdom_bridge::parse_vdom(&json).map_err(|error| anyhow::anyhow!("{error}"))
}

fn cpu_meter_ops() -> Vec<DrawOp> {
    vec![
        DrawOp::FillRect {
            x: 0.0,
            y: 4.0,
            width: 80.0,
            height: 16.0,
            color: Some(format!("#{CHROME_METER_FILL:06x}")),
        },
        DrawOp::DrawText {
            text: "42%".to_string(),
            x: 2.0,
            y: 4.0,
            color: Some("#ffffff".to_string()),
        },
    ]
}

fn open_chrome(cx: &mut HeadlessAppContext, node: VDomNode) -> Result<WindowHandle<ChromeHarness>> {
    cx.open_window(size(px(560.0), px(80.0)), |_, cx| {
        cx.new(|_| ChromeHarness::new(node, vec![("cpu-meter", cpu_meter_ops())]))
    })
}

fn open_chrome_with_dispatch(
    cx: &mut HeadlessAppContext,
    node: VDomNode,
    dispatch: extension_host::vdom_bridge::CommandDispatch,
) -> Result<WindowHandle<ChromeHarness>> {
    cx.open_window(size(px(560.0), px(80.0)), |_, cx| {
        cx.new(|_| {
            ChromeHarness::new_with_dispatch(
                node,
                vec![("cpu-meter", cpu_meter_ops())],
                Some(dispatch),
            )
        })
    })
}


fn extension_chrome_semantic_button_dispatches_command() -> Result<()> {
    let mut cx = headless_app()?;
    let dispatched = std::rc::Rc::new(std::cell::Cell::new(false));
    let dispatch: extension_host::vdom_bridge::CommandDispatch = {
        let dispatched = dispatched.clone();
        std::rc::Rc::new(
            move |invocation: CommandInvocation, _window: &mut Window, _cx: &mut App| {
                dispatched.set(invocation.command == "z3rm.pane.split");
            },
        )
    };
    let window = open_chrome_with_dispatch(&mut cx, status_bar_vdom()?, dispatch)?;
    draw_frame(&mut cx, window.into())?;
    let (_, tree) = draw_frame(&mut cx, window.into())?;
    // Only `save_frame` runs these, and this scenario does not save one, so
    // without this the extension chrome is checked for dispatch but never for
    // whether a reader could find the thing being dispatched.
    check_a11y(&tree, "extension chrome button");

    let button = a11y_nodes_with_role(&tree, "Button")
        .into_iter()
        .next()
        .context("status bar button missing from accessibility tree")?;
    let node_id = button
        .get("accesskit_id")
        .and_then(serde_json::Value::as_str)
        .context("status bar button missing AccessKit node id")?
        .parse::<u64>()
        .context("invalid AccessKit node id")?;
    let delivered = cx.simulate_a11y_action(
        window.into(),
        gpui::accesskit::ActionRequest {
            target_tree: gpui::accesskit::TreeId::ROOT,
            target_node: gpui::accesskit::NodeId(node_id),
            action: gpui::accesskit::Action::Click,
            data: None,
        },
    )?;
    assert!(delivered, "semantic Click must reach the window's accessibility action callback");
    cx.run_until_parked();
    assert!(
        dispatched.get(),
        "semantic Click must dispatch the VDOM button command"
    );
    Ok(())
}
/// Chrome shapes the golden status-bar frame does not contain. Kept in its own
/// fixture and deliberately not saved: perturbing a screenshot baseline to
/// assert a semantic property means the next person has to regenerate an image
/// on another platform to land an unrelated change.
fn semantic_chrome_vdom() -> Result<VDomNode> {
    let json = serde_json::json!({
        "type": "div",
        "props": { "id": "semantic-chrome" },
        "style": { "flexDirection": "row", "gap": "8px", "padding": "6px" },
        "children": [
            { "type": "span", "props": { "id": "session-name" }, "children": ["session: main"] },
            {
                // A plain node made clickable: an extension can call it what it
                // likes, but it is a control and has to reach the tree as one.
                //
                // Marked chosen the way an extension marks a row: the class
                // gives it a background colour, and the state has to reach a
                // reader as well as the theme.
                "type": "div",
                "props": {
                    "id": "zoom-toggle",
                    "onClick": "z3rm.pane.zoom",
                    "class": "selected"
                },
                "style": { "padding": "6px" },
                "children": ["Zoom"]
            },
            {
                "type": "input",
                "props": {
                    "id": "filter-input",
                    "value": "pane-2",
                    "placeholder": "filter panes",
                    "onChange": "z3rm.status-bar.filter"
                },
                "style": { "width": "140px", "height": "24px" }
            }
        ]
    });
    extension_host::vdom_bridge::parse_vdom(&json).map_err(|error| anyhow::anyhow!("{error}"))
}

fn extension_chrome_exposes_its_controls_and_text() -> Result<()> {
    let mut cx = headless_app()?;
    let window = open_chrome(&mut cx, semantic_chrome_vdom()?)?;

    draw_frame(&mut cx, window.into())?;
    let (_, tree) = draw_frame(&mut cx, window.into())?;
    check_a11y(&tree, "extension_chrome_exposes_its_controls_and_text");

    let mut buttons: Vec<String> = a11y_nodes_with_role(&tree, "Button")
        .iter()
        .filter_map(|node| a11y_string_field(node, "label"))
        .collect();
    buttons.sort();
    assert_eq!(
        buttons,
        vec!["Zoom".to_string()],
        "a node with an onClick is a control whatever it is typed as"
    );
    // The same node carries `class: "selected"`. That draws a background and
    // says nothing on its own; being the chosen one is state, and state has to
    // reach a reader.
    let selected: Vec<bool> = a11y_nodes_with_role(&tree, "Button")
        .iter()
        .filter_map(|node| node.get("aria")?.get("selected")?.as_bool())
        .collect();
    assert_eq!(
        selected,
        vec![true],
        "an extension marking a row chosen must not say it in colour alone"
    );

    let labels: Vec<String> = a11y_nodes_with_role(&tree, "Label")
        .iter()
        .filter_map(|node| a11y_string_field(node, "label"))
        .collect();
    assert!(
        labels.iter().any(|label| label == "session: main"),
        "the extension's own text has to reach the tree: {labels:?}"
    );

    let input = a11y_nodes_with_role(&tree, "TextInput");
    let input = input.first().context("the input must be exposed")?;
    assert_eq!(
        a11y_string_field(input, "label"),
        Some("filter panes".to_string()),
        "a filled field keeps the name it had when it was empty"
    );
    assert_eq!(
        a11y_string_field(input, "value"),
        Some("pane-2".to_string()),
        "and reports what is in it as its value"
    );

    Ok(())
}

fn extension_chrome_vdom_renders_status_bar() -> Result<()> {
    let mut cx = headless_app()?;
    let window = open_chrome(&mut cx, status_bar_vdom()?)?;

    // Two frames: the first establishes layout, the second paints against it.
    draw_frame(&mut cx, window.into())?;
    let (image, tree) = draw_frame(&mut cx, window.into())?;
    save_frame("extension_chrome_status_bar", &image, &tree)?;

    #[cfg(target_os = "linux")]
    assert_eq!(
        image_digest(&image),
        0x1ceb2b6e4d850ada,
        "Linux extension screenshot baseline changed; inspect target/ui_screenshots/extension_chrome_status_bar.png"
    );

    let colors = distinct_colors(&image);
    assert!(
        colors > 4,
        "status bar frame looks blank: only {colors} distinct colors"
    );

    let bar = [
        ((CHROME_BAR_BG >> 16) & 0xff) as u8,
        ((CHROME_BAR_BG >> 8) & 0xff) as u8,
        (CHROME_BAR_BG & 0xff) as u8,
    ];
    let button = [
        ((CHROME_BUTTON_BG >> 16) & 0xff) as u8,
        ((CHROME_BUTTON_BG >> 8) & 0xff) as u8,
        (CHROME_BUTTON_BG & 0xff) as u8,
    ];
    let meter = [
        ((CHROME_METER_FILL >> 16) & 0xff) as u8,
        ((CHROME_METER_FILL >> 8) & 0xff) as u8,
        (CHROME_METER_FILL & 0xff) as u8,
    ];

    assert!(
        count_near_color(&image, bar, 4) > 5_000,
        "the status bar background must fill the bar region"
    );
    assert!(
        count_near_color(&image, button, 4) > 300,
        "the button background from `style.background` must be painted"
    );
    assert!(
        count_near_color(&image, meter, 4) > 1_000,
        "the display-list fillRect must be painted (§5.4 high-frequency widget path)"
    );

    // Text is rendered by the real platform text system, so the frame must
    // contain near-white glyph pixels that no styled rect produces.
    let glyph_pixels = count_near_color(&image, [230, 230, 230], 25);
    assert!(
        glyph_pixels > 100,
        "expected rasterized label glyphs in the status bar, found {glyph_pixels}"
    );

    // The extension's own text: spans carrying labels rather than controls.
    // They contribute no node on their own, so the bridge has to name them or
    // the status bar reaches a reader as its buttons and nothing else.
    let labels: Vec<String> = a11y_nodes_with_role(&tree, "Label")
        .iter()
        .filter_map(|node| a11y_string_field(node, "label"))
        .collect();
    assert!(
        labels.iter().any(|label| label == "session: main"),
        "the extension's own text has to reach the tree: {labels:?}"
    );

    let roles = a11y_role_summary(&tree);
    assert!(
        roles.iter().any(|role| role == "Button"),
        "extension button must be exposed to assistive technology: {roles:?}"
    );
    assert!(
        roles.iter().any(|role| role == "TextInput"),
        "extension input must be exposed to assistive technology: {roles:?}"
    );
    assert_eq!(
        a11y_nodes_with_role(&tree, "Button").len(),
        1,
        "the status bar fixture contains one semantic button"
    );

    // A control with a role but no name is announced as just "button" by a
    // screen reader. The fixture sets no `aria-label`, so these names can only
    // come from the bridge deriving them from content and placeholder.
    assert_eq!(
        a11y_nodes_with_role(&tree, "Button")
            .first()
            .and_then(|node| a11y_string_field(node, "label")),
        Some("Split".to_string()),
        "the button's accessible name must fall back to its text content"
    );
    assert_eq!(
        a11y_nodes_with_role(&tree, "TextInput")
            .first()
            .and_then(|node| a11y_string_field(node, "label")),
        Some("filter panes".to_string()),
        "the input's accessible name must fall back to its placeholder"
    );

    // §5.4 A display list paints straight to draw-ops, so the only thing a
    // screen reader can read is the text it draws — here the meter's "42%".
    let meter = a11y_nodes(&tree)
        .into_iter()
        .find(|(_, node)| a11y_string_field(node, "label").as_deref() == Some("42%"))
        .map(|(_, node)| node)
        .expect("a display-list region must be named by the text it draws");
    assert_eq!(
        meter
            .get("aria")
            .and_then(|aria| aria.get("live")),
        None,
        "a high-frequency widget must not announce itself on every repaint"
    );
    assert_eq!(
        a11y_nodes_with_role(&tree, "TextInput").len(),
        1,
        "the status bar fixture contains one semantic text input"
    );
    assert!(
        roles.iter().any(|role| role == "Window"),
        "the accessibility tree must retain its window root: {roles:?}"
    );

    Ok(())
}

fn extension_chrome_display_list_updates_without_touching_vdom() -> Result<()> {
    // §5.4: a display-list repaint must change pixels without the surrounding
    // VDOM tree changing at all.
    let mut cx = headless_app()?;
    let node = status_bar_vdom()?;
    let window = open_chrome(&mut cx, node)?;

    draw_frame(&mut cx, window.into())?;
    let (before, _) = draw_frame(&mut cx, window.into())?;
    let meter = [
        ((CHROME_METER_FILL >> 16) & 0xff) as u8,
        ((CHROME_METER_FILL >> 8) & 0xff) as u8,
        (CHROME_METER_FILL & 0xff) as u8,
    ];
    let before_fill = count_near_color(&before, meter, 4);

    cx.update_window(window.into(), |view, _window, cx| -> Result<()> {
        let view: Entity<ChromeHarness> = view
            .downcast()
            .map_err(|_| anyhow::anyhow!("root view is not ChromeHarness"))?;
        view.update(cx, |harness, cx| {
            harness.renderer.set_display_list(
                "cpu-meter",
                vec![DrawOp::FillRect {
                    x: 0.0,
                    y: 4.0,
                    width: 20.0,
                    height: 16.0,
                    color: Some(format!("#{CHROME_METER_FILL:06x}")),
                }],
            );
            cx.notify();
        });
        Ok(())
    })??;

    let (after, after_tree) = draw_frame(&mut cx, window.into())?;
    save_frame(
        "extension_chrome_display_list_shrunk",
        &after,
        &after_tree,
    )?;
    let after_fill = count_near_color(&after, meter, 4);

    assert!(
        before_fill > after_fill,
        "shrinking the display-list rect must shrink the painted area: \
         before={before_fill}, after={after_fill}"
    );
    assert!(
        after_fill > 0,
        "the display-list region must still paint after an update"
    );
    Ok(())
}

// ============================================================================
// Sanity: the harness itself
// ============================================================================

struct Swatch;

impl Render for Swatch {
    fn render(&mut self, _window: &mut Window, _cx: &mut Context<Self>) -> impl IntoElement {
        div()
            .size_full()
            .bg(gpui::rgb(0x000000))
            .child(div().w(px(40.0)).h(px(40.0)).bg(gpui::rgb(0x00ff00)))
    }
}

fn terminal_semantic_scroll_actions_move_viewport() -> Result<()> {
    let mut cx = headless_app()?;
    let (domain, _server) = MockMuxServer::start(terminal_grid_with_history())?;
    cx.allow_parking();

    let window = open_mux_pane(&mut cx, domain)?;
    let (_, tree) = draw_until(&mut cx, window.into(), |tree| {
        a11y_text_run_values(tree)
            .iter()
            .any(|value| value.contains(TERMINAL_MARKER))
    })?;

    let terminal = a11y_nodes_with_role(&tree, "Terminal")
        .into_iter()
        .next()
        .context("Terminal node missing from accessibility tree")?;
    let node_id = terminal
        .get("accesskit_id")
        .and_then(serde_json::Value::as_str)
        .context("Terminal node missing AccessKit id")?
        .parse::<u64>()
        .context("invalid AccessKit node id")?;
    let request = |action: gpui::accesskit::Action| gpui::accesskit::ActionRequest {
        target_tree: gpui::accesskit::TreeId::ROOT,
        target_node: gpui::accesskit::NodeId(node_id),
        action,
        data: None,
    };
    let delivered = cx.simulate_a11y_action(window.into(), request(gpui::accesskit::Action::ScrollUp))?;
    assert!(delivered, "ScrollUp must reach the terminal's accessibility action listener");
    cx.run_until_parked();
    let (_, tree) = draw_until(&mut cx, window.into(), |tree| {
        a11y_text_run_values(tree)
            .iter()
            .any(|value| value.contains("HIST-3"))
    })?;
    let runs = a11y_text_run_values(&tree);
    assert!(
        runs.iter().any(|value| value.contains("HIST-3")),
        "semantic ScrollUp must expose the newest history row: {runs:?}"
    );
    assert!(
        !runs.iter().any(|value| value.contains("HIST-0")),
        "a single semantic ScrollUp must not expose the oldest row: {runs:?}"
    );

    let delivered =
        cx.simulate_a11y_action(window.into(), request(gpui::accesskit::Action::ScrollDown))?;
    assert!(delivered, "ScrollDown must reach the terminal's accessibility action listener");
    draw_until(&mut cx, window.into(), |tree| {
        !a11y_text_run_values(tree)
            .iter()
            .any(|value| value.contains("HIST-3"))
    })?;
    Ok(())
}

/// §3.3 The shell reports where each command started and how it ended, and the
/// server keeps those markers — but nothing in the GUI reached them, so
/// scrolling back for "what did that command print" meant hunting by eye.
///
/// The jump moves the viewport, which a sighted user reads at a glance and a
/// screen-reader user cannot: without a live region saying where it landed, the
/// two keystrokes are silent and indistinguishable from doing nothing.
fn prompt_jump_moves_the_viewport_and_says_where_it_landed() -> Result<()> {
    let mut cx = headless_app()?;
    let (domain, _server) = MockMuxServer::start(terminal_grid_with_history())?;
    cx.allow_parking();

    let window = open_mux_pane(&mut cx, domain)?;
    let (_, tree) = draw_until(&mut cx, window.into(), |tree| {
        a11y_text_run_values(tree)
            .iter()
            .any(|value| value.contains(TERMINAL_MARKER))
    })?;
    // The precondition: history is off-screen, so a jump that does nothing
    // cannot be mistaken for a jump that worked.
    assert!(
        !a11y_text_run_values(&tree)
            .iter()
            .any(|value| value.contains("HIST-")),
        "history must start above the viewport"
    );
    assert!(
        a11y_nodes_with_role(&tree, "Status").is_empty(),
        "nothing has been jumped to yet, so there is nothing to announce"
    );

    // The action is dispatched from whatever holds focus, and a headless window
    // focuses nothing on its own.
    cx.update_window(window.into(), |view, window, cx| {
        let handle = view
            .downcast::<MuxPaneView>()
            .ok()
            .map(|view| gpui::Focusable::focus_handle(view.read(cx), cx));
        if let Some(handle) = handle {
            window.focus(&handle, cx);
        }
        window.dispatch_action(
            Box::new(settings::mux_actions::JumpToPreviousPrompt),
            cx,
        );
    })?;
    cx.run_until_parked();

    // The newest command's prompt sits two rows into history, so the jump has
    // to bring history into view.
    let (_, tree) = draw_until(&mut cx, window.into(), |tree| {
        !a11y_nodes_with_role(tree, "Status").is_empty()
    })?;
    check_a11y(&tree, "mux prompt jump");
    let (frame, _) = draw_frame(&mut cx, window.into())?;
    save_frame("mux_prompt_jump", &frame, &tree)?;

    let announced: Vec<String> = a11y_nodes_with_role(&tree, "Status")
        .into_iter()
        .filter_map(|node| a11y_string_field(node, "value"))
        .collect();
    assert!(
        announced
            .iter()
            .any(|value| value.starts_with("Command 2 of 2")),
        "the jump must say which command it landed on: {announced:?}"
    );
    assert!(
        announced.iter().any(|value| value.contains("exited 1")),
        "a reader goes looking for a command because of how it ended: {announced:?}"
    );

    let runs = a11y_text_run_values(&tree);
    assert!(
        runs.iter().any(|value| value.contains("HIST-")),
        "the jump must move the viewport into history: {runs:?}"
    );

    // A second jump backwards lands on the older command; a third has nowhere
    // left to go and must say so rather than silently leaving the view alone.
    for expected in ["Command 1 of 2", "At the oldest recorded command"] {
        cx.update_window(window.into(), |_, window, cx| {
            window.dispatch_action(
                Box::new(settings::mux_actions::JumpToPreviousPrompt),
                cx,
            );
        })?;
        cx.run_until_parked();
        let (_, tree) = draw_until(&mut cx, window.into(), |tree| {
            a11y_nodes_with_role(tree, "Status")
                .into_iter()
                .filter_map(|node| a11y_string_field(node, "value"))
                .any(|value| value.starts_with(expected))
        })?;
        let announced: Vec<String> = a11y_nodes_with_role(&tree, "Status")
            .into_iter()
            .filter_map(|node| a11y_string_field(node, "value"))
            .collect();
        assert!(
            announced.iter().any(|value| value.starts_with(expected)),
            "expected {expected:?}, got {announced:?}"
        );
    }

    Ok(())
}

/// A failure and a piece of news arrive through the same component, told apart
/// on screen by a red warning icon. An icon is not a node and carries no text,
/// so the screenshot and the a11y dump beside it are the two halves of the
/// evidence: the frame shows what a sighted user sees, the tree shows what a
/// reader is told.
fn notification_severity_reaches_the_reader() -> Result<()> {
    struct Stack {
        failure: Entity<MessageNotification>,
        news: Entity<MessageNotification>,
    }

    impl Render for Stack {
        fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
            div()
                .size_full()
                .bg(theme::ActiveTheme::theme(&**cx).colors().background)
                .p(px(16.0))
                .flex()
                .flex_col()
                .gap(px(12.0))
                .child(self.failure.clone())
                .child(self.news.clone())
        }
    }

    let mut cx = headless_app()?;
    let window = cx.open_window(size(px(520.0), px(260.0)), |_, cx| {
        let failure = cx.new(|cx| {
            MessageNotification::from_workspace_error("could not reach the mux server", cx)
        });
        let news = cx.new(|cx| MessageNotification::new("Updated to z3rm 1.12", cx));
        cx.new(|_| Stack { failure, news })
    })?;

    let (image, tree) = draw_frame(&mut cx, window.into())?;
    save_frame("notification_severity", &image, &tree)?;

    let announcements: Vec<String> = a11y_nodes(&tree)
        .into_iter()
        .filter_map(|(_, node)| a11y_string_field(node, "label"))
        .collect();
    assert!(
        announcements
            .iter()
            .any(|text| text == "Error: could not reach the mux server"),
        "the failure has to say it is one; got {announcements:?}"
    );
    assert!(
        announcements
            .iter()
            .any(|text| text == "Updated to z3rm 1.12"),
        "and an ordinary message must not be dressed up as an error; got {announcements:?}"
    );
    // Both notifications are drawn, so the frame is not a single flat fill and
    // the red error accent is actually on screen rather than merely described.
    assert!(
        distinct_colors(&image) > 8,
        "the frame collapsed to a flat fill, so the screenshot proves nothing"
    );
    Ok(())
}

fn headless_renderer_produces_real_pixels() -> Result<()> {
    // Guards the harness: a blank software or GPU frame makes every other
    // visual assertion meaningless.
    let mut cx = headless_app()?;
    let window = cx.open_window(size(px(100.0), px(100.0)), |_, cx| cx.new(|_| Swatch))?;
    let (image, tree) = draw_frame(&mut cx, window.into())?;
    save_frame("harness_swatch", &image, &tree)?;
    let green = count_near_color(&image, [0, 255, 0], 2);
    #[cfg(target_os = "linux")]
    assert_eq!(
        image_digest(&image),
        0x473a8a51de9b6d25,
        "Linux swatch screenshot baseline changed; inspect target/ui_screenshots/harness_swatch.png"
    );
    assert!(
        green > 1_000,
        "expected a solid green swatch in the framebuffer, found {green} pixels"
    );
    Ok(())
}

/// Keeps a reference to `App` alive in scope so the unused-import lint does not
/// fire when the assertions above evolve.
#[allow(dead_code)]
fn _app_type_is_used(_: &App) {}

/// macOS refuses AppKit and Metal calls off the main thread, and libtest only
/// keeps tests on the main thread when it runs them one at a time. Owning the
/// harness lets `cargo test` run this suite correctly without callers having to
/// remember `--test-threads=1`.

/// Magenta accent background for the updated grid: far from anything the theme
/// or the initial grid paints, so its presence in the framebuffer is
/// unambiguous evidence that the updated snapshot was rasterized rather than
/// some incidental chrome.
const UPDATED_ACCENT_BG: u32 = 0xff00ff;

/// Text marker unique to the updated grid, so the a11y tree can prove the
/// follow-up fetch's snapshot replaced the initial one.
const UPDATED_MARKER: &str = "Z3RM-PUSH-SYNC";

/// §3.1 / §3.3 PaneOutput is a lossy dirty signal, never a byte stream for the
/// client to parse: the server stays the sole VT parser, and every
/// render-affecting change is pulled through the structured grid snapshot
/// path. This drives the whole contract: mock server → PaneOutputChunk
/// (sequence 1) → socket → MuxPaneView dirty signal → follow-up
/// FetchGridUpdate → authoritative updated grid (generation 10, output
/// fence 1) → framebuffer + a11y tree. The chunk payload is deliberately
/// opaque — a client that tried to render it would change nothing, because
/// only the pulled snapshot reaches the renderer.
fn pane_output_dirty_signal_pulls_authoritative_grid() -> Result<()> {
    let mut cx = headless_app()?;
    let output = b"opaque bytes the client must never parse\n".to_vec();
    let updated = MockGrid {
        cols: 60,
        rows: 12,
        lines: vec![
            format!("{UPDATED_MARKER} row0"),
            "updated second line".to_string(),
            "updated third line 0123456789".to_string(),
            "updated magenta accent row".to_string(),
        ],
        accent_row: 3,
        accent_background: UPDATED_ACCENT_BG,
        accent_foreground: 0xffffff,
        generation: 10,
        history: Vec::new(),
        history_version: 0,
    };
    let (domain, _server) = MockMuxServer::start_with_output(terminal_grid(), Some(updated), output)?;
    cx.allow_parking();

    let window = open_mux_pane(&mut cx, domain)?;
    let (image, tree) = draw_until_pixels(&mut cx, window.into(), |image| {
        count_near_color(image, [255, 0, 255], 24) > 200
    })?;
    save_frame("mux_pane_dirty_signal_grid_update", &image, &tree)?;

    let magenta = count_near_color(&image, [255, 0, 255], 24);
    assert!(
        magenta > 200,
        "the updated grid's accent row must reach the framebuffer; magenta pixels: {magenta}"
    );
    check_a11y(&tree, "pane after a dirty signal");
    let runs = a11y_text_run_values(&tree);
    assert!(
        runs.iter().any(|value| value.contains(UPDATED_MARKER)),
        "the dirty signal must pull the authoritative updated grid; runs: {runs:?}"
    );
    assert!(
        !runs.iter().any(|value| value.contains(TERMINAL_MARKER)),
        "the updated grid must replace the initial one; runs: {runs:?}"
    );
    Ok(())
}

fn main() {
    let cases: &[(&str, fn() -> Result<()>)] = &[
        (
            "mux_pane_renders_terminal_grid_and_exposes_a11y_tree",
            mux_pane_renders_terminal_grid_and_exposes_a11y_tree,
        ),
        (
            "mux_pane_a11y_tree_survives_repeated_frames",
            mux_pane_a11y_tree_survives_repeated_frames,
        ),
        (
            "extension_chrome_vdom_renders_status_bar",
            extension_chrome_vdom_renders_status_bar,
        ),
        (
            "extension_chrome_exposes_its_controls_and_text",
            extension_chrome_exposes_its_controls_and_text,
        ),
        (
            "extension_chrome_semantic_button_dispatches_command",
            extension_chrome_semantic_button_dispatches_command,
        ),
        (
            "extension_chrome_display_list_updates_without_touching_vdom",
            extension_chrome_display_list_updates_without_touching_vdom,
        ),
        (
            "terminal_semantic_scroll_actions_move_viewport",
            terminal_semantic_scroll_actions_move_viewport,
        ),
        (
            "pane_output_dirty_signal_pulls_authoritative_grid",
            pane_output_dirty_signal_pulls_authoritative_grid,
        ),
        (
            "headless_renderer_produces_real_pixels",
            headless_renderer_produces_real_pixels,
        ),
        (
            "notification_severity_reaches_the_reader",
            notification_severity_reaches_the_reader,
        ),
        (
            "prompt_jump_moves_the_viewport_and_says_where_it_landed",
            prompt_jump_moves_the_viewport_and_says_where_it_landed,
        ),
    ];

    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, case) in cases {
        if filter
            .as_deref()
            .is_some_and(|filter| !name.contains(filter))
        {
            continue;
        }
        ran += 1;
        print!("test {name} ... ");
        match case() {
            Ok(()) => println!("ok"),
            Err(error) => {
                println!("FAILED\n{error:?}");
                failed.push(*name);
            }
        }
    }

    println!(
        "\ntest result: {}. {} passed; {} failed",
        if failed.is_empty() { "ok" } else { "FAILED" },
        ran - failed.len(),
        failed.len()
    );
    if !failed.is_empty() {
        std::process::exit(1);
    }
}
