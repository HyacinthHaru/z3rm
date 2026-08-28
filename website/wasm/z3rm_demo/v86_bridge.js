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

/// The boot page is intentionally dependency-free. This API is installed by
/// this classic script before Trunk's module script runs, so it can observe
/// both the module's fetches and the guest's XMLHttpRequests.
(function installLoadingProgress() {
  const stages = new Map();
  const samples = [];
  const state = {
    currentStage: "starting",
    gpuiReady: false,
    firstPaneSnapshotReady: false,
    error: null,
    lastLoaded: 0,
  };
  let requestNumber = 0;
  let refreshTimer = null;

  function finiteNumber(value) {
    const number = Number(value);
    return Number.isFinite(number) && number >= 0 ? number : 0;
  }

  function htmlRoot() {
    return document.documentElement;
  }

  function parseTotal(value) {
    const total = Number.parseInt(value || "", 10);
    return Number.isFinite(total) && total > 0 ? total : 0;
  }

  function responseTotal(headers) {
    if (!headers || typeof headers.get !== "function") {
      return 0;
    }
    const range = headers.get("Content-Range") || "";
    const rangeMatch = range.match(/\/([0-9]+)\s*$/);
    return parseTotal(rangeMatch ? rangeMatch[1] : headers.get("Content-Length"));
  }

  function resourceUrl(input) {
    try {
      const raw = typeof input === "string" || input instanceof URL ? String(input) : input?.url;
      return new URL(raw || "unknown-resource", document.baseURI).href;
    } catch {
      return String(input || "unknown-resource");
    }
  }

  function resourceStage(kind, url) {
    requestNumber += 1;
    return `${kind} ${url} (${requestNumber})`;
  }

  function aggregate() {
    let loaded = 0;
    let total = 0;
    let indeterminate = stages.size === 0;
    for (const stage of stages.values()) {
      loaded += stage.loaded;
      if (stage.total > 0) {
        total += stage.total;
      } else {
        indeterminate = true;
      }
    }
    return { loaded, total, indeterminate };
  }

  function rollingRate(now) {
    while (samples.length > 0 && now - samples[0].at > 3000) {
      samples.shift();
    }
    if (samples.length === 0) {
      return 0;
    }
    let bytes = 0;
    for (const sample of samples) {
      bytes += sample.bytes;
    }
    const elapsed = Math.max(100, now - samples[0].at);
    return (bytes * 1000) / elapsed;
  }

  function formatBytes(bytes) {
    return `${Math.round(bytes).toLocaleString()} B`;
  }

  function formatRate(rate) {
    // Keep the unit explicit even for large downloads: this is a byte rate,
    // not an estimate based on elapsed guest stages.
    return `${Math.round(rate).toLocaleString()} B/s`;
  }

  function render() {
    const progress = document.getElementById("loading-progress");
    const bar = document.getElementById("loading-progress-bar");
    const label = document.getElementById("loading-progress-label");
    const detail = document.getElementById("loading-progress-detail");
    const retry = document.getElementById("loading-progress-retry");
    if (!progress || !bar || !label || !detail) {
      return;
    }
    const fill = bar.firstElementChild;
    const now = performance.now();
    const { loaded, total, indeterminate } = aggregate();
    const ready = state.gpuiReady && state.firstPaneSnapshotReady && !state.error;
    progress.dataset.state = state.error ? "error" : ready ? "ready" : "loading";
    progress.setAttribute("aria-busy", String(!ready));
    progress.setAttribute("aria-hidden", String(ready));

    if (state.error) {
      label.textContent = "Unable to load z3rm";
      detail.textContent = `${state.error.stage}: ${state.error.message}`;
      if (retry) {
        retry.hidden = false;
      }
      bar.dataset.indeterminate = "true";
      fill?.style.removeProperty("width");
      bar.style.removeProperty("--loading-progress-width");
      bar.removeAttribute("aria-valuenow");
      bar.setAttribute("aria-valuetext", "Loading failed");
      return;
    }

    if (retry) {
      retry.hidden = true;
    }
    if (ready) {
      label.textContent = "z3rm is ready";
      detail.textContent = "The first authoritative pane snapshot is rendering.";
    } else {
      label.textContent = `Loading ${state.currentStage}`;
      detail.textContent = indeterminate
        ? `${formatBytes(loaded)} loaded · ${formatRate(rollingRate(now))}`
        : `${formatBytes(loaded)} of ${formatBytes(total)} · ${formatRate(rollingRate(now))}`;
    }

    if (indeterminate || total <= 0) {
      bar.dataset.indeterminate = "true";
      fill?.style.removeProperty("width");
      bar.style.removeProperty("--loading-progress-width");
      bar.removeAttribute("aria-valuenow");
      bar.setAttribute("aria-valuetext", "Loading");
    } else {
      const percent = Math.min(100, (loaded / total) * 100);
      bar.dataset.indeterminate = "false";
      fill?.style.setProperty("width", `${percent}%`);
      bar.style.setProperty("--loading-progress-width", `${percent}%`);
      bar.setAttribute("aria-valuenow", String(Math.round(percent)));
      bar.setAttribute("aria-valuetext", `${Math.round(percent)} percent loaded`);
    }
  }

  function recordStage(name, loaded, total) {
    const stageName = String(name || "loading");
    const next = { loaded: finiteNumber(loaded), total: finiteNumber(total) };
    const previous = stages.get(stageName);
    stages.set(stageName, next);
    state.currentStage = stageName;

    const currentLoaded = aggregate().loaded;
    if (currentLoaded >= state.lastLoaded) {
      const delta = currentLoaded - state.lastLoaded;
      if (delta > 0) {
        samples.push({ at: performance.now(), bytes: delta });
      }
    } else if (previous) {
      // A reused stage name can represent a fresh request. Reset the rolling
      // baseline instead of reporting a negative download rate.
      samples.length = 0;
    }
    state.lastLoaded = currentLoaded;
    render();
  }

  function markError(stage, message) {
    if (state.error) {
      return;
    }
    state.error = { stage: String(stage || "loading"), message: String(message || "request failed") };
    render();
  }

  function markReady() {
    state.gpuiReady = true;
    htmlRoot().setAttribute("data-gpui-ready", "true");
    render();
  }

  function markFirstPaneSnapshotReady() {
    state.firstPaneSnapshotReady = true;
    htmlRoot().setAttribute("data-first-pane-snapshot-ready", "true");
    render();
  }

  const api = {
    stage: recordStage,
    ready: markReady,
    firstPaneSnapshotReady: markFirstPaneSnapshotReady,
    error: markError,
    retry() {
      window.location.reload();
    },
  };
  window.__z3rm_progress = api;

  function bindDom() {
    const retry = document.getElementById("loading-progress-retry");
    retry?.addEventListener("click", () => api.retry());
    render();
    if (refreshTimer === null) {
      refreshTimer = window.setInterval(() => {
        if (state.gpuiReady && state.firstPaneSnapshotReady) {
          window.clearInterval(refreshTimer);
          refreshTimer = null;
        } else {
          render();
        }
      }, 250);
    }
  }

  if (document.readyState === "loading") {
    document.addEventListener("DOMContentLoaded", bindDom, { once: true });
  } else {
    bindDom();
  }

  function installFetchTracker() {
    if (typeof window.fetch !== "function") {
      return;
    }
    const originalFetch = window.fetch;
    window.fetch = function trackedFetch(input, init) {
      const url = resourceUrl(input);
      const stage = resourceStage("fetch", url);
      recordStage(stage, 0, 0);
      let request;
      try {
        request = originalFetch.call(this, input, init);
      } catch (error) {
        markError(stage, error instanceof Error ? error.message : String(error));
        throw error;
      }
      return Promise.resolve(request).then(
        (response) => {
          const total = responseTotal(response.headers);
          recordStage(stage, 0, total);
          if (!response.ok) {
            markError(stage, `HTTP ${response.status} while loading ${url}`);
          }
          void trackFetchBody(response, stage, total);
          return response;
        },
        (error) => {
          markError(stage, error instanceof Error ? error.message : String(error));
          throw error;
        },
      );
    };
  }

  async function trackFetchBody(response, stage, total) {
    let clone;
    try {
      // Reading only a clone leaves the response body available to Trunk,
      // WebAssembly, and every caller of fetch.
      clone = response.clone();
    } catch {
      return;
    }
    const reader = clone.body?.getReader();
    if (!reader) {
      return;
    }
    let loaded = 0;
    try {
      while (true) {
        const result = await reader.read();
        if (result.done) {
          break;
        }
        if (result.value) {
          loaded += result.value.byteLength;
          recordStage(stage, loaded, total);
        }
      }
      if (loaded > 0 || total > 0) {
        recordStage(stage, loaded, total);
      }
    } catch (error) {
      markError(stage, error instanceof Error ? error.message : String(error));
    }
  }

  function installXhrTracker() {
    const Xhr = window.XMLHttpRequest;
    if (!Xhr || !Xhr.prototype || typeof Xhr.prototype.open !== "function" || typeof Xhr.prototype.send !== "function") {
      return;
    }
    const prototype = Xhr.prototype;
    const originalOpen = prototype.open;
    const originalSend = prototype.send;
    const trackerState = new WeakMap();

    prototype.open = function trackedOpen(method, url) {
      trackerState.set(this, {
        method: String(method || "GET").toUpperCase(),
        url: resourceUrl(url),
        listeners: null,
      });
      return originalOpen.apply(this, arguments);
    };

    prototype.send = function trackedSend(body) {
      const xhr = this;
      const request = trackerState.get(xhr);
      if (!request || typeof xhr.addEventListener !== "function") {
        return originalSend.call(this, body);
      }
      request.stage = resourceStage(`xhr ${request.method}`, request.url);
      let loaded = 0;
      let total = 0;
      const headerTotal = () => {
        try {
          const range = xhr.getResponseHeader("Content-Range") || "";
          const match = range.match(/\/([0-9]+)\s*$/);
          return parseTotal(match ? match[1] : xhr.getResponseHeader("Content-Length"));
        } catch {
          return 0;
        }
      };
      const update = (event) => {
        if (Number.isFinite(event?.loaded)) {
          loaded = Math.max(loaded, event.loaded);
        }
        if (event?.lengthComputable && event.total > 0) {
          total = event.total;
        } else {
          total = total || headerTotal();
        }
        recordStage(request.stage, loaded, total);
      };
      const start = () => {
        total = headerTotal();
        recordStage(request.stage, 0, total);
      };
      const finish = (event) => {
        update(event);
        if (loaded === 0) {
          try {
            if (xhr.response && Number.isFinite(xhr.response.byteLength)) {
              loaded = xhr.response.byteLength;
            }
          } catch {
            // Accessing a response can fail for an invalid responseType.
          }
        }
        recordStage(request.stage, loaded, total);
        if (xhr.status >= 400) {
          markError(request.stage, `HTTP ${xhr.status} while loading ${request.url}`);
        }
        cleanup();
      };
      const fail = () => {
        markError(request.stage, `request failed while loading ${request.url}`);
        cleanup();
      };
      const cleanup = () => {
        if (!request.listeners) return;
        for (const [type, listener] of request.listeners) {
          xhr.removeEventListener(type, listener);
        }
        request.listeners = null;
      };
      request.listeners = [
        ["loadstart", start],
        ["progress", update],
        ["load", finish],
        ["error", fail],
        ["abort", fail],
        ["timeout", fail],
      ];
      for (const [type, listener] of request.listeners) {
        xhr.addEventListener(type, listener);
      }
      recordStage(request.stage, 0, 0);
      try {
        return originalSend.call(this, body);
      } catch (error) {
        markError(request.stage, error instanceof Error ? error.message : String(error));
        cleanup();
        throw error;
      }
    };
  }

  installFetchTracker();
  installXhrTracker();
})();

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
  const url = new URL("libv86.js", V86_ASSETS).href;
  window.__z3rm_progress.stage(`v86: loading ${url}`, 0, 0);
  return new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = url;
    script.onload = () => resolve();
    script.onerror = () => reject(new Error(`could not load ${url}`));
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
  const progress = window.__z3rm_progress;
  progress.stage("v86: loading the guest emulator", 0, 0);
  await loadV86Library();
  if (typeof window.V86 !== "function") {
    throw new Error("libv86.js loaded without exposing V86");
  }
  progress.stage("v86: creating the guest", 0, 0);

  // A failed Rust module should not prevent the serial fallback from showing
  // boot output, but it must remain visible rather than silently hanging.
  void waitForWasmBindings().then((bindings) => {
    if (!bindings) {
      progress.error("GPUI WebAssembly", "bindings did not initialize within 15 seconds");
    }
  });

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

  progress.stage("v86: booting the guest", 0, 0);
  emulator.add_listener("emulator-stopped", () => {
    if (!document.documentElement.hasAttribute("data-first-pane-snapshot-ready")) {
      progress.error("v86 guest", "the emulator stopped before the first pane snapshot was ready");
    }
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
      try {
        bindings.z3rm_v86_serial_bytes(bytes);
      } catch (error) {
        progress.error("GPUI WebAssembly serial bridge", error instanceof Error ? error.message : String(error));
      }
    }
    if (!muxReady) {
      muxBootText.push(...bytes);
      const text = decoder.decode(Uint8Array.from(muxBootText));
      if (text.includes("Z3RM_MUX_READY")) {
        muxReady = true;
        muxBootText = [];
        progress.stage("v86: mux server ready", 0, 0);
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
      progress.stage("v86: starting the mux server", 0, 0);
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

function startBoot() {
  boot().catch((error) => {
    const message = error instanceof Error ? error.message : String(error);
    window.__z3rm_progress.error("v86 guest", message);
    console.error("v86 bridge failed to start:", error);
  });
}

// The tracker above installs during parsing, while boot waits for the body so
// the serial fallback and loading surface are present before the guest speaks.
if (document.readyState === "loading") {
  document.addEventListener("DOMContentLoaded", startBoot, { once: true });
} else {
  startBoot();
}