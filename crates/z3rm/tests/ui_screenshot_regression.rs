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

#![cfg(all(target_os = "macos", unix))]

use anyhow::{Context as _, Result};
use assets::Assets;
use extension_host::vdom_bridge::{DrawOp, VDomNode, VDomPalette, VDomRenderer};
use gpui::{
    AnyWindowHandle, App, AppContext as _, Context, Entity, HeadlessAppContext, IntoElement,
    ParentElement as _, Render, Styled as _, WeakEntity, Window, WindowHandle, div, px, size,
};
use image::RgbaImage;
use mux::MuxDomain;
use mux_protocol::{
    Cell, CellStyle, CursorState, Envelope, FetchGridUpdateResponse, FullGridSnapshot, Request,
    Response, envelope::Payload as EnvelopePayload,
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

/// Build a headless app with a real platform text system (so glyph metrics and
/// rasterization match the shipping app), real embedded fonts, and a real GPU
/// renderer for screenshot capture.
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
            window.draw(cx).clear();
            window.debug_a11y_tree_json()
        })?
        .context(
            "debug_a11y_tree_json returned None; Z3RM_A11Y_BUILD_HEADLESS must be set before \
             the window is opened",
        )?;
    let tree: serde_json::Value =
        serde_json::from_str(&a11y_json).context("a11y tree must be valid JSON")?;
    let image = cx.capture_screenshot(window).context("capture screenshot")?;
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
fn save_frame(name: &str, image: &RgbaImage, tree: &serde_json::Value) -> Result<()> {
    save_screenshot(name, image)?;
    save_a11y_tree(name, tree)?;
    Ok(())
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
        .filter(|pixel| {
            (0..3).all(|channel| {
                pixel.0[channel].abs_diff(rgb[channel]) <= tolerance
            })
        })
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

fn a11y_nodes_with_role<'a>(
    tree: &'a serde_json::Value,
    role: &str,
) -> Vec<&'a serde_json::Value> {
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
/// Image placements do not surface in the accessibility tree, so an
/// image-bearing frame can only be recognized from the framebuffer.
fn draw_until_pixels(
    cx: &mut HeadlessAppContext,
    window: AnyWindowHandle,
    converged: impl Fn(&RgbaImage) -> bool,
) -> Result<(RgbaImage, serde_json::Value)> {
    let deadline = Instant::now() + CONVERGE_TIMEOUT;
    loop {
        cx.run_until_parked();
        // The client coalesces PaneOutput behind a background timer; without
        // advancing the clock that timer never fires and the bytes are never
        // handed to the emulator.
        cx.advance_clock(Duration::from_millis(20));
        cx.run_until_parked();
        let (image, tree) = draw_frame(cx, window)?;
        if converged(&image) {
            return Ok((image, tree));
        }
        if Instant::now() >= deadline {
            return Ok((image, tree));
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
            history_size: 0,
            history_version: 0,
            modes: None,
        }
    }
}

/// Serve mux requests on `stream` until the peer disconnects or `stop` is set.
///
/// `FetchGridUpdate` always answers with the same full snapshot: the view may
/// refetch after a resize, and re-sending the authoritative snapshot is exactly
/// what a real server does on a generation mismatch (§15.4). Every other
/// request gets an empty (success) response so nothing the view does at startup
/// — resize, focus, subscribe — is left hanging.
fn serve_mock_mux(
    stream: UnixStream,
    grid: MockGrid,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    serve_mock_mux_with_output(stream, grid, Vec::new(), stop)
}

