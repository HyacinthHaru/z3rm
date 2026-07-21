# Spec Compliance Audit — Rounds 2 & 3

Date: 2026-07-22

## Round 2: Deep Section Verification

| Section | Check | Result |
|---------|-------|--------|
| §3.1 render path | DisplayOnly + write_output + send_input (no local PTY) | PASS |
| §3.2 MuxTransport | Local + Ssh(SshSession) variants | PASS |
| §3.3 generation counter | AtomicU64 per pane, GridDiffRing 64 entries | PASS |
| §3.3 DEC-2026 | BSU/ESU parsing + AdaptiveCoalescer | PASS |
| §3.8 stale socket | remove_file before spawn | PASS |
| §3.9 protocol version | PROTOCOL_VERSION in envelope, version_compatible check | PASS |
| §4 shadow snapshot | WAL-first, SeqNo, D_MAX, single-writer thread | PASS |
| §5.2 QuickJS thread | execute_in_thread via thread::Builder | PASS |
| §5.3 capabilities | memory_limit_mb, cpu_budget_ms, IoTokenBucket | PASS |
| §15.7 core commands | 5 register_action calls (Split/Close/Zoom/NewTab) | PASS |
| §16.1 daemon auto-spawn | ensure_daemon_running → spawn_daemon → wait_for_socket | PASS |
| §16.4 scrollback | handle_fetch_scrollback + scrollback_version | PASS |
| §16.9 ring buffer | GridDiffRing default 64 entries | PASS |
| §16.12 reconnect | watch_daemon_connection + show_daemon_connection_lost | PASS |

## Round 3: Data Flow Integrity & Edge Cases

| Check | Result | Notes |
|-------|--------|-------|
| Input path: keystroke → keystroke_to_bytes → MuxDomain::send_input | PASS | Never touches local PTY |
| DisplayOnly write_to_pty is no-op | PASS | TerminalType::DisplayOnly → None for pty_tx |
| PaneRemoved → CloseRequested (zombie prevention) | PASS | Emits event, workspace closes item |
| Double-render prevention | PASS | write_snapshot_to_terminal only on gen==0 or reconnect |
| TerminalBounds proper metrics | PASS | 8.4px cell, 18px line (not DEBUG 5px) |
| Multi-client input serialization | PASS | Server-side PTY writes are serial per pane |

## Conclusion

All 3 rounds complete. 0 blocking issues found.
Core architecture fully compliant with spec §3.1 exception (in-place render-path).
