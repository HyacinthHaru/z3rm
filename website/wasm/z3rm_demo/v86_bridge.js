(() => {
  const MAX_BOOT_BUFFER = 1024 * 1024;
  const MAX_TERMINAL_TEXT = 200_000;
  const pendingInput = [];
  const outputBatch = [];
  const bridgeBacklog = [];
  let outputBatchBytes = 0;
  let bridgeBacklogBytes = 0;
  let emulator = null;
  let flushScheduled = false;
  const decoder = new TextDecoder();

  const terminal = () => document.getElementById("boot-terminal-output");
  const shell = () => document.getElementById("boot-terminal");

  const setStatus = (status) => {
    document.documentElement.dataset.v86Status = status;
    const node = document.getElementById("boot-terminal-status");
    if (node) node.textContent = status;
  };

  const renderSerial = (bytes) => {
    const node = terminal();
    if (!node) return;
    node.textContent += decoder.decode(bytes, { stream: true });
    if (node.textContent.length > MAX_TERMINAL_TEXT) {
      node.textContent = node.textContent.slice(-MAX_TERMINAL_TEXT);
    }
    shell()?.scrollTo(0, shell().scrollHeight);
  };

  const flushInput = () => {
    if (!emulator) return;
    for (const bytes of pendingInput.splice(0)) {
      emulator.serial_send_bytes(0, bytes);
    }
  };

  window.z3rmV86SerialInput = (bytes) => {
    const copy = Uint8Array.from(bytes);
    if (emulator) emulator.serial_send_bytes(0, copy);
    else pendingInput.push(copy);
  };

  const sendBacklog = () => {
    const sink = window.z3rmPushSerialOutput;
    if (typeof sink !== "function" || bridgeBacklogBytes === 0) return false;
    const chunk = new Uint8Array(bridgeBacklogBytes);
    let offset = 0;
    for (const bytes of bridgeBacklog.splice(0)) {
      chunk.set(bytes, offset);
      offset += bytes.length;
    }
    bridgeBacklogBytes = 0;
    sink(chunk);
    return true;
  };

  const retryBridge = () => {
    if (!sendBacklog()) setTimeout(retryBridge, 25);
  };

  const flushOutput = () => {
    flushScheduled = false;
    if (outputBatchBytes === 0) return;
    const chunk = new Uint8Array(outputBatchBytes);
    let offset = 0;
    for (const bytes of outputBatch.splice(0)) {
      chunk.set(bytes, offset);
      offset += bytes.length;
    }
    outputBatchBytes = 0;
    renderSerial(chunk);

    const sink = window.z3rmPushSerialOutput;
    if (typeof sink === "function") {
      sendBacklog();
      sink(chunk);
    } else {
      bridgeBacklog.push(chunk);
      bridgeBacklogBytes += chunk.length;
      while (bridgeBacklogBytes > MAX_BOOT_BUFFER && bridgeBacklog.length > 0) {
        bridgeBacklogBytes -= bridgeBacklog.shift().length;
      }
      setTimeout(retryBridge, 25);
    }
  };

  const scheduleOutputFlush = () => {
    if (flushScheduled) return;
    flushScheduled = true;
    queueMicrotask(flushOutput);
  };

  const onSerialByte = (byte) => {
    outputBatch.push(Uint8Array.of(byte & 0xff));
    outputBatchBytes += 1;
    scheduleOutputFlush();
  };

  const ensureRuntime = async () => {
    if (typeof window.V86 === "function") return;
    const source = await fetch("./v86/libv86.js").then((response) => {
      if (!response.ok) throw new Error(`v86 runtime HTTP ${response.status}`);
      return response.text();
    });
    // Self-hosted, version-locked asset verified by public/v86/SHA256SUMS.txt.
    (0, eval)(source);
  };

  const welcomeScript = [
    "#!/bin/sh",
    "clear",
    "printf '\\033[1;36m'",
    "echo '     _____ _____                 '",
    "echo '    |__  /|___ / _ __ _ __ ___  '",
    "echo '      / /   |_ \\| .__| ._ \\` _ \\\\ '",
    "echo '     / /_ ___) | |  | | | | | |'",
    "echo '    /____|____/|_|  |_| |_| |_|'",
    "echo",
    "printf '\\033[0m'",
    "printf '\\033[1mYour shells outlive the window.\\033[0m\\n\\n'",
    "echo 'This is a real Linux VM running in your browser.'",
    "echo 'The terminal is rendered by Z3rm GPUI through the'",
    "echo 'same mux protocol used in production.'",
    "echo",
    "printf '\\033[90m── try it ──────────────────────────────────\\033[0m\\n\\n'",
    "printf '  \\033[33muname -a\\033[0m          kernel info\\n'",
    "printf '  \\033[33mcat /proc/cpuinfo\\033[0m CPU details\\n'",
    "printf '  \\033[33mfree -h\\033[0m           memory usage\\n'",
    "printf '  \\033[33mls /mnt/\\033[0m          9p shared filesystem\\n\\n'",
    "printf '\\033[90m───────────────────────────────────────────\\033[0m\\n\\n'",
  ].join("\n");

  const boot = async () => {
    await ensureRuntime();
    if (typeof window.V86 !== "function") {
      throw new Error("v86 runtime did not register window.V86");
    }

    setStatus("loading");
    emulator = new window.V86({
      wasm_path: "./v86/v86.wasm",
      memory_size: 64 * 1024 * 1024,
      vga_memory_size: 4 * 1024 * 1024,
      bios: { url: "./v86/seabios.bin" },
      bzimage: { url: "./v86/buildroot-bzimage.bin" },
      cmdline: "console=ttyS0 root=/dev/ram0 rw",
      autostart: true,
      disable_keyboard: true,
      disable_mouse: true,
      disable_speaker: true,
      serial_console: { type: "none" },
      screen: { container: document.getElementById("v86-screen") || undefined },
      filesystem: {},
    });
    let shellReady = false;
    const promptPattern = /[#$%>] $/;
    const detectPrompt = (byte) => {
      onSerialByte(byte);
      if (shellReady) return;
      const node = terminal();
      if (!node) return;
      const tail = node.textContent.slice(-80);
      if (promptPattern.test(tail)) {
        shellReady = true;
        emulator.create_file(
          "welcome.sh",
          new TextEncoder().encode(welcomeScript),
        ).then(() => {
          const cmd = "sh /mnt/welcome.sh\r";
          emulator.serial_send_bytes(0, new TextEncoder().encode(cmd));
        }).catch((error) => {
          console.warn("welcome inject failed:", error);
        });
      }
    };
    emulator.add_listener("serial0-output-byte", detectPrompt);
    emulator.add_listener("emulator-ready", () => setStatus("ready"));
    emulator.add_listener("emulator-started", () => {
      flushInput();
      setStatus("running");
    });
    window.z3rmV86 = emulator;
  };

  const keyBytes = (event) => {
    if (event.ctrlKey && event.key.length === 1) {
      const code = event.key.toUpperCase().charCodeAt(0) - 64;
      if (code > 0 && code < 32) return Uint8Array.of(code);
    }
    const named = {
      Enter: "\r", Backspace: "\x7f", Tab: "\t", Escape: "\x1b",
      ArrowUp: "\x1b[A", ArrowDown: "\x1b[B", ArrowRight: "\x1b[C", ArrowLeft: "\x1b[D",
      Home: "\x1b[H", End: "\x1b[F", Delete: "\x1b[3~",
    };
    const text = named[event.key] ?? (event.key.length === 1 ? event.key : "");
    return new TextEncoder().encode(text);
  };

  addEventListener("DOMContentLoaded", () => {
    const host = shell();
    host?.addEventListener("keydown", (event) => {
      const bytes = keyBytes(event);
      if (bytes.length > 0) {
        event.preventDefault();
        window.z3rmV86SerialInput(bytes);
      }
    });
    host?.addEventListener("pointerdown", () => host.focus());
    host?.focus();
  });

  const readyObserver = new MutationObserver(() => {
    if (document.documentElement.dataset.gpuiReady === "true") {
      document.documentElement.classList.add("gpui-ready");
    }
  });
  readyObserver.observe(document.documentElement, { attributes: true });

  const waitForGpuiCanvas = () => new Promise((resolve) => {
    const started = performance.now();
    const poll = () => {
      const gpuiCanvas = document.querySelector("body > canvas");
      if (gpuiCanvas || performance.now() - started >= 3000) {
        resolve();
      } else {
        requestAnimationFrame(poll);
      }
    };
    if (document.readyState === "loading") {
      addEventListener("DOMContentLoaded", poll, { once: true });
    } else {
      poll();
    }
  });

  waitForGpuiCanvas().then(boot).catch((error) => {
    const message = error instanceof Error ? error.message : String(error);
    setStatus(`failed: ${message}`);
    console.error("failed to boot v86", error);
  });
})();
