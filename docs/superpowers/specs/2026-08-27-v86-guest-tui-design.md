# v86 Guest mux_server and TUI Landing Design

## Goal

The browser client must connect to the real `mux_server` process running inside
v86 Linux. The guest also carries a terminal-only z3rm landing application.
The landing application can render images, accept mouse and wheel input, expose
download links, and copy text through the same server-canonical mux path.

## Existing transport

The guest server is a statically linked `i686-unknown-linux-musl` binary. The
browser supplies it through v86's HTTP-backed 9p filesystem. The guest replaces
the boot shell with `z3rm-server --serial /dev/ttyS0`.

The client retains `mux::MuxDomain::connect_in_memory` only as a byte-carrier
adapter. A `MemStream` pair is pumped across v86 serial0. Bytes before
`Z3RM_MUX_READY` are boot text; bytes after the marker are length-delimited
protobuf mux frames. The server-side PTY and alacritty terminal remain the
single authority for process state, terminal modes, grid, and scrollback.

## Guest TUI

Add a small `z3rm_guest_tui` binary with no runtime dependency on a host GUI.
It is statically compiled for i686-musl and packaged into the same 9p tree as
the server. The program:

- draws the landing content with ANSI/DEC sequences;
- switches to the alternate screen and enables SGR mouse reporting;
- maps wheel events to page scrolling and button regions to actions;
- emits Kitty Graphics Protocol transmit/display sequences for bundled PNGs;
- emits OSC 8 hyperlinks with the `z3rm-download:` scheme for downloadable
  artifacts;
- emits OSC 52 for explicit copy actions;
- exits cleanly on the configured quit key and restores terminal modes.

The TUI is the only landing-page implementation inside the guest. The wasm
client supplies terminal chrome and renders the guest's authoritative grid.

## Media and action protocol

Extend `mux_protocol` notifications with a `PaneMedia` event. It contains the
pane id, monotonically increasing media sequence, Kitty image id, image format,
cell row/column placement, chunk bytes, and a final-chunk flag. The server
parser recognizes complete Kitty APC sequences before passing ordinary bytes
to alacritty. It reassembles Kitty continuation chunks per pane and publishes
media notifications in the same order as the corresponding PTY output.

Ordinary text and OSC 8 hyperlinks continue through alacritty and the existing
`Cell.hyperlink` field. The client treats only `z3rm-download:` links as local
download actions: it requests the named static artifact, creates a Blob URL,
and activates an anchor with a download filename. Other hyperlinks remain
ordinary links.

The existing emulator clipboard hook handles OSC 52. A clipboard notification
carries text to the client; the browser writes it only from an explicit user
copy gesture, and displays a visible failure if browser permission is denied.

Kitty media is rendered as a GPUI image layer positioned from the server
reported cell coordinates. Media is keyed by `(pane_id, image_id)` and removed
when the server sends the corresponding delete operation or pane lifecycle
removal. A missing or malformed sequence is logged and does not corrupt grid
bytes.

## Input and scrolling

The existing mux `SendInput` request remains the only path from client to guest
PTY. When the server terminal mode reports mouse tracking, the client encodes
click, drag, and wheel events as SGR mouse sequences and sends them through the
same request. Without mouse tracking, wheel events use the existing server
scrollback request. The TUI owns page scroll state while in its alternate
screen; the server continues to own scrollback state outside it.

## Loading progress

The standalone wasm page contains a Proto-UI-styled status surface with a
stage label, determinate bar, percent, transferred/total bytes, and current
bytes-per-second. A fetch wrapper is installed before the Trunk module script.
It clones responses and reads the clone body only for measurement, leaving the
original response available to WebAssembly or v86. Content-Length, when
present, supplies the denominator; otherwise the bar is indeterminate and
shows measured bytes per second without inventing a percentage.

Tracked stages are:

1. Loading z3rm WebAssembly.
2. Loading the Linux kernel and v86 runtime.
3. Loading the guest 9p files, including `mux_server` and the TUI.
4. Waiting for `Z3RM_MUX_READY`.
5. Connecting the mux protocol.
6. Ready.

Errors replace the bar with the failed resource/stage and a retry control.
The surface hides only after `data-gpui-ready="true"` and the first guest
pane snapshot has been rendered.

## Failure handling

- If 9p mount or the guest binary fails, the boot terminal remains visible and
  shows the guest's actual stderr/output.
- If the ready marker is absent, the client times out with an actionable stage
  error and never sends mux frames into the shell.
- If a frame is malformed, the client closes the serial link and reports a
  protocol error rather than attempting byte resynchronization over an
  untrusted stream.
- If Kitty media is unsupported by a terminal surface, text and links still
  render and the download/copy actions remain available.

## Verification

Unit tests cover Kitty continuation parsing, OSC 8 download URI extraction,
OSC 52 decoding, media notification framing, and progress accounting at zero,
unknown, and complete content lengths. A real browser test must demonstrate:

- the visible progress stage and measured transfer rate;
- the GPUI z3rm shell connected to a guest-owned PTY;
- TUI text and Kitty image rendering;
- wheel scrolling and mouse activation;
- a browser download from a TUI link;
- copy through the browser clipboard;
- no in-process `WasmMuxServer` remains on the client path.
