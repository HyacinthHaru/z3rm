// §3.1 The guest behind the terminal: a Linux running in v86, reached over its
// emulated serial port.
//
// The Rust side owns the pane and its pty; this owns the emulator. The two meet
// at exactly two calls:
//
//   window.__z3rm_v86.send(bytes)          pane -> guest  (called from Rust)
//   wasmBindings.z3rm_v86_serial_bytes(b)  guest -> pane  (called from here)
//
// `window.wasmBindings` is what Trunk publishes for a `web` bindgen target, so
// the module's hashed filename never has to be known here.

const V86_ASSETS = new URL("../v86/", document.baseURI);
const BOOT_CMDLINE = "console=ttyS0 tsc=reliable mitigations=off random.trust_cpu=on";
const MEMORY_BYTES = 128 * 1024 * 1024;

/// Serial output arrives a byte at a time and a booting kernel prints far
/// faster than a frame. Batching per frame turns thousands of tiny calls into
/// one, and one repaint instead of thousands.
class SerialBatch {
  constructor(deliver) {
    this.deliver = deliver;
    this.pending = [];
    this.scheduled = false;
  }

  push(byte) {
    this.pending.push(byte);
    if (this.scheduled) {
      return;
    }
    this.scheduled = true;
    requestAnimationFrame(() => {
      this.scheduled = false;
      const batch = Uint8Array.from(this.pending);
      this.pending.length = 0;
      this.deliver(batch);
    });
  }
}

function loadV86Library() {
  return new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = new URL("libv86.js", V86_ASSETS).href;
    script.onload = () => resolve();
    script.onerror = () => reject(new Error(`could not load ${script.src}`));
    document.head.append(script);
  });
}

/// The Rust side installs its exports on `window.wasmBindings` once the module
/// has initialised, which happens on its own schedule relative to this file.
function waitForWasmBindings() {
  return new Promise((resolve) => {
    const poll = () => {
      const bindings = window.wasmBindings;
      if (bindings && typeof bindings.z3rm_v86_serial_bytes === "function") {
        resolve(bindings);
        return;
      }
      requestAnimationFrame(poll);
    };
    poll();
  });
}

async function boot() {
  await loadV86Library();

  const emulator = new window.V86({
    wasm_path: new URL("v86.wasm", V86_ASSETS).href,
    memory_size: MEMORY_BYTES,
    bios: { url: new URL("seabios.bin", V86_ASSETS).href },
    bzimage: { url: new URL("buildroot-bzimage.bin", V86_ASSETS).href, async: false },
    cmdline: BOOT_CMDLINE,
    autostart: true,
    // No screen, no input devices: the terminal is the only interface, and the
    // pane owns the keyboard.
    disable_keyboard: true,
    disable_mouse: true,
    disable_speaker: true,
  });

  // Attach before anything else. The kernel starts printing during the
  // constructor, and a listener added a tick later loses the whole boot.
  const batch = new SerialBatch((bytes) => {
    const bindings = window.wasmBindings;
    if (bindings) {
      bindings.z3rm_v86_serial_bytes(bytes);
    }
  });
  emulator.add_listener("serial0-output-byte", (byte) => batch.push(byte));

  window.__z3rm_v86 = {
    emulator,
    send(bytes) {
      for (const byte of bytes) {
        emulator.bus.send("serial0-input", byte);
      }
    },
  };

  // A serial line carries no window size, so the guest is told once the shell
  // is up. Rust knows the size the pane settled on.
  const bindings = await waitForWasmBindings();
  const size = bindings.z3rm_v86_pane_size?.();
  if (size) {
    const [rows, cols] = size;
    emulator.serial0_send(`stty rows ${rows} cols ${cols}\n`);
  }
}

boot().catch((error) => {
  console.error("v86 bridge failed to start:", error);
});
