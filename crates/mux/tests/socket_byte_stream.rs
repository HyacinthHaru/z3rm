//! # Socket byte-stream framing tests
//!
//! 这些测试只覆盖 mux_protocol frame/unframe 在原始 socket 字节流上的行为
//! (部分帧、字节边界切片、半关闭、畸形前缀)。它们**不**起 mux_server 子进程,
//! 因此不是端到端测试。真正的 e2e 在 crates/mux/tests/e2e.rs。
//!
//! Every test decodes the envelope it expects. Raw byte counts cannot tell a
//! correct frame from garbage of the same length, and assertions that used to
//! stop at `n > 0` / `n >= 0` let framing regressions pass silently.

use anyhow::{Context, Result};
use mux_protocol::proto::envelope::Payload as EnvelopePayload;
use mux_protocol::proto::notification::Event as NotificationEvent;
use mux_protocol::proto::request::Body as RequestBody;
use mux_protocol::{Envelope, Notification, Request};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

/// Generous enough for a slow CI box, tight enough that a stalled read fails
/// the test instead of hanging it.
const FRAME_TIMEOUT: Duration = Duration::from_secs(2);

fn envelope_request(request_id: u64, body: RequestBody) -> Envelope {
    Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(EnvelopePayload::Request(Request {
            request_id,
            body: Some(body),
        })),
    }
}

fn envelope_notification(event: NotificationEvent) -> Envelope {
    Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(EnvelopePayload::Notification(Notification {
            event: Some(event),
        })),
    }
}

/// Read exactly one frame off the stream and decode it. Fails on EOF before
/// the frame completes, on garbage bytes, and on leftover bytes after the
/// frame — any of those means the framing boundary moved.
async fn read_one_frame(stream: &mut UnixStream) -> Result<Envelope> {
    let mut buf = Vec::new();
    let mut scratch = [0u8; 128];
    loop {
        let n = tokio::time::timeout(FRAME_TIMEOUT, stream.read(&mut scratch))
            .await
            .context("frame read timed out")?
            .context("frame read failed")?;
        if n == 0 {
            anyhow::bail!(
                "EOF with {} buffered bytes and no complete frame",
                buf.len()
            );
        }
        buf.extend_from_slice(&scratch[..n]);
        match mux_protocol::unframe(&buf) {
            Ok((envelope, consumed)) => {
                anyhow::ensure!(
                    consumed == buf.len(),
                    "frame consumed {consumed} of {} buffered bytes",
                    buf.len()
                );
                return Ok(envelope);
            }
            // Partial frame so far; keep reading until it completes or errors.
            Err(_) => continue,
        }
    }
}

// ============================================================
// §9 模拟数据包丢失
// ============================================================

/// §9 数据包丢失恢复:split across socket reads, the reassembled bytes must
/// decode to exactly the envelope that was sent. Asserting only "the first
/// read saw fewer bytes" would pass even if framing broke, so this also
/// refuses a partial decode and verifies the reassembled payload.
#[tokio::test]
async fn test_packet_loss_simulation() -> Result<()> {
    let (mut client, mut server) = unix_pipe().await?;

    let envelope = envelope_request(
        1,
        RequestBody::CreateSession(mux_protocol::CreateSessionRequest {
            name: "test".into(),
            cwd: "/tmp".into(),
        }),
    );
    let frame = mux_protocol::frame(&envelope)?;
    anyhow::ensure!(
        frame.len() >= 4,
        "frame too small to split: {}",
        frame.len()
    );

    // Deliver only the first segment: the second is "lost" for now.
    let split_at = frame.len() / 2;
    client.write_all(&frame[..split_at]).await?;
    client.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(FRAME_TIMEOUT, server.read(&mut buf)).await??;
    assert_eq!(n, split_at, "first read must see only the first segment");
    assert!(
        mux_protocol::unframe(&buf[..n]).is_err(),
        "a half frame must not decode as a complete message"
    );

    // The "lost" segment arrives; the assembled bytes must decode to the
    // original envelope and nothing else.
    client.write_all(&frame[split_at..]).await?;
    client.flush().await?;

    let mut assembled = buf[..n].to_vec();
    loop {
        let m = tokio::time::timeout(FRAME_TIMEOUT, server.read(&mut buf)).await??;
        anyhow::ensure!(m > 0, "EOF before the second segment completed the frame");
        assembled.extend_from_slice(&buf[..m]);
        match mux_protocol::unframe(&assembled) {
            Ok((decoded, consumed)) => {
                assert_eq!(
                    consumed,
                    assembled.len(),
                    "split frame must reassemble to exactly one frame"
                );
                assert_eq!(
                    decoded, envelope,
                    "reassembled frame must match what was sent"
                );
                return Ok(());
            }
            Err(_) => continue,
        }
    }
}

