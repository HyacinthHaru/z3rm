//! §3.1-in-guest The serial link to the mux server running inside v86.
//!
//! The desktop client reaches its daemon over a socket; this tab reaches a
//! mux_server running *inside the emulated Linux* over the machine's serial
//! port. The same in-memory duplex the in-tab server used still stands in for
//! the socket — but its far end is no longer a server in this process, it is
//! a byte pump across the JS boundary:
//!
//! * client → guest: the domain writes frames into the client end; the pump
//!   drains the server end and hands the bytes to JS (`__z3rm_v86.send`).
//! * guest → client: JS delivers serial bytes ([`on_guest_bytes`]); before the
//!   server is up they are boot text (the page renders them itself), after it
//!   they are protocol frames written into the server end.

use anyhow::Context as _;
use mux::MuxDomain;
use mux_protocol::mem_transport::{self, MemStream};
use std::io::{Read as _, Write as _};
use wasm_bindgen::prelude::*;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

/// The marker the guest's start script prints just before exec'ing the mux
/// server. Everything on the serial line before it is boot text; everything
/// after it is mux protocol framing.
pub const READY_MARKER: &[u8] = b"Z3RM_MUX_READY";

struct LinkState {
    server_end: MemStream,
    ready: Arc<AtomicBool>,
    /// Boot text accumulated until the marker, so a marker split across
    /// batches is still detected.
    pending: Mutex<Vec<u8>>,
}

static LINK: Mutex<Option<Arc<LinkState>>> = Mutex::new(None);

fn lock<'a>(state: &'a Mutex<Option<Arc<LinkState>>>) -> std::sync::MutexGuard<'a, Option<Arc<LinkState>>> {
    // A poisoned lock means a panicking pump left coherent bytes behind;
    // recover rather than lose the link.
    state.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// Install the serial link: returns a domain already wired across the serial
/// line and a ready flag that flips once the in-guest mux server answers.
pub fn install() -> anyhow::Result<(Arc<mux::MuxDomain>, Arc<AtomicBool>)> {
    let (server_end, client_end) = mem_transport::pair();
    let ready = Arc::new(AtomicBool::new(false));
    let state = Arc::new(LinkState {
        server_end: server_end.clone(),
        ready: ready.clone(),
        pending: Mutex::new(Vec::new()),
    });

    // Drain the server end whenever the domain writes: the bytes cross the
    // JS boundary to the guest's serial input.
    let pump_state = state.clone();
    server_end.set_notify(Box::new(move || pump_to_guest(&pump_state)));

    *lock(&LINK) = Some(state);
    let domain = mux::MuxDomain::connect_in_memory(client_end)
        .context("connecting the mux domain across the serial link")?;
    Ok((Arc::new(domain), ready))
}

/// Server-end bytes are client protocol frames: drain and forward to JS.
fn pump_to_guest(state: &LinkState) {
    const CHUNK: usize = 4096;
    let mut buf = [0u8; CHUNK];
    let mut server_end = state.server_end.clone();
    loop {
        // Non-blocking read: WouldBlock ends the drain.
        let n = match server_end.read(&mut buf) {
            Ok(n) => n,
            Err(ref e) if e.kind() == std::io::ErrorKind::WouldBlock => return,
            Err(ref e) if e.kind() == std::io::ErrorKind::BrokenPipe => return,
            Err(_) => return,
        };
        if n == 0 {
            return;
        }
        if let Err(error) = v86_send(&buf[..n]) {
            log::warn!("serial link: guest rejected {} bytes: {error:?}", n);
            return;
        }
    }
}

/// Feed one batch of the guest's serial output into the link.
///
/// Called from JS per animation frame. Before the ready marker the bytes are
/// boot text the page renders itself; after it they are protocol frames.
pub fn on_guest_bytes(bytes: &[u8]) {
    let Some(state) = lock(&LINK).clone() else {
        return;
    };
    if state.ready.load(Ordering::SeqCst) {
        let mut server_end = state.server_end.clone();
        let _ = server_end.write(bytes);
        return;
    }

    // Boot phase: accumulate and look for the marker.
    let mut pending = state.pending.lock().unwrap_or_else(|poisoned| poisoned.into_inner());
    pending.extend_from_slice(bytes);
    if let Some(position) = find_subsequence(&pending, READY_MARKER) {
        let after = position + READY_MARKER.len();
        // Anything after the marker in this batch is already protocol.
        let trailing = pending.split_off(after);
        pending.clear();
        drop(pending);
        state.ready.store(true, Ordering::SeqCst);
        if !trailing.is_empty() {
            let mut server_end = state.server_end.clone();
            let _ = server_end.write(&trailing);
        }
    } else if pending.len() > 64 * 1024 {
        // Boot noise without a marker: keep only the tail so a split marker
        // can still be found.
        let keep_from = pending.len() - READY_MARKER.len();
        let tail = pending.split_off(keep_from);
        *pending = tail;
    }
}

/// Wait until the in-guest mux server is answering on the serial line.
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

#[wasm_bindgen]
unsafe extern "C" {
    #[wasm_bindgen(js_namespace = ["window", "__z3rm_v86"], js_name = send, catch)]
    fn v86_send(bytes: &[u8]) -> Result<(), JsValue>;
}
