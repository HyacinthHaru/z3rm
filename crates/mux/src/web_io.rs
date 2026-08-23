//! wasm single-process transport driver.
//!
//! Native builds run `io_and_router_loop` on a dedicated `mux-io` thread with
//! a nonblocking socket and a 1ms idle sleep. The browser has no threads, so
//! the wasm build drives the identical framed protocol cooperatively: the
//! in-memory stream's notify callback and an explicit pump after each queued
//! write replace the thread. Protocol semantics are shared with the native
//! loop through `read_next_frame_generic` and `route_envelope_payload`.

use super::*;
use mux_protocol::mem_transport::MemStream;
use std::io::Write as _;
use std::sync::mpsc;

enum PumpStep {
    Progress,
    Idle,
    Closed,
}

pub(crate) struct WasmIoDriver {
    stream: MemStream,
    write_rx: mpsc::Receiver<Vec<u8>>,
    read_buffer: Vec<u8>,
    pending_writes: VecDeque<(Vec<u8>, usize)>,
}

impl WasmIoDriver {
    fn new(stream: MemStream, write_rx: mpsc::Receiver<Vec<u8>>) -> Self {
        Self {
            stream,
            write_rx,
            read_buffer: Vec::new(),
            pending_writes: VecDeque::new(),
        }
    }

    /// One cooperative iteration mirroring the native loop body: drain the
    /// request channel, flush pending writes, then read every complete frame
    /// currently available. `WouldBlock` anywhere just means "no progress
    /// right now", never an error.
    fn step(
        &mut self,
        inner: &Arc<parking_lot::RwLock<DomainInner>>,
        subscribers: &Arc<parking_lot::Mutex<Vec<SubscriberSender>>>,
        worker_epoch: u64,
    ) -> PumpStep {
        let mut progressed = false;

        while let Ok(framed) = self.write_rx.try_recv() {
            self.pending_writes.push_back((framed, 0));
        }

        while let Some((frame, offset)) = self.pending_writes.front_mut() {
            if *offset == frame.len() {
                self.pending_writes.pop_front();
                continue;
            }
            match self.stream.write(&frame[*offset..]) {
                Ok(0) => {
                    tracing::error!("in-memory stream write returned zero bytes");
                    return PumpStep::Closed;
                }
                Ok(written) => {
                    *offset += written;
                    progressed = true;
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    tracing::error!(error = %error, "in-memory stream write error");
                    return PumpStep::Closed;
                }
            }
        }

        loop {
            match MuxDomain::read_next_frame_generic(&mut self.stream, &mut self.read_buffer) {
                Ok(Some(framed)) => {
                    progressed = true;
                    let envelope = match mux_protocol::unframe(&framed) {
                        Ok((envelope, _)) => envelope,
                        Err(error) => {
                            tracing::error!(error = %error, "failed to decode envelope");
                            return PumpStep::Closed;
                        }
                    };
                    match envelope.payload {
                        Some(payload) => MuxDomain::route_envelope_payload(
                            payload,
                            inner,
                            subscribers,
                            worker_epoch,
                        ),
                        None => tracing::warn!("envelope with no payload"),
                    }
                }
                Ok(None) => break,
                Err(error) => {
                    tracing::error!(error = %error, "in-memory stream read error");
                    return PumpStep::Closed;
                }
            }
        }

        if progressed { PumpStep::Progress } else { PumpStep::Idle }
    }
}

/// Shared wasm I/O state: the driver plus the routing context the native
/// worker thread would carry. Pumped from notify callbacks and from the
/// `send_request` write hook.
pub(crate) struct WasmIoShared {
    driver: parking_lot::Mutex<WasmIoDriver>,
    inner: Arc<parking_lot::RwLock<DomainInner>>,
    subscribers: Arc<parking_lot::Mutex<Vec<SubscriberSender>>>,
    epoch: u64,
}

impl WasmIoShared {
    /// Pump until no progress can be made. Re-entrant safe: writing to the
    /// in-memory stream synchronously wakes the peer, and the peer's answer
    /// can re-enter this pump while the driver's lock is held; `try_lock`
    /// makes that nested call a no-op because the outer loop will observe the
    /// new bytes on its next iteration.
    pub(crate) fn pump(&self) {
        let mut driver = match self.driver.try_lock() {
            Some(driver) => driver,
            None => return,
        };
        loop {
            if self.inner.read().transport_epoch.load(Ordering::SeqCst) != self.epoch {
                break;
            }
            match driver.step(&self.inner, &self.subscribers, self.epoch) {
                PumpStep::Progress => continue,
                PumpStep::Idle => break,
                PumpStep::Closed => {
                    let pending =
                        drain_pending_requests_for_epoch(&mut self.inner.write(), self.epoch);
                    drop(pending);
                    break;
                }
            }
        }
    }
}

impl MuxDomain {
    /// Connect to an in-process mux_server over an in-memory duplex stream.
    /// This is the wasm replacement for `connect_local`: same framed
    /// protocol, same routing, no socket and no I/O thread.
    pub fn connect_in_memory(stream: MemStream) -> Result<Self> {
        let (write_tx, write_rx) = std::sync::mpsc::sync_channel(WRITE_QUEUE_CAPACITY);
        let subscribers: Arc<parking_lot::Mutex<Vec<SubscriberSender>>> =
            Arc::new(parking_lot::Mutex::new(Vec::new()));
        let inner = Arc::new(parking_lot::RwLock::new(DomainInner {
            next_request_id: AtomicU64::new(1),
            pending_requests: HashMap::new(),
            subscribers: subscribers.clone(),
            write_tx,
            transport_epoch: AtomicU64::new(0),
        }));

        let shared = Arc::new(WasmIoShared {
            driver: parking_lot::Mutex::new(WasmIoDriver::new(stream.clone(), write_rx)),
            inner: inner.clone(),
            subscribers,
            epoch: 0,
        });
        let notify = shared.clone();
        stream.set_notify(Box::new(move || notify.pump()));

        Ok(MuxDomain {
            inner,
            window_id: parking_lot::RwLock::new(mint_window_id()),
            local_socket_path: parking_lot::RwLock::new(None),
            last_attached_snapshot: parking_lot::RwLock::new(None),
            last_attached_session_id: parking_lot::RwLock::new(None),
            wasm_io: parking_lot::Mutex::new(Some(shared)),
        })
    }
}
