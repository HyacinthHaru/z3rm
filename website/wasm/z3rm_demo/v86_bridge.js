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

const V86_ASSETS = new URL("./v86/", document.baseURI);
const BOOT_CMDLINE = "console=ttyS0 tsc=reliable mitigations=off random.trust_cpu=on";
const MEMORY_BYTES = 128 * 1024 * 1024;
const MAX_TERMINAL_TEXT = 200_000;

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
    const flush = () => {
      this.scheduled = false;
      const batch = Uint8Array.from(this.pending);
      this.pending.length = 0;
      this.deliver(batch);
    };
    // rAF works on real browsers; setTimeout is the headless/CI fallback.
    requestAnimationFrame(flush);
    setTimeout(flush, 50);
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
    const start = Date.now();
    const poll = () => {
      const bindings = window.wasmBindings;
      if (bindings && typeof bindings.z3rm_v86_serial_bytes === "function") {
        resolve(bindings);
        return;
      }
      if (Date.now() - start >= 15000) {
        resolve(null);
        return;
      }
      setTimeout(poll, 100);
    };
    poll();
  });
}

/// Render serial output to the boot terminal as a fallback when GPUI is not
/// yet or never available.
const decoder = new TextDecoder();
let muxBootText = [];
function renderSerial(bytes) {
  const node = document.getElementById("boot-terminal-output");
  if (!node) return;
  node.textContent += decoder.decode(bytes, { stream: true });
  if (node.textContent.length > MAX_TERMINAL_TEXT) {
    node.textContent = node.textContent.slice(-MAX_TERMINAL_TEXT);
  }
  const shell = document.getElementById("boot-terminal");
  shell?.scrollTo(0, shell.scrollHeight);
}

async function boot() {
  await loadV86Library();

  const emulator = new window.V86({
    wasm_path: new URL("v86.wasm", V86_ASSETS).href,
    memory_size: MEMORY_BYTES,
    bios: { url: new URL("seabios.bin", V86_ASSETS).href },
    bzimage: { url: new URL("buildroot-bzimage.bin", V86_ASSETS).href, async: false },
    cmdline: BOOT_CMDLINE,
    // The guest's mux_server binary and start script, served over the 9p
    // filesystem v86 exposes to the guest as tag "host9p".
    filesystem: {
      baseurl: new URL("./fs/", V86_ASSETS).href,
      basefs: new URL("./fs/fs.json", V86_ASSETS).href,
    },
    autostart: true,
    // No screen, no input devices: the terminal is the only interface, and the
    // pane owns the keyboard.
    disable_keyboard: true,
    disable_mouse: true,
    disable_speaker: true,
  });

  // Attach before anything else. The kernel starts printing during the
  // constructor, and a listener added a tick later loses the whole boot.
  let muxReady = false;
  const batch = new SerialBatch((bytes) => {
    // Forward everything to the Rust serial link: it renders boot text via
    // this same path until the in-guest mux server signals ready, then
    // switches to protocol framing.
    const bindings = window.wasmBindings;
    if (bindings && typeof bindings.z3rm_v86_serial_bytes === "function") {
      bindings.z3rm_v86_serial_bytes(bytes);
    }
    if (!muxReady) {
      muxBootText.push(...bytes);
      const text = decoder.decode(Uint8Array.from(muxBootText));
      if (text.includes("Z3RM_MUX_READY")) {
        muxReady = true;
        muxBootText = [];
      } else if (muxBootText.length > 8192) {
        muxBootText = muxBootText.slice(-4096);
      } else {
        renderSerial(bytes);
      }
    }
  });
  emulator.add_listener("serial0-output-byte", (byte) => batch.push(byte));

  // Once the guest shell answers, replace it with the mux server: serial0
  // becomes the mux protocol transport the client speaks across.
  let muxStartTyped = false;
  emulator.add_listener("serial0-output-byte", (byte) => {
    if (muxStartTyped) return;
    muxBootText.push(byte);
    if (muxBootText.length > 4096) muxBootText = muxBootText.slice(-2048);
    const text = decoder.decode(Uint8Array.from(muxBootText), { stream: false });
    if (text.includes("~%")) {
      muxStartTyped = true;
      setTimeout(() => {
        const cmd = "/mnt/start-mux.sh\n";
        for (const ch of cmd) {
          emulator.bus.send("serial0-input", ch.charCodeAt(0));
        }
      }, 300);
    }
  });

  window.__z3rm_v86 = {
    emulator,
    send(bytes) {
      for (const byte of bytes) {
        emulator.bus.send("serial0-input", byte);
      }
    },
  };

}

boot().catch((error) => {
  console.error("v86 bridge failed to start:", error);
});