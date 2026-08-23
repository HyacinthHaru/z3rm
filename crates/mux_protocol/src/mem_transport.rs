//! In-memory duplex transport for running client and server in one process.
//!
//! Used by the wasm build where client and mux_server share a single
//! (single-threaded) process and no sockets or threads are available.
//! The framed prost protocol on top is identical to the socket transport;
//! only the byte carrier differs.

use std::collections::VecDeque;
use std::io::{self, Read, Write};
use std::sync::{Arc, Mutex, MutexGuard};

struct Shared {
    buffer: VecDeque<u8>,
    closed: bool,
    notify: Option<Arc<dyn Fn() + Send + Sync>>,
}

fn lock(shared: &Mutex<Shared>) -> MutexGuard<'_, Shared> {
    // A poisoned lock here means the other endpoint panicked mid-transfer;
    // the buffer itself is still coherent, so recover rather than fail IO.
    shared.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// One end of an in-memory duplex byte stream.
///
/// Reads are non-blocking: an empty buffer yields `ErrorKind::WouldBlock`.
/// Register `set_notify` to be woken when bytes arrive or the peer closes.
#[derive(Clone)]
pub struct MemStream {
    incoming: Arc<Mutex<Shared>>,
    outgoing: Arc<Mutex<Shared>>,
}

impl MemStream {
    /// True when a read would return bytes or observe end-of-stream.
    pub fn is_readable(&self) -> bool {
        let shared = lock(&self.incoming);
        !shared.buffer.is_empty() || shared.closed
    }

    /// Register a callback fired whenever new bytes arrive or the peer closes.
    /// The callback runs outside the internal lock and may re-enter `read`.
    pub fn set_notify(&self, notify: Box<dyn Fn() + Send + Sync>) {
        lock(&self.incoming).notify = Some(Arc::from(notify));
    }

    /// Signal end-of-stream to the peer. Further writes fail with `BrokenPipe`.
    pub fn close(&self) {
        let notify = {
            let mut shared = lock(&self.outgoing);
            shared.closed = true;
            shared.notify.take()
        };
        if let Some(notify) = notify {
            notify();
        }
    }
}

/// Create a connected pair of in-memory streams.
pub fn pair() -> (MemStream, MemStream) {
    let a_to_b = Arc::new(Mutex::new(Shared {
        buffer: VecDeque::new(),
        closed: false,
        notify: None,
    }));
    let b_to_a = Arc::new(Mutex::new(Shared {
        buffer: VecDeque::new(),
        closed: false,
        notify: None,
    }));
    (
        MemStream {
            incoming: Arc::clone(&b_to_a),
            outgoing: Arc::clone(&a_to_b),
        },
        MemStream {
            incoming: a_to_b,
            outgoing: b_to_a,
        },
    )
}

impl Read for MemStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        if buf.is_empty() {
            return Ok(0);
        }
        let mut shared = lock(&self.incoming);
        if shared.buffer.is_empty() {
            return if shared.closed {
                Ok(0)
            } else {
                Err(io::Error::from(io::ErrorKind::WouldBlock))
            };
        }
        let count = shared.buffer.len().min(buf.len());
        for slot in buf.iter_mut().take(count) {
            *slot = shared.buffer.pop_front().unwrap_or(0);
        }
        Ok(count)
    }
}

impl Write for MemStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        let notify = {
            let mut shared = lock(&self.outgoing);
            if shared.closed {
                return Err(io::Error::from(io::ErrorKind::BrokenPipe));
            }
            shared.buffer.extend(buf);
            shared.notify.as_ref().map(Arc::clone)
        };
        // Fire after releasing the lock: the waker typically pumps a reader
        // that immediately locks this same buffer.
        if let Some(notify) = notify {
            (notify)();
        }
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_flow_both_directions() {
        let (mut client, mut server) = pair();
        client.write_all(b"ping").expect("client write");
        let mut buf = [0u8; 8];
        let count = server.read(&mut buf).expect("server read");
        assert_eq!(&buf[..count], b"ping");

        server.write_all(b"pong!").expect("server write");
        let count = client.read(&mut buf).expect("client read");
        assert_eq!(&buf[..count], b"pong!");
    }

    #[test]
    fn empty_read_reports_would_block() {
        let (_client, mut server) = pair();
        let mut buf = [0u8; 4];
        let error = server.read(&mut buf).expect_err("empty read must not block");
        assert_eq!(error.kind(), io::ErrorKind::WouldBlock);
    }

    #[test]
    fn close_is_observed_as_eof() {
        let (mut client, mut server) = pair();
        client.close();
        assert!(server.is_readable());
        let mut buf = [0u8; 4];
        assert_eq!(server.read(&mut buf).expect("eof read"), 0);
        // close() is a half-close: the peer may still write back.
        server.write_all(b"x").expect("write after peer close");
        assert_eq!(client.read(&mut buf).expect("read after close"), 1);
        // Writing into a closed outgoing half fails.
        assert_eq!(
            client
                .write_all(b"x")
                .expect_err("write into closed half")
                .kind(),
            io::ErrorKind::BrokenPipe
        );
    }

    #[test]
    fn notify_fires_on_write_and_close() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        let (mut client, server) = pair();
        let fired = Arc::new(AtomicUsize::new(0));
        server.set_notify({
            let fired = Arc::clone(&fired);
            Box::new(move || {
                fired.fetch_add(1, Ordering::SeqCst);
            })
        });
        client.write_all(b"a").expect("write");
        client.close();
        assert_eq!(fired.load(Ordering::SeqCst), 2);
    }
}