// ============================================================
// §9 模拟网络延迟
// ============================================================

/// §9 延迟注入:after the delay the frame must still decode to the exact
/// notification that was sent, not merely produce some bytes.
#[tokio::test]
async fn test_latency_injection() -> Result<()> {
    let (mut client, mut server) = unix_pipe().await?;

    let envelope = envelope_notification(NotificationEvent::PaneDirty(mux_protocol::PaneDirty {
        pane_id: "p1".into(),
    }));
    let frame = mux_protocol::frame(&envelope)?;

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.write_all(&frame).await.expect("write delayed frame");
        client.flush().await.expect("flush delayed frame");
    });

    let decoded = read_one_frame(&mut server).await?;
    assert_eq!(
        decoded, envelope,
        "delayed frame must decode to the sent notification"
    );
    Ok(())
}

// ============================================================
// §9 模拟网络分区
// ============================================================

/// §9 网络分区:the peer delivers its last frame and then vanishes; the
/// reader must decode that frame and then observe EOF. The previous
/// `assert!(n >= 0)` on a `usize` could never fail, so this decodes the
/// request and pins the EOF.
#[tokio::test]
async fn test_network_partition() -> Result<()> {
    let (client, mut server) = unix_pipe().await?;

    let envelope = envelope_request(
        1,
        RequestBody::ListSessions(mux_protocol::ListSessionsRequest {}),
    );
    let frame = mux_protocol::frame(&envelope)?;

    tokio::spawn(async move {
        let mut c = client;
        c.write_all(&frame).await.expect("write partition frame");
        c.flush().await.expect("flush partition frame");
        drop(c);
    });

    let decoded = read_one_frame(&mut server).await?;
    assert_eq!(
        decoded, envelope,
        "the final frame before a partition must decode"
    );

    let mut tail = [0u8; 16];
    let n = tokio::time::timeout(FRAME_TIMEOUT, server.read(&mut tail)).await??;
    assert_eq!(n, 0, "after the peer drops, the read half must return EOF");
    Ok(())
}

// ============================================================
// §9 重连测试
// ============================================================

/// §9 客户端重连:a new connection after disconnect must carry its own frames
/// cleanly — the first connection delivers its notification then EOFs, and
/// the second connection's request decodes with no residue from the first.
/// The previous version wrote bytes and asserted nothing about what arrived.
#[tokio::test]
async fn test_reconnect_after_disconnect() -> Result<()> {
    let (mut client1, mut server1) = unix_pipe().await?;

    let first = envelope_notification(NotificationEvent::PaneFocused(mux_protocol::PaneFocused {
        pane_id: "p1".into(),
    }));
    client1.write_all(&mux_protocol::frame(&first)?).await?;
    client1.flush().await?;
    drop(client1);

    assert_eq!(
        read_one_frame(&mut server1).await?,
        first,
        "first connection must deliver its frame before EOF"
    );
    let mut tail = [0u8; 16];
    let n = tokio::time::timeout(FRAME_TIMEOUT, server1.read(&mut tail)).await??;
    assert_eq!(n, 0, "first connection must EOF after its frame");

    let (mut client2, mut server2) = unix_pipe().await?;
    let second = envelope_request(2, RequestBody::Detach(mux_protocol::DetachRequest {}));
    client2.write_all(&mux_protocol::frame(&second)?).await?;
    client2.flush().await?;

    assert_eq!(
        read_one_frame(&mut server2).await?,
        second,
        "the reconnected socket must carry its own frame"
    );
    Ok(())
}

// ============================================================
// §9 半关闭
// ============================================================

/// §9 半关闭:shutting down the write half must deliver pending bytes, EOF
/// the peer's read half, and leave the reverse direction open. A reader that
/// treats half-close as a full teardown would drop the server's reply.
#[tokio::test]
async fn test_half_close_read_half_stays_open() -> Result<()> {
    let (mut client, mut server) = unix_pipe().await?;

    let outbound = envelope_request(7, RequestBody::Detach(mux_protocol::DetachRequest {}));
    client.write_all(&mux_protocol::frame(&outbound)?).await?;
    client.flush().await?;
    // tokio UnixStream shutdown = Shutdown::Write: no more client->server bytes.
    client.shutdown().await?;

    assert_eq!(
        read_one_frame(&mut server).await?,
        outbound,
        "the write-half shutdown must not lose buffered bytes"
    );
    let mut eof_probe = [0u8; 8];
    let n = tokio::time::timeout(FRAME_TIMEOUT, server.read(&mut eof_probe)).await??;
    assert_eq!(
        n, 0,
        "write-half shutdown must produce EOF on the peer's read half"
    );

    // The server->client direction is untouched by the client's half-close.
    let reply = envelope_notification(NotificationEvent::PaneFocused(mux_protocol::PaneFocused {
        pane_id: "after-half-close".into(),
    }));
    server.write_all(&mux_protocol::frame(&reply)?).await?;
    server.flush().await?;
    assert_eq!(
        read_one_frame(&mut client).await?,
        reply,
        "the read half must stay open across a write-half shutdown"
    );
    Ok(())
}