/// Same as [`serve_mock_mux`], plus a byte stream pushed as `PaneOutputChunk`
/// once the client subscribes.
///
/// This is the §3.1 in-place render path: the server forwards raw PTY bytes and
/// the client's DisplayOnly emulator parses them, which is the only way escape
/// sequences the grid snapshot cannot express — image protocols among them —
/// reach the renderer.
fn serve_mock_mux_with_output(
    mut stream: UnixStream,
    grid: MockGrid,
    pane_output: Vec<u8>,
    stop: Arc<AtomicBool>,
) -> Result<(), String> {
    stream
        .set_read_timeout(Some(Duration::from_millis(100)))
        .map_err(|error| format!("set mock mux read timeout: {error}"))?;

    let snapshot = grid.snapshot();
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
            let ready_for_output = matches!(&request.body, Some(RequestBody::FetchGridUpdate(_)));
            let response = mock_response(&request, &snapshot, grid.generation);
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

            if ready_for_output && !pane_output_sent && !pane_output.is_empty() {
                pane_output_sent = true;
                let notification = Envelope {
                    version: Some(mux_protocol::PROTOCOL_VERSION),
                    payload: Some(EnvelopePayload::Notification(
                        mux_protocol::Notification {
                            event: Some(mux_protocol::notification::Event::PaneOutput(
                                mux_protocol::PaneOutputChunk {
                                    pane_id: MOCK_PANE_ID.to_string(),
                                    data: pane_output.clone(),
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
    generation: u64,
) -> Response {
    let body = match &request.body {
        Some(RequestBody::FetchGridUpdate(fetch)) => {
            Some(ResponseBody::GridUpdate(FetchGridUpdateResponse {
                from_generation: fetch.since_generation,
                to_generation: generation,
                update: Some(FetchUpdate::FullSnapshot(snapshot.clone())),
            }))
        }
        // Empty body = success. Anything the view issues during startup that is
        // not answered would leave a task waiting forever.
        _ => None,
    };
    Response {
        request_id: request.request_id,
        body,
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
        Self::start_with_output(grid, Vec::new())
    }

    fn start_with_output(
        grid: MockGrid,
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
            move || serve_mock_mux_with_output(server, grid, pane_output, stop)
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

    // --- a11y structure (§16.4) ---
    let terminals = a11y_nodes_with_role(&tree, "Terminal");
    assert!(
        !terminals.is_empty(),
        "MuxPaneView must expose a Role::Terminal node, roles seen: {:?}",
        a11y_role_summary(&tree)
    );
    assert!(
        terminals
            .iter()
            .any(|node| a11y_string_field(node, "label").as_deref() == Some("terminal output")),
        "the TerminalElement surface must be labelled"
    );

    let text_runs = a11y_text_run_values(&tree);
    assert!(
        text_runs.iter().any(|value| value.contains(TERMINAL_MARKER)),
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

    // KNOWN GAP (§16.4): `MuxPaneView::render` puts `.aria_label(pane title)`
    // on the root div, but `Styled::aria_label` alone leaves `a11y_role()` at
    // `None`, and GPUI drops role-less elements from the tree. The pane title
    // therefore never reaches assistive technology, and the focusable pane root
    // contributes no tab stop. Pinned here so a fix is noticed.
    assert!(
        !a11y_nodes(&tree)
            .iter()
            .any(|(_, node)| a11y_string_field(node, "label")
                .is_some_and(|label| label != "terminal output")),
        "the pane title is exposed now — replace this with a positive assertion \
         and update docs/development/ui-regression-testing.md"
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

fn open_chrome(
    cx: &mut HeadlessAppContext,
    node: VDomNode,
) -> Result<WindowHandle<ChromeHarness>> {
    cx.open_window(size(px(560.0), px(80.0)), |_, cx| {
        cx.new(|_| ChromeHarness::new(node, vec![("cpu-meter", cpu_meter_ops())]))
    })
}

fn extension_chrome_vdom_renders_status_bar() -> Result<()> {
    let mut cx = headless_app()?;
    let window = open_chrome(&mut cx, status_bar_vdom()?)?;

    // Two frames: the first establishes layout, the second paints against it.
    draw_frame(&mut cx, window.into())?;
    let (image, tree) = draw_frame(&mut cx, window.into())?;
    save_frame("extension_chrome_status_bar", &image, &tree)?;

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

    // KNOWN GAP (§5.4 / §16.4): `vdom_bridge` never calls `Styled::role`, and
    // GPUI only emits an AccessKit node for elements whose `a11y_role()` is
    // `Some`. Every button, input and label the bridge produces is therefore
    // invisible to assistive technology — the whole chrome collapses to the
    // bare Window root. This assertion pins the current behaviour so the gap
    // cannot be forgotten, and fails the moment the bridge starts emitting
    // roles (at which point it should be replaced with positive assertions
    // for Role::Button / Role::TextInput).
    let roles = a11y_role_summary(&tree);
    assert_eq!(
        roles,
        vec!["Window".to_string()],
        "extension chrome a11y expectations changed. If vdom_bridge now sets \
         roles, replace this with positive Role::Button / Role::TextInput \
         assertions and update docs/development/ui-regression-testing.md"
    );
    assert!(
        a11y_text_run_values(&tree).is_empty(),
        "chrome label text is not exposed as TextRun today; if it is now, \
         update this test and the docs"
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

    let (after, _) = draw_frame(&mut cx, window.into())?;
    save_screenshot("extension_chrome_display_list_shrunk", &after)?;
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

fn headless_renderer_produces_real_pixels() -> Result<()> {
    // Guards the harness: if the Metal headless renderer silently degrades to a
    // blank surface, every other assertion in this file becomes meaningless.
    let mut cx = headless_app()?;
    let window = cx.open_window(size(px(100.0), px(100.0)), |_, cx| cx.new(|_| Swatch))?;
    let (image, _) = draw_frame(&mut cx, window.into())?;
    save_screenshot("harness_swatch", &image)?;

    let green = count_near_color(&image, [0, 255, 0], 2);
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

/// A kitty graphics sequence that draws a solid magenta block.
///
/// Magenta is far from anything the theme or the mock grid paints, so its
/// presence in the framebuffer is unambiguous evidence that the image itself
/// was rasterized rather than some incidental chrome.
fn kitty_magenta_block(control: &str) -> Vec<u8> {
    use base64::Engine as _;

    let image = image::RgbaImage::from_pixel(32, 16, image::Rgba([255, 0, 255, 255]));
    let mut png = Vec::new();
    image
        .write_to(&mut std::io::Cursor::new(&mut png), image::ImageFormat::Png)
        .expect("encode test png");
    let encoded = base64::engine::general_purpose::STANDARD.encode(&png);
    format!("\x1b_G{control};{encoded}\x1b\\").into_bytes()
}

/// §3.1 / §11.2 The image protocols only work across the mux boundary because
/// the server forwards raw PTY bytes and the client's DisplayOnly emulator
/// parses them. Nothing in the grid snapshot can carry an image, so a
/// regression that made the server filter escape sequences — or the client skip
/// the graphics scan — would silently lose images while every grid test kept
/// passing. This drives the whole path: mock server → PaneOutputChunk → socket
/// → MuxPaneView → TerminalElement::paint_image.
fn mux_pane_renders_kitty_image_from_pane_output() -> Result<()> {
    let mut cx = headless_app()?;
    let output = kitty_magenta_block("a=T,f=100,t=d,c=6,r=3");
    let (domain, _server) = MockMuxServer::start_with_output(terminal_grid(), output)?;
    cx.allow_parking();

    let window = open_mux_pane(&mut cx, domain)?;
    let (image, tree) = draw_until_pixels(&mut cx, window.into(), |image| {
        count_near_color(image, [255, 0, 255], 24) > 200
    })?;
    save_frame("mux_pane_kitty_image", &image, &tree)?;

    let magenta = count_near_color(&image, [255, 0, 255], 24);
    assert!(
        magenta > 200,
        "the transmitted image must reach the framebuffer; magenta pixels: {magenta}"
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
            "extension_chrome_display_list_updates_without_touching_vdom",
            extension_chrome_display_list_updates_without_touching_vdom,
        ),
        (
            "mux_pane_renders_kitty_image_from_pane_output",
            mux_pane_renders_kitty_image_from_pane_output,
        ),
        (
            "headless_renderer_produces_real_pixels",
            headless_renderer_produces_real_pixels,
        ),
    ];

    let filter = std::env::args().skip(1).find(|arg| !arg.starts_with('-'));
    let mut failed = Vec::new();
    let mut ran = 0;
    for (name, case) in cases {
        if filter.as_deref().is_some_and(|filter| !name.contains(filter)) {
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
