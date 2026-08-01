//! # Socket byte-stream framing tests
//!
//! 这些测试只覆盖 mux_protocol frame/unframe 在原始 socket 字节流上的行为
//! (部分帧、TCP/Unix socket 字节边界切片)。它们**不**起 mux_server 子进程,
//! 因此不是端到端测试。真正的 e2e 在 crates/mux/tests/e2e.rs。

use anyhow::Result;
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::UnixStream;

// ============================================================
// §9 模拟数据包丢失
// ============================================================

/// §9 数据包丢失模拟：验证部分帧丢失时连接处理
#[tokio::test]
async fn test_packet_loss_simulation() -> Result<()> {
    let (mut client, mut server) = unix_pipe().await?;

    let frame = mux_protocol::frame(&mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Request(
            mux_protocol::Request {
                request_id: 1,
                body: Some(mux_protocol::proto::request::Body::CreateSession(
                    mux_protocol::CreateSessionRequest {
                        name: "test".into(),
                        cwd: "/tmp".into(),
                    },
                )),
            },
        )),
    })?;

    // 模拟数据包丢失：只发送一半字节
    let split_at = frame.len() / 2;
    client.write_all(&frame[..split_at]).await?;
    client.flush().await?;

    let mut buf = vec![0u8; 4096];
    let n = server.read(&mut buf).await?;
    assert!(n < frame.len(), "数据包丢失后读取应少于完整帧");

    Ok(())
}

// ============================================================
// §9 模拟网络延迟
// ============================================================

/// §9 延迟注入：验证请求在延迟后仍能被正确接收
#[tokio::test]
async fn test_latency_injection() -> Result<()> {
    let (mut client, mut server) = unix_pipe().await?;

    let frame = mux_protocol::frame(&mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Notification(
            mux_protocol::Notification {
                event: Some(mux_protocol::proto::notification::Event::PaneDirty(
                    mux_protocol::PaneDirty {
                        pane_id: "p1".into(),
                    },
                )),
            },
        )),
    })?;

    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(50)).await;
        let _ = client.write_all(&frame).await;
        let _ = client.flush().await;
    });

    let mut buf = vec![0u8; 4096];
    let n = tokio::time::timeout(Duration::from_millis(500), server.read(&mut buf))
        .await
        .expect("读取超时")?;

    assert!(n > 0, "延迟后应能收到数据");
    Ok(())
}

// ============================================================
// §9 模拟网络分区
// ============================================================

/// §9 网络分区模拟：验证连接断开后正确处理
#[tokio::test]
async fn test_network_partition() -> Result<()> {
    let (client, mut server) = unix_pipe().await?;

    let frame = mux_protocol::frame(&mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Request(
            mux_protocol::Request {
                request_id: 1,
                body: Some(mux_protocol::proto::request::Body::ListSessions(
                    mux_protocol::ListSessionsRequest {},
                )),
            },
        )),
    })?;

    tokio::spawn(async move {
        let mut c = client;
        let _ = c.write_all(&frame).await;
        let _ = c.flush().await;
        drop(c);
    });

    let mut buf = vec![0u8; 4096];
    let n = server.read(&mut buf).await?;
    assert!(n >= 0);

    let n2 = server.read(&mut buf).await?;
    assert_eq!(n2, 0, "连接断开后应返回 EOF");
    Ok(())
}

// ============================================================
// §9 重连测试
// ============================================================

/// §9 客户端重连模拟：断开后重新连接
#[tokio::test]
async fn test_reconnect_after_disconnect() -> Result<()> {
    let (mut client1, server1) = unix_pipe().await?;

    let frame = mux_protocol::frame(&mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Notification(
            mux_protocol::Notification {
                event: Some(mux_protocol::proto::notification::Event::PaneFocused(
                    mux_protocol::PaneFocused {
                        pane_id: "p1".into(),
                    },
                )),
            },
        )),
    })?;

    client1.write_all(&frame).await?;
    client1.flush().await?;
    drop(client1);
    drop(server1);

    // 模拟重连
    let (mut client2, _server2) = unix_pipe().await?;

    let frame2 = mux_protocol::frame(&mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Request(
            mux_protocol::Request {
                request_id: 2,
                body: Some(mux_protocol::proto::request::Body::Detach(
                    mux_protocol::DetachRequest {},
                )),
            },
        )),
    })?;

    client2.write_all(&frame2).await?;
    client2.flush().await?;
    Ok(())
}

// ============================================================
// §9 帧完整性测试
// ============================================================

/// §9 损坏帧处理:验证 unframe 对长度前缀无效或 payload 不完整的数据报错。
/// 长度前缀 0 + trailing bytes 是合法 protobuf (空消息 + 未消费尾部),
/// 所以用真正不完整的长度前缀来测:声明 len=10 但只给 2 字节 payload。
#[test]
fn test_corrupted_frame_handling() {
    // varint(10) = 0x0A,后跟 2 字节 payload (期望 10) -> underflow
    let truncated = vec![0x0A, 0x01, 0x02];
    let result = mux_protocol::unframe(&truncated);
    assert!(result.is_err(), "声明长度超过实际 payload 应返回解码错误");
}

/// §9 截断帧处理
#[test]
fn test_truncated_frame_handling() {
    let env = mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Notification(
            mux_protocol::Notification {
                event: Some(mux_protocol::proto::notification::Event::PaneDirty(
                    mux_protocol::PaneDirty {
                        pane_id: "p1".into(),
                    },
                )),
            },
        )),
    };
    let full_frame = mux_protocol::frame(&env).unwrap();
    let truncated = &full_frame[..full_frame.len() / 2];
    let result = mux_protocol::unframe(truncated);
    assert!(result.is_err(), "截断帧应返回解码错误");
}

/// §9 帧消费字节数验证
#[test]
fn test_frame_consumption_count() {
    let env = mux_protocol::Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(mux_protocol::proto::envelope::Payload::Request(
            mux_protocol::Request {
                request_id: 1,
                body: Some(mux_protocol::proto::request::Body::CreateSession(
                    mux_protocol::CreateSessionRequest {
                        name: "test".into(),
                        cwd: "/tmp".into(),
                    },
                )),
            },
        )),
    };

    let framed = mux_protocol::frame(&env).unwrap();
    let (decoded, consumed) = mux_protocol::unframe(&framed).unwrap();
    assert_eq!(consumed, framed.len());
    assert!(decoded.version.is_some());
}

// ============================================================
// §9 辅助函数
// ============================================================

async fn unix_pipe() -> Result<(UnixStream, UnixStream)> {
    let dir = tempfile::tempdir()?;
    let sock_path = dir.path().join("test.sock");

    let listener = tokio::net::UnixListener::bind(&sock_path)?;
    let server = tokio::spawn(async move {
        let (stream, _) = listener.accept().await.unwrap();
        stream
    });

    let client = UnixStream::connect(&sock_path).await?;
    let server = server.await?;
    Ok((client, server))
}
