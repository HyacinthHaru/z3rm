# 0003 - UDP Resilient Transport (Mosh-Inspired)

**Status:** Accepted

## Context

SSH over TCP disconnects on network changes, IP changes, or brief outages — killing the mux session. Mosh (Mobile Shell) solves this with UDP-based State Synchronization Protocol (SSP): datagram-based, per-packet AES-OCB encryption, roaming via client/server epoch synchronization, predictive local echo. z3rm needs equivalent transport-layer resilience for the mux protocol, orthogonal to the multiplexing layer itself.

## Decision

Implement a mosh-inspired UDP transport as a post-foundation enhancement (Phase 10+):
- Per-packet AEAD (AES-256-GCM or ChaCha20-Poly1305) with per-packet nonce (epoch + sequence)
- Stateless roaming: client roams by sending packets from new IP/port with incremented epoch; server accepts if epoch increments and MAC verifies
- Predictive local echo for keystrokes (optional, Phase 11+)
- SSP-style state synchronization for screen state (Phase 10), not keystroke prediction initially
- Transport layer is pluggable: TCP (SSH) for Day 0, UDP (mosh-style) for Phase 10+

Transport is a separate crate (`z3rm_transport`) behind a `Transport` trait. Mux protocol (frames, sessions, windows) is transport-agnostic.

## Consequences

- **Positive:** Transport-layer resilience orthogonal to multiplexing. SSH works Day 0; UDP resilience is additive. No mux protocol changes needed for transport swap. Stateless roaming survives NAT rebinding, Wi-Fi/cellular handoff.
- **Negative:** UDP path requires kernel support (UDP sockets), firewall traversal considerations, NAT traversal (STUN/TURN/ICE) for direct connections. AEAD per packet adds CPU overhead vs TCP TLS session. Predictive echo adds speculative execution complexity.
- **Mitigation:** Phase 10 (transport) after Phase 1-9 (core mux). STUN/TURN as separate Phase 11. Hardware AES-NI / ChaCha20-Poly1305 mitigates crypto cost.