//! §3.1-in-guest Serial transport for the in-guest mux_server.
//!
//! v86's emulated tty is not a reliable epoll/AsyncFd source. The serial
//! transport therefore uses two blocking OS threads (one read, one write) and
//! presents their byte queues as a normal tokio AsyncRead/AsyncWrite stream.
//! The mux protocol and connection handler remain identical to the socket path.

use std::collections::VecDeque;
use std::io::{Read as _, Write as _};
use std::os::fd::{FromRawFd, IntoRawFd};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::task::{Context, Poll};
use std::thread;

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Open `device` as a raw duplex stream. Blocking reader/writer threads avoid
/// relying on epoll readiness for v86's emulated UART.
pub fn open_raw(device: &Path) -> Result<SerialStream> {
    use std::os::unix::fs::OpenOptionsExt;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .custom_flags(libc::O_NOCTTY)
        .open(device)?;
    let fd = file.into_raw_fd();

    // SAFETY: fd is an open tty descriptor owned by this function.
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) != 0 {
            anyhow::bail!("tcgetattr failed on {}", device.display());
        }
        libc::cfmakeraw(&mut termios);
        termios.c_cc[libc::VMIN] = 1;
        termios.c_cc[libc::VTIME] = 0;
        termios.c_cflag |= libc::CLOCAL | libc::CREAD;
        if libc::tcsetattr(fd, libc::TCSANOW, &termios) != 0 {
            anyhow::bail!("tcsetattr failed on {}", device.display());
        }
    }

    // Separate descriptors let a blocking reader and writer run concurrently.
    // SAFETY: dup returns independent descriptors referring to the configured
    // tty; ownership is transferred to the File values below.
    let read_fd = unsafe { libc::dup(fd) };
    let write_fd = unsafe { libc::dup(fd) };
    if read_fd < 0 || write_fd < 0 {
        unsafe {
            libc::close(fd);
            if read_fd >= 0 {
                libc::close(read_fd);
            }
            if write_fd >= 0 {
                libc::close(write_fd);
            }
        }
        anyhow::bail!("dup failed for serial tty {}", device.display());
    }
    // The two duplicated descriptors now own the tty; close the original raw fd.
    unsafe {
        libc::close(fd);
    }

    let reader = unsafe { std::fs::File::from_raw_fd(read_fd) };
    let writer = unsafe { std::fs::File::from_raw_fd(write_fd) };
    let (incoming_tx, incoming_rx) = tokio::sync::mpsc::unbounded_channel();
    let (outgoing_tx, mut outgoing_rx) = tokio::sync::mpsc::unbounded_channel::<Vec<u8>>();

    thread::Builder::new()
        .name("z3rm-serial-reader".into())
        .spawn(move || {
            let mut reader = reader;
            let mut buffer = [0u8; 4096];
            loop {
                match reader.read(&mut buffer) {
                    Ok(0) => thread::yield_now(),
                    Ok(count) => {
                        if incoming_tx.send(buffer[..count].to_vec()).is_err() {
                            break;
                        }
                    }
                    Err(error) if error.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(error) => {
                        tracing::debug!(%error, "serial reader stopped");
                        break;
                    }
                }
            }
        })?;

    thread::Builder::new()
        .name("z3rm-serial-writer".into())
        .spawn(move || {
            let mut writer = writer;
            while let Some(buffer) = outgoing_rx.blocking_recv() {
                if let Err(error) = writer.write_all(&buffer) {
                    tracing::debug!(%error, "serial writer stopped");
                    break;
                }
                if let Err(error) = writer.flush() {
                    tracing::debug!(%error, "serial writer flush stopped");
                    break;
                }
            }
        })?;

    Ok(SerialStream {
        incoming: incoming_rx,
        outgoing: outgoing_tx,
        pending_read: VecDeque::new(),
    })
}

/// Tokio stream backed by blocking serial reader/writer threads.
pub struct SerialStream {
    incoming: tokio::sync::mpsc::UnboundedReceiver<Vec<u8>>,
    outgoing: tokio::sync::mpsc::UnboundedSender<Vec<u8>>,
    pending_read: VecDeque<u8>,
}

impl AsyncRead for SerialStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        loop {
            if !self.pending_read.is_empty() {
                let count = self.pending_read.len().min(buf.remaining());
                let bytes: Vec<u8> = self.pending_read.drain(..count).collect();
                buf.put_slice(&bytes);
                return Poll::Ready(Ok(()));
            }
            match Pin::new(&mut self.incoming).poll_recv(cx) {
                Poll::Ready(Some(bytes)) => self.pending_read.extend(bytes),
                Poll::Ready(None) => return Poll::Ready(Ok(())),
                Poll::Pending => return Poll::Pending,
            }
        }
    }
}

impl AsyncWrite for SerialStream {
    fn poll_write(
        self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
        buffer: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        if self.outgoing.send(buffer.to_vec()).is_err() {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "serial writer stopped",
            )));
        }
        Poll::Ready(Ok(buffer.len()))
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Run the mux server over a serial device. One client (the browser tab) owns
/// the tty for the lifetime of the process.
pub fn run_serial(device: PathBuf) -> Result<()> {
    crate::setup_logging()?;
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    runtime.block_on(async move {
        let sessions = std::sync::Arc::new(parking_lot::RwLock::new(Vec::new()));
        let database = std::sync::Arc::new(parking_lot::Mutex::new(crate::persistence::Connection));
        let clipboard = std::sync::Arc::new(crate::clipboard::ServerClipboard::new());
        let server_settings = crate::server_settings::ServerSettings::load();
        let extension_host = std::sync::Arc::new(crate::extension_host::ServerExtensionHost::new());
        let shutdown_state = std::sync::Arc::new(crate::ShutdownState {
            requested: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ack_request_id: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            acked: std::sync::Arc::new(tokio::sync::Notify::new()),
        });
        let stream = open_raw(&device)?;
        crate::connection::handle_connection(
            stream,
            sessions,
            database,
            clipboard,
            server_settings,
            shutdown_state,
            extension_host,
        )
        .await
    })
}

pub fn default_serial_device() -> PathBuf {
    std::env::var("Z3RM_MUX_SERIAL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/dev/ttyS0"))
}
