//! §3.1 The PTY seam.
//!
//! Natively a pane owns a real pty pair from `portable_pty` and a child
//! process, and a thread blocks on the master reading bytes. A browser tab has
//! no pty, no process and no thread to block, so the guest's output arrives by
//! being pushed in from JS — the v86 serial callback in #56 — and a pane's
//! writes go back out the same way.
//!
//! Both sides are named through these aliases so `pane.rs` holds one set of
//! fields rather than two.

#[cfg(not(target_family = "wasm"))]
pub use native::*;
#[cfg(target_family = "wasm")]
pub use wasm::*;

#[cfg(not(target_family = "wasm"))]
mod native {
    pub use portable_pty::PtySize;

    pub type MasterPtyBox = Box<dyn portable_pty::MasterPty + Send>;
    pub type ChildBox = Box<dyn portable_pty::Child + Send + Sync>;
}

#[cfg(target_family = "wasm")]
mod wasm {
    use parking_lot::Mutex;
    use std::io::{self, Write};
    use std::sync::Arc;

    /// Mirrors `portable_pty::PtySize` so the resize call site is shared.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub struct PtySize {
        pub rows: u16,
        pub cols: u16,
        pub pixel_width: u16,
        pub pixel_height: u16,
    }

    /// What a pane writes toward the guest, and the size it last asked for.
    ///
    /// The handler is installed by the JS bridge (#56). Until it is, writes are
    /// dropped rather than buffered without bound: nothing is listening, and a
    /// growing buffer in a tab is worse than a lost keystroke before the guest
    /// has booted.
    #[derive(Default)]
    struct Shared {
        input_handler: Option<Box<dyn Fn(&[u8])>>,
        size: PtySize,
    }

    /// §3.1 The browser's stand-in for a pty master.
    #[derive(Clone, Default)]
    pub struct WasmPty {
        shared: Arc<Mutex<Shared>>,
    }

    impl WasmPty {
        pub fn new() -> Self {
            Self::default()
        }

        /// Install the sink that carries pane writes to the guest.
        pub fn set_input_handler(&self, handler: Box<dyn Fn(&[u8])>) {
            self.shared.lock().input_handler = Some(handler);
        }

        /// The size the pane last resized to, for the bridge to forward.
        pub fn size(&self) -> PtySize {
            self.shared.lock().size
        }

        pub fn resize(&self, size: PtySize) -> anyhow::Result<()> {
            self.shared.lock().size = size;
            Ok(())
        }

        /// A `Write` end for `Pane::pty_writer`.
        pub fn writer(&self) -> Box<dyn Write + Send> {
            Box::new(WasmPtyWriter {
                shared: self.shared.clone(),
            })
        }
    }

    struct WasmPtyWriter {
        shared: Arc<Mutex<Shared>>,
    }

    // There is a single thread in the browser build, so the handler never
    // actually crosses one. `Pane` is declared `Send` on every target, so the
    // promise has to be made here rather than forking the struct.
    unsafe impl Send for WasmPtyWriter {}
    unsafe impl Send for WasmPty {}
    unsafe impl Sync for WasmPty {}

    impl Write for WasmPtyWriter {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            if let Some(handler) = self.shared.lock().input_handler.as_ref() {
                handler(buffer);
            }
            Ok(buffer.len())
        }

        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }

    pub type MasterPtyBox = Box<WasmPty>;

    /// There is no child process to reap; the guest is the emulator.
    pub struct WasmChild;

    pub struct WasmChildKiller;

    impl WasmChildKiller {
        pub fn kill(&mut self) -> anyhow::Result<()> {
            Ok(())
        }
    }

    impl WasmChild {
        pub fn clone_killer(&self) -> WasmChildKiller {
            WasmChildKiller
        }
    }

    pub type ChildBox = Box<WasmChild>;
}
