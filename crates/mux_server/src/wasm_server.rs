//! §3.1 The browser's mux server: one cooperative task, pumped from JS.
//!
//! Natively `Server::run` owns a tokio accept loop and one task per client. A
//! tab has no listener and no runtime, so this owns the same session state and
//! serves exactly one client over the in-memory transport, decoding the same
//! length-prefixed prost frames and emitting the same responses and
//! notifications. Nothing about the protocol changes; only what drives it.

use anyhow::{Context as _, Result};
use mux_protocol::mem_transport::MemStream;
use mux_protocol::{Envelope, frame, unframe};
use parking_lot::Mutex;
use std::io::{Read as _, Write as _};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};

use crate::rt::mpsc;

type Sessions = Arc<parking_lot::RwLock<Vec<crate::session::Session>>>;

/// One in-browser server bound to one client stream.
pub struct WasmMuxServer {
    stream: MemStream,
    /// Bytes read but not yet forming a whole frame.
    pending: Mutex<Vec<u8>>,
    sessions: Sessions,
    outbound_tx: mpsc::UnboundedSender<Envelope>,
    outbound_rx: Mutex<mpsc::UnboundedReceiver<Envelope>>,
    database: Arc<parking_lot::Mutex<crate::persistence::Connection>>,
    clipboard: Arc<crate::clipboard::ServerClipboard>,
    server_settings: Arc<crate::server_settings::ServerSettings>,
    client_role: Arc<parking_lot::Mutex<Option<crate::session::ClientRole>>>,
    connection_client_id: Arc<parking_lot::Mutex<Option<String>>>,
    shutdown_state: Arc<crate::ShutdownState>,
    extension_host: Arc<crate::extension_host::ServerExtensionHost>,
    forward_tasks: Arc<parking_lot::Mutex<Vec<crate::rt::JoinHandle<()>>>>,
    /// Set while `pump` is running so the notify that a write triggers does not
    /// re-enter it. The outer pump observes whatever arrived meanwhile.
    pumping: AtomicBool,
}

impl WasmMuxServer {
    pub fn new(stream: MemStream) -> Arc<Self> {
        let (outbound_tx, outbound_rx) = mpsc::unbounded_channel();
        Arc::new(Self {
            stream,
            pending: Mutex::new(Vec::new()),
            sessions: Arc::default(),
            outbound_tx,
            outbound_rx: Mutex::new(outbound_rx),
            database: Arc::new(parking_lot::Mutex::new(crate::persistence::Connection)),
            clipboard: Arc::new(crate::clipboard::ServerClipboard::default()),
            server_settings: crate::server_settings::ServerSettings::load(),
            client_role: Arc::default(),
            connection_client_id: Arc::default(),
            shutdown_state: Arc::new(crate::ShutdownState {
                requested: Arc::new(AtomicBool::new(false)),
                ack_request_id: Arc::new(AtomicU64::new(0)),
                acked: Arc::new(crate::rt::Notify::new()),
            }),
            extension_host: Arc::new(crate::extension_host::ServerExtensionHost::new()),
            forward_tasks: Arc::default(),
            pumping: AtomicBool::new(false),
        })
    }

    pub fn sessions(&self) -> &Sessions {
        &self.sessions
    }

    /// Wake this server whenever the client writes.
    ///
    /// The peer's write fires this synchronously, which is why `pump` guards
    /// against re-entering itself.
    pub fn attach_notify(self: &Arc<Self>) {
        let server = Arc::downgrade(self);
        self.stream.set_notify(Box::new(move || {
            if let Some(server) = server.upgrade() {
                server.pump();
            }
        }));
    }

    /// Read whatever the client sent, dispatch it, and flush replies.
    ///
    /// Safe to call spuriously: with nothing buffered it does nothing.
    pub fn pump(self: &Arc<Self>) {
        use std::sync::atomic::Ordering;
        if self.pumping.swap(true, Ordering::AcqRel) {
            // Already inside a pump — the bytes that just arrived will be seen
            // by the read below before it returns.
            return;
        }
        let outcome = self.pump_inner();
        self.pumping.store(false, Ordering::Release);
        if let Err(error) = outcome {
            tracing::error!(%error, "wasm mux server pump failed");
        }
    }

    fn pump_inner(self: &Arc<Self>) -> Result<()> {
        self.read_available()?;
        while let Some(envelope) = self.take_frame()? {
            let server = self.clone();
            // Dispatch is async only because the native handlers are; nothing
            // here yields across a real await point, and the reply lands in
            // `outbound_tx` either way.
            crate::rt::spawn(async move {
                if let Err(error) = server.dispatch(&envelope).await {
                    tracing::error!(%error, "wasm mux server request failed");
                }
                server.flush_outbound();
            });
        }
        self.flush_outbound();
        Ok(())
    }

    async fn dispatch(self: &Arc<Self>, envelope: &Envelope) -> Result<()> {
        crate::connection::dispatch_envelope(
            envelope,
            &self.sessions,
            &self.outbound_tx,
            &self.database,
            &self.clipboard,
            &self.server_settings,
            &self.client_role,
            &self.connection_client_id,
            &self.shutdown_state,
            &self.extension_host,
            &self.forward_tasks,
            // Same page, same origin, no network in between: the same
            // trust the local socket gets, for the same reason.
            crate::connection::ConnectionTrust::LocalSocket,
        )
        .await
    }

    fn read_available(&self) -> Result<()> {
        let mut buffer = [0u8; 8192];
        let stream = &mut self.stream.clone();
        loop {
            match stream.read(&mut buffer) {
                Ok(0) => return Ok(()),
                Ok(count) => self.pending.lock().extend_from_slice(&buffer[..count]),
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => return Ok(()),
                Err(error) => return Err(error).context("reading the in-memory client stream"),
            }
        }
    }

    /// Decode one whole frame, leaving a partial one buffered.
    fn take_frame(&self) -> Result<Option<Envelope>> {
        let mut pending = self.pending.lock();
        if pending.is_empty() {
            return Ok(None);
        }
        match unframe(&pending) {
            Ok((envelope, consumed)) => {
                pending.drain(..consumed);
                Ok(Some(envelope))
            }
            // A short buffer is the ordinary case between reads, not an error.
            Err(_) => Ok(None),
        }
    }

    fn flush_outbound(self: &Arc<Self>) {
        let stream = &mut self.stream.clone();
        let mut outbound = self.outbound_rx.lock();
        while let Some(envelope) = outbound.try_recv() {
            match frame(&envelope) {
                Ok(bytes) => {
                    if let Err(error) = stream.write_all(&bytes) {
                        tracing::warn!(%error, "writing to the in-memory client stream failed");
                        return;
                    }
                }
                Err(error) => tracing::error!(%error, "encoding a response envelope failed"),
            }
        }
    }
}