// ============================================================
// §9 帧完整性测试
// ============================================================

/// §9 损坏帧处理:underflow, overlong prefix, and corrupted payload must all
/// be decode errors — never a silent empty-envelope success.
#[test]
fn test_corrupted_frame_handling() {
    // varint(10) = 0x0A followed by only 2 of the declared 10 payload bytes.
    let truncated = vec![0x0A, 0x01, 0x02];
    mux_protocol::unframe(&truncated)
        .expect_err("payload shorter than the prefix declares must error");

    // An 11-byte run of continuation bits can never terminate a varint.
    let overlong = vec![0xFF; mux_protocol::MAX_VARINT_LEN + 1];
    mux_protocol::unframe(&overlong).expect_err("overlong varint prefix must error");

    // Valid single-byte prefix, payload flipped to bytes no Envelope encodes
    // to (0xFF runs make the first field tag's varint unterminated).
    let envelope = envelope_request(
        1,
        RequestBody::CreateSession(mux_protocol::CreateSessionRequest {
            name: "test".into(),
            cwd: "/tmp".into(),
        }),
    );
    let mut frame = mux_protocol::frame(&envelope).unwrap();
    assert!(
        frame.len() < 128,
        "test frame must have a single-byte length prefix, got {}",
        frame.len()
    );
    for byte in &mut frame[1..] {
        *byte = 0xFF;
    }
    mux_protocol::unframe(&frame).expect_err("corrupted payload must not decode");
}

/// §9 截断帧处理:a truncated payload and a lone prefix byte must both error —
/// the reader must not guess a length or invent an empty message.
#[test]
fn test_truncated_frame_handling() {
    let full_frame = mux_protocol::frame(&envelope_notification(NotificationEvent::PaneDirty(
        mux_protocol::PaneDirty {
            pane_id: "p1".into(),
        },
    )))
    .unwrap();
    let truncated = &full_frame[..full_frame.len() / 2];
    mux_protocol::unframe(truncated).expect_err("truncated payload must error");

    // Push the envelope past a single-byte prefix, then hand the decoder one
    // prefix byte with no continuation.
    let big = mux_protocol::frame(&envelope_notification(NotificationEvent::PaneDirty(
        mux_protocol::PaneDirty {
            pane_id: "p".repeat(300),
        },
    )))
    .unwrap();
    assert!(
        big.len() > 128,
        "big frame must use a multi-byte length prefix, got {}",
        big.len()
    );
    mux_protocol::unframe(&big[..1]).expect_err("a lone prefix byte must not decode");
}

/// §9 帧消费字节数验证:two back-to-back frames must tile the buffer exactly —
/// the first decode stops at the first boundary and the remainder decodes as
/// the second frame.
#[test]
fn test_frame_consumption_count() {
    let first = envelope_request(
        1,
        RequestBody::CreateSession(mux_protocol::CreateSessionRequest {
            name: "one".into(),
            cwd: "/tmp".into(),
        }),
    );
    let second = envelope_request(
        2,
        RequestBody::ListSessions(mux_protocol::ListSessionsRequest {}),
    );
    let first_frame = mux_protocol::frame(&first).unwrap();
    let second_frame = mux_protocol::frame(&second).unwrap();
    let mut stream = first_frame.clone();
    stream.extend(second_frame);

    let (decoded_first, consumed) = mux_protocol::unframe(&stream).unwrap();
    assert_eq!(decoded_first, first);
    assert_eq!(
        consumed,
        first_frame.len(),
        "consumed must stop exactly at the first frame boundary"
    );

    let (decoded_second, consumed_second) = mux_protocol::unframe(&stream[consumed..]).unwrap();
    assert_eq!(decoded_second, second);
    assert_eq!(
        consumed + consumed_second,
        stream.len(),
        "the two frames must exactly tile the stream"
    );
}

// ============================================================
// §9 辅助函数
// ============================================================

async fn unix_pipe() -> Result<(UnixStream, UnixStream)> {
    let dir = tempfile::tempdir()?;
    let sock_path = dir.path().join("test.sock");

    let listener = tokio::net::UnixListener::bind(&sock_path)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.expect("accept test connection");
        stream
    });

    let client = UnixStream::connect(&sock_path).await?;
    let server = server.await?;
    Ok((client, server))
}
