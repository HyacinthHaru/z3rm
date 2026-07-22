# Mosh Competitive Research

## Architecture Overview

Mosh (Mobile Shell) is a remote terminal protocol focused on **resilient transport** over lossy/high-latency networks. Key insight: **transport-layer resilience is orthogonal to multiplexing**. Mosh uses UDP with per-packet AEAD, stateless roaming, and local-echo prediction.

### SSP (State Synchronization Protocol) Transport Design
- **UDP-based transport**: Replaces TCP with packetized UDP. Each packet is self-contained (no stream state). Survives packet loss, reordering, duplicate delivery. z3rm's transport resolver (Plan 19/25) should adopt: **transport resilience is a separate layer** from mux-protocol/mux-server. Mux-protocol rides over any transport (TCP, UDP, WebRTC, QUIC).
- **Per-packet AEAD (AES-OMF)**: Every packet authenticated + encrypted. Mosh uses AES-128 in OMF mode. z3rm transport (Plan 25) should require **per-packet AEAD** regardless of underlying transport (QUIC natively provides DTLS; TCP needs TLS; UDP needs custom AEAD).

### Stateless Roaming
- **Client IP/port changes transparent**: Server maintains per-session keys, not per-connection state. Client can change IP/port mid-session (WiFi→cellular→WiFi) without reconnecting. z3rm's mux-server (Plan 10) should support **stateless roaming**: client identity tied to session ID + auth token (not source IP). Client reconnect retries handshake; server resumes session state.
- **Sequence numbers for ordering**: Each packet has monotonically increasing sequence number. Server ignores out-of-order/duplicate packets. z3rm transport should use **per-session monotonically increasing sequence numbers** for idempotent packet processing.

### Frame-Rate Control (Bandwidth Adaptation)
- **Control byte rate per packet**: Each message type declares max rate (e.g., terminal output: unlimited; cursor position: 30/s; screen size: rare). Server throttles high-rate messages to client. z3rm's mux-protocol (Plan 9) should include **per-message-type rate hints**; mux-server enforces per-client rate limits.
- **Frame-rate control**: Display state messages batched per "frame" (default 20fps = 50ms). Client interpolates. z3rm grid→client notifications should batch at **configurable frame rate** (default 60fps).

### Local-Echo Prediction
- **Local echo predictions**: Client predicts screen changes for common keystrokes (typed text, backspace, arrow keys) before server round-trip. Predicted state shows immediately; reconciled on server reply. Mismatch → rollback + re-sync. z3rm's terminal-view should support **optional local-echo prediction** for high-latency sessions (toggle per-session). Predictions keyed by keystroke → expected screen diff.

### State Synchronization Protocol (SSP)
- **SSP packets**: Client→Server: keystrokes, window size, flow control. Server→Client: terminal output, screen state, prompt echo. z3rm's mux-protocol (Plan 9) should adopt **similar message taxonomy**: input event, output event, flow control, window resize. Transport-agnostic.
- **State-based, not delta-based**: Server sends full screen state periodically (every N frames or on diff). Client can always render current state from latest snapshot. z3rm's terminal-view shadow snapshot (Plan 13) should support **pull-based full state** (client requests snapshot) in addition to push diffs.

## Lessons for z3rm

| Mosh Pattern | z3rm Adaptation |
|--------------|-----------------|
| Transport orthogonal to mux | **Transport resolver tier** (Plan 19/25) decoupled from mux-protocol |
| UDP + per-packet AEAD | **Per-packet AEAD required** for UDP transport; TLS for TCP; DTLS for QUIC |
| Stateless roaming | **Session ID + auth token** replaces source IP binding (Plan 10) |
| Sequence numbers | **Per-session monotonic sequence** for idempotent processing |
| Frame-rate control | **Per-message-type rate hints** in mux-protocol (Plan 9) |
| Local-echo prediction | **Optional local echo** in terminal-view for high-latency sessions |
| State-based SSP | **Shadow snapshot** (Plan 13): full state pull + push diffs |
| Frame batching | **60fps notification batching** (Plan 10): grid→client |

## Key Source Files (Mosh)
- `gen/mosh-ncurses/terminal/terminal.cc` — Mosh terminal emulator
- `src/terminal/terminal.cc` — User terminal emulator (prediction)
- `src/completeterminal.cc` — Completion/prediction logic
- `src/networktransport.cc` — UDP transport, AEAD, roaming
- `src/transport.cc` — Transport framework
- `src/userinput.cc` — Keystroke → SSP packet
- `src/pty.cc` — PTY handling (driven by server process)

## Competitive Positioning Note
Mosh demonstrates **resilient transport layer** as a separable concern. z3rm's transport resolver (Plan 19/25) should treat transport resilience as **orthogonal to multiplexer logic**: mux-protocol rides over any transport; transport layer handles reliability/roaming/security independently.