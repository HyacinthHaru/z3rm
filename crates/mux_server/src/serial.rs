//! §3.1-in-guest Serial transport for the in-guest mux_server.
//!
//! The browser tab owns the other end of this wire: v86 bridges the guest's
//! `ttyS0` to the page, and the client speaks the same length-prefixed
//! protobuf framing the unix socket uses. Exactly one client exists (the
//! page), so the "listener" is the tty itself.

use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use anyhow::Result;
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};

/// Open `device` as a raw (termios raw, no echo) duplex stream for the mux
/// protocol. The caller replaces whatever console process held the tty, so
/// no line discipline competes with the framing.
pub fn open_raw(device: &Path) -> Result<SerialStream> {
    use std::os::fd::IntoRawFd;

    let file = std::fs::OpenOptions::new()
        .read(true)
        .write(true)
        .open(device)?;
    let fd = file.into_raw_fd();

    // raw mode: no echo, no line discipline, no signal chars — the mux
    // framing is binary and must not be touched.
    // SAFETY: fd is a valid open file descriptor we just created.
    unsafe {
        let mut termios: libc::termios = std::mem::zeroed();
        if libc::tcgetattr(fd, &mut termios) != 0 {
            anyhow::bail!("tcgetattr failed on {}", device.display());
        }
        libc::cfmakeraw(&mut termios);
        if libc::tcsetattr(fd, libc::TCSANOW, &termios) != 0 {
            anyhow::bail!("tcsetattr failed on {}", device.display());
        }
    }

    // Verify the fd is valid before taking ownership of it as a File.
    // SAFETY: querying the validity of an fd we just created.
    if unsafe { libc::fcntl(fd, libc::F_GETFD) } == -1 {
        anyhow::bail!("serial fd invalid after termios setup");
    }
    let file = unsafe { std::os::fd::FromRawFd::from_raw_fd(fd) };
    let async_fd = tokio::io::unix::AsyncFd::new(file)?;
    Ok(SerialStream { fd, async_fd })
}

/// A raw serial port as a tokio duplex stream.
pub struct SerialStream {
    fd: std::os::fd::RawFd,
    async_fd: tokio::io::unix::AsyncFd<std::fs::File>,
}

fn read_direct(fd: std::os::fd::RawFd, buf: &mut [u8]) -> std::io::Result<usize> {
    // SAFETY: buf is a valid slice for read(2).
    let n = unsafe { libc::read(fd, buf.as_mut_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

fn write_direct(fd: std::os::fd::RawFd, buf: &[u8]) -> std::io::Result<usize> {
    // SAFETY: buf is a valid slice for write(2).
    let n = unsafe { libc::write(fd, buf.as_ptr().cast(), buf.len()) };
    if n < 0 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(n as usize)
    }
}

impl AsyncRead for SerialStream {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        loop {
            let mut guard = match this.async_fd.poll_read_ready_mut(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            };
            let unfilled = buf.initialize_unfilled();
            let fd = this.fd;
            match guard.try_io(|_| read_direct(fd, unfilled)) {
                Ok(Ok(0)) => return Poll::Ready(Ok(())),
                Ok(Ok(n)) => {
                    buf.advance(n);
                    return Poll::Ready(Ok(()));
                }
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
            guard.clear_ready_matching(tokio::io::Ready::READABLE);
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
    }
}

impl AsyncWrite for SerialStream {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        loop {
            let mut guard = match this.async_fd.poll_write_ready_mut(cx) {
                Poll::Ready(Ok(guard)) => guard,
                Poll::Ready(Err(e)) => return Poll::Ready(Err(e)),
                Poll::Pending => {
                    cx.waker().wake_by_ref();
                    return Poll::Pending;
                }
            };
            let fd = this.fd;
            match guard.try_io(|_| write_direct(fd, buf)) {
                Ok(Ok(n)) => return Poll::Ready(Ok(n)),
                Ok(Err(e)) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                Ok(Err(e)) => return Poll::Ready(Err(e)),
                Err(_would_block) => {}
            }
            guard.clear_ready_matching(tokio::io::Ready::WRITABLE);
            cx.waker().wake_by_ref();
            return Poll::Pending;
        }
    }

    fn poll_flush(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }

    fn poll_shutdown(self: Pin<&mut Self>, _cx: &mut Context<'_>) -> Poll<std::io::Result<()>> {
        Poll::Ready(Ok(()))
    }
}

/// Run the mux server over a serial device. Single client (the browser tab):
/// the "accept loop" is one connection that lives as long as the tty does.
pub fn run_serial(device: PathBuf) -> Result<()> {
    crate::setup_logging()?;

    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;

    rt.block_on(async move {
        let sessions = Arc::new(parking_lot::RwLock::new(Vec::new()));
        let database = Arc::new(parking_lot::Mutex::new(crate::persistence::Connection));
        let clipboard = Arc::new(crate::clipboard::ServerClipboard::new());
        let server_settings = crate::server_settings::ServerSettings::load();
        let extension_host = Arc::new(crate::extension_host::ServerExtensionHost::new());
        let shutdown_state = Arc::new(crate::ShutdownState {
            requested: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            ack_request_id: Arc::new(std::sync::atomic::AtomicU64::new(0)),
            acked: Arc::new(tokio::sync::Notify::new()),
        });

        let stream = open_raw(&device)?;
        eprintln!("mux_server: serial transport ready on {}", device.display());

        // One client for the life of the tty. If the connection ends (the tab
        // closed), exit — the guest has nothing else to serve.
        if let Err(error) =
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
        {
            eprintln!("mux_server: serial connection ended: {error:#}");
        }
        Ok(())
    })
}

/// Default serial device inside the guest.
pub fn default_serial_device() -> PathBuf {
    std::env::var("Z3RM_MUX_SERIAL")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("/dev/ttyS0"))
}
