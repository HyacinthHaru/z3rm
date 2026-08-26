//! §3.1-in-guest The serial link to the mux server running inside v86.
//!
//! The desktop client reaches its daemon over a socket; this tab reaches a
//! mux_server running inside the emulated Linux over the machine's serial
//! port. The same in-memory duplex used by the old in-tab server stands in for
//! the socket, while this module pumps its far end across the JS boundary.

use anyhow::Context as _;
use mux::MuxDomain;
use mux_protocol::mem_transport::{self, MemStream};
use std::io::{Read as _, Write as _};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use wasm_bindgen::prelude::*;

/// The marker the guest start script prints just before exec'ing mux_server.
/// Bytes before it are boot text; bytes after it are protocol frames.
pub const READY_MARKER: &[u8] = b"Z3RM_MUX_READY";

struct LinkState {
    server_end: MemStream,
    ready: Arc<AtomicBool>,
    /// Boot text accumulated until the marker, so a marker split across
    /// batches is still detected.
    pending: Mutex<Vec<u8>>,
}

static LINK: Mutex<Option<Arc<LinkState>>> = Mutex::new(None);

fn lock<'a>(
    state: &'a Mutex<Option<Arc<LinkState>>>,
) -> std::sync::MutexGuard<'a, Option<Arc<LinkState>>> {
    state
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Install the serial link: returns a domain already wired across the serial
/// line and a ready flag that flips once the in-guest mux server answers.
pub fn install() -> anyhow::Result<(Arc<MuxDomain>, Arc<AtomicBool>)> {
    let (server_end, client_end) = mem_transport::pair();
    let ready = Arc::new(AtomicBool::new(false));
    let state = Arc::new(LinkState {
        server_end: server_end.clone(),
        ready: ready.clone(),
        pending: Mutex::new(Vec::new()),
    });

    // Drain the server end whenever the domain writes: these are client
    // protocol frames and cross the JS boundary to serial0.
    let pump_state = state.clone();
    server_end.set_notify(Box::new(move || pump_to_guest(&pump_state)));

    *lock(&LINK) = Some(state);
    let domain = MuxDomain::connect_in_memory(client_end)
        .context("connecting the mux domain across the serial link")?;
    Ok((Arc::new(domain), ready))
}

/// Drain client protocol frames and hand them to the guest serial input.
fn pump_to_guest(state: &LinkState) {
    const CHUNK: usize = 4096;
    let mut buffer = [0u8; CHUNK];
    let mut server_end = state.server_end.clone();
    loop {
        let count = match server_end.read(&mut buffer) {
            Ok(count) => count,
            Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(error) if error.kind() == std::io::ErrorKind::BrokenPipe => return,
            Err(_) => return,
        };
        if count == 0 {
            return;
        }
        if let Err(error) = v86_send(&buffer[..count]) {
            log::warn!("serial link: guest rejected {count} bytes: {error:?}");
            return;
        }
    }
}

/// Feed one batch of guest serial output into the client side of the link.
/// Boot text is discarded here because the page renders it; after the marker,
/// bytes are the normal length-prefixed mux protocol stream.
pub fn on_guest_bytes(bytes: &[u8]) {
    let Some(state) = lock(&LINK).clone() else {
        return;
    };
    if state.ready.load(Ordering::SeqCst) {
        let mut server_end = state.server_end.clone();
        if let Err(error) = server_end.write_all(bytes) {
            log::warn!("serial link: failed to enqueue guest bytes: {error}");
        }
        return;
    }

    let mut pending = state
        .pending
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.extend_from_slice(bytes);
    let Some(position) = find_subsequence(&pending, READY_MARKER) else {
        if pending.len() > 64 * 1024 {
            let keep_from = pending.len() - READY_MARKER.len();
            let tail = pending.split_off(keep_from);
            *pending = tail;
        }
        return;
    };

    let after = position + READY_MARKER.len();
    let trailing = pending.split_off(after);
    pending.clear();
    drop(pending);
    state.ready.store(true, Ordering::SeqCst);
    if !trailing.is_empty() {
        let mut server_end = state.server_end.clone();
        if let Err(error) = server_end.write_all(&trailing) {
            log::warn!("serial link: failed to enqueue post-marker bytes: {error}");
        }
    }
}

/// Wait until the in-guest mux server has replaced the console shell.
pub async fn wait_ready(ready: &AtomicBool) -> anyhow::Result<()> {
    const TIMEOUT: web_time::Duration = web_time::Duration::from_secs(60);
    let deadline = web_time::Instant::now() + TIMEOUT;
    loop {
        if ready.load(Ordering::SeqCst) {
            return Ok(());
        }
        if web_time::Instant::now() > deadline {
            anyhow::bail!(
                "the mux server inside the v86 guest never signalled ready on the serial line"
            );
        }
        gloo_timers::future::TimeoutFuture::new(200).await;
    }
}

fn find_subsequence(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

/// Hand client protocol frames to v86's serial input.
#[wasm_bindgen]
unsafe extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__z3rm_v86"], js_name = send, catch)]
    fn v86_send(bytes: &[u8]) -> Result<(), JsValue>;
}
