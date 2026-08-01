//! §16.6 transport_resilient 测试套件。

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use anyhow::{Context as _, Result};
use mux_protocol::proto;
use mux_protocol::proto::{Envelope, Notification, PaneDirty, PaneOutputChunk};
use tokio::net::UdpSocket;
use tokio::time::timeout;

use super::UdpResilientTransport;
use super::crypto::{
    Direction, KEY_SIZE, NONCE_COUNTER_LEN, PACKET_OVERHEAD, PacketCodec, PacketWindow,
    REPLAY_WINDOW_SIZE,
};
use super::packet::{
    DatagramHeader, MAX_FRAGMENT_PAYLOAD, MAX_REASSEMBLY_MESSAGES, MTU, Reassembler,
    fragment_count_for, reassembly_timeout,
};
use super::rtt::{
    ACK_INTERVAL, HeartbeatManager, RTO_MIN, RttEstimator, SEND_INTERVAL_MAX, SEND_INTERVAL_MIN,
    SERVER_ASSOCIATION_TIMEOUT, TimestampTracker,
};
use super::transport::UdpSession;

const TEST_TIMEOUT: Duration = Duration::from_secs(5);
const QUIET_TIMEOUT: Duration = Duration::from_millis(300);

fn loopback() -> Result<SocketAddr> {
    Ok("127.0.0.1:0".parse()?)
}

fn fragment_header(message_id: u32, index: u16, count: u16) -> DatagramHeader {
    DatagramHeader {
        message_id,
        fragment_index: index,
        fragment_count: count,
        timestamp: 0,
        timestamp_reply: None,
        keepalive: false,
    }
}

async fn collect_datagrams(socket: &UdpSocket, expected: usize) -> Result<Vec<Vec<u8>>> {
    let mut datagrams = Vec::with_capacity(expected);
    let mut buffer = [0u8; 2048];
    while datagrams.len() < expected {
        let (len, _) = timeout(TEST_TIMEOUT, socket.recv_from(&mut buffer)).await??;
        let datagram = buffer.get(..len).context("datagram longer than buffer")?;
        datagrams.push(datagram.to_vec());
    }
    Ok(datagrams)
}

// ============================================================================
// §16.6 AEAD、nonce 方向切分、重放窗口
// ============================================================================

/// §16.6 AEAD 加密/解密往返测试。
#[test]
fn test_aead_roundtrip() -> Result<()> {
    let key = [1u8; KEY_SIZE];
    let client = PacketCodec::new(key, Direction::ClientToServer);
    let server = PacketCodec::new(key, Direction::ServerToClient);

    let plaintext = b"Hello, UDP resilient transport!";
    let packet = client.encrypt(plaintext)?;
    assert_eq!(packet.len(), plaintext.len() + PACKET_OVERHEAD);
    assert_eq!(server.decrypt(&packet)?, plaintext);

    let reply = b"and hello back";
    let packet = server.encrypt(reply)?;
    assert_eq!(client.decrypt(&packet)?, reply);
    Ok(())
}

/// §16.6 两个方向的 nonce 空间必须互不相交。
///
/// 早先的实现让两端各自从序列号 1 开始, 于是双方发出的第 N 个包 (key, nonce) 完全相同,
/// AES-GCM 的密钥流被复用: 两段密文异或即得两段明文异或, 认证子密钥也可被恢复。
#[test]
fn test_direction_nonce_spaces_are_disjoint() -> Result<()> {
    let key = [7u8; KEY_SIZE];
    let client = PacketCodec::new(key, Direction::ClientToServer);
    let server = PacketCodec::new(key, Direction::ServerToClient);

    let plaintext = b"identical plaintext sent from both endpoints";
    let mut seen_counters = HashSet::new();

    for round in 0..64 {
        let from_client = client.encrypt(plaintext)?;
        let from_server = server.encrypt(plaintext)?;

        let client_counter = u64::from_be_bytes(
            from_client
                .get(..NONCE_COUNTER_LEN)
                .context("missing counter")?
                .try_into()?,
        );
        let server_counter = u64::from_be_bytes(
            from_server
                .get(..NONCE_COUNTER_LEN)
                .context("missing counter")?
                .try_into()?,
        );

        assert_eq!(client_counter >> 63, 0, "client direction bit must be 0");
        assert_eq!(server_counter >> 63, 1, "server direction bit must be 1");
        assert!(
            seen_counters.insert(client_counter),
            "nonce counter reused in round {}",
            round
        );
        assert!(
            seen_counters.insert(server_counter),
            "nonce counter reused in round {}",
            round
        );

        // 若 (key, nonce) 复用, 同一明文在两端会产生逐字节相同的密文。
        assert_ne!(from_client, from_server, "keystream reused in round {}", round);
    }
    Ok(())
}

/// §16.6 反射回发送者自己的包必须被拒绝。
#[test]
fn test_reflected_packet_is_rejected() -> Result<()> {
    let key = [8u8; KEY_SIZE];
    let client = PacketCodec::new(key, Direction::ClientToServer);
    let server = PacketCodec::new(key, Direction::ServerToClient);

    let packet = client.encrypt(b"client payload")?;
    assert!(
        client.decrypt(&packet).is_err(),
        "a codec must reject packets carrying its own direction bit"
    );
    assert_eq!(server.decrypt(&packet)?, b"client payload");
    Ok(())
}

/// §16.6 重放窗口: 拒绝重复的数据包。
#[test]
fn test_replay_is_rejected() -> Result<()> {
    let key = [2u8; KEY_SIZE];
    let client = PacketCodec::new(key, Direction::ClientToServer);
    let server = PacketCodec::new(key, Direction::ServerToClient);

    let packet = client.encrypt(b"test message")?;
    assert_eq!(server.decrypt(&packet)?, b"test message");
    assert!(server.decrypt(&packet).is_err(), "replay must be rejected");
    Ok(())
}

/// §16.6 丢包数量超过窗口后, 连接必须继续可用。
///
/// 早先的实现在 `gap > 64` 时直接拒绝且不推进 high_water, 于是 UDP 上一次丢 65 个包之后
/// 所有后续合法包被永久拒绝, 连接静默死亡。
#[test]
fn test_replay_window_recovers_from_large_gap() -> Result<()> {
    let key = [9u8; KEY_SIZE];
    let client = PacketCodec::new(key, Direction::ClientToServer);
    let server = PacketCodec::new(key, Direction::ServerToClient);

    let first = client.encrypt(b"first")?;
    assert_eq!(server.decrypt(&first)?, b"first");

    // 连续丢掉远超窗口大小的一批包。
    for _ in 0..(REPLAY_WINDOW_SIZE * 4) {
        client.encrypt(b"lost in transit")?;
    }

    let after_gap = client.encrypt(b"after the gap")?;
    assert_eq!(
        server.decrypt(&after_gap)?,
        b"after the gap",
        "the window must jump forward instead of refusing to advance"
    );

    for index in 0..32 {
        let packet = client.encrypt(b"still flowing")?;
        assert_eq!(
            server.decrypt(&packet)?,
            b"still flowing",
            "packet {} rejected after the gap",
            index
        );
    }
    Ok(())
}

/// §16.6 滑动窗口的跳跃、补洞与过旧拒绝。
#[test]
fn test_replay_window_sliding_behaviour() {
    let mut window = PacketWindow::new();

    assert!(!window.mark(0), "sequence 0 is never legitimate");
    assert!(window.mark(1));
    assert!(!window.mark(1), "duplicate must be rejected");

    // 向前跳跃后回填窗口内的空洞。
    assert!(window.mark(5));
    assert_eq!(window.high_water(), 5);
    assert!(window.mark(3));
    assert!(!window.mark(3));
    assert!(window.mark(2));
    assert!(window.mark(4));
    assert!(!window.mark(5));

    // 大跨度跳跃: 旧序列号全部落到窗口之外。
    assert!(window.mark(1000));
    assert_eq!(window.high_water(), 1000);
    assert!(!window.mark(5), "sequence far behind the window must be rejected");

    let oldest_in_window = 1000 - (REPLAY_WINDOW_SIZE as u64 - 1);
    assert!(window.is_acceptable(oldest_in_window));
    assert!(window.mark(oldest_in_window));
    assert!(!window.mark(oldest_in_window - 1), "one slot past the window");

    assert!(window.is_acceptable(1001));
    assert!(!window.is_acceptable(1000), "already accepted");
}

/// §16.6 认证失败的伪造包不得推进接收窗口。
///
/// 窗口如果在认证之前推进, 链路上的任何人只要伪造一个序列号靠前的垃圾包, 就能把窗口顶走,
/// 让之后的合法包全部落在窗口之外。
#[test]
fn test_forged_packet_does_not_advance_window() -> Result<()> {
    let key = [10u8; KEY_SIZE];
    let client = PacketCodec::new(key, Direction::ClientToServer);
    let server = PacketCodec::new(key, Direction::ServerToClient);

    let first = client.encrypt(b"first")?;
    server.decrypt(&first)?;
    assert_eq!(server.recv_high_water(), 1);

    // 方向位正确、序列号靠前, 但密文是垃圾。
    let mut forged = vec![0u8; PACKET_OVERHEAD];
    forged
        .get_mut(..NONCE_COUNTER_LEN)
        .context("forged packet too short")?
        .copy_from_slice(&40u64.to_be_bytes());
    assert!(server.decrypt(&forged).is_err());
    assert_eq!(
        server.recv_high_water(),
        1,
        "an unauthenticated packet must not move the replay window"
    );

    for index in 0..32 {
        let packet = client.encrypt(b"legitimate")?;
        assert_eq!(
            server.decrypt(&packet)?,
            b"legitimate",
            "legitimate packet {} rejected after a forgery attempt",
            index
        );
    }
    Ok(())
}

/// §16.6 过短的数据包必须被拒绝而不是 panic。
#[test]
fn test_short_packet_is_rejected() {
    let codec = PacketCodec::new([3u8; KEY_SIZE], Direction::ServerToClient);
    for len in 0..PACKET_OVERHEAD {
        assert!(codec.decrypt(&vec![0u8; len]).is_err(), "len={}", len);
    }
}

// ============================================================================
// §16.6 数据报头部与分片重组
// ============================================================================

/// §16.6 头部编解码往返。
#[test]
fn test_datagram_header_roundtrip() -> Result<()> {
    let header = DatagramHeader {
        message_id: 0xDEAD_BEEF,
        fragment_index: 3,
        fragment_count: 9,
        timestamp: 0x1234,
        timestamp_reply: Some(0x5678),
        keepalive: true,
    };
    let mut encoded = header.encode().to_vec();
    encoded.extend_from_slice(b"payload bytes");

    let (decoded, payload) = DatagramHeader::decode(&encoded)?;
    assert_eq!(decoded, header);
    assert_eq!(payload, b"payload bytes");

    let without_reply = DatagramHeader {
        timestamp_reply: None,
        keepalive: false,
        ..header
    };
    let encoded = without_reply.encode();
    let (decoded, payload) = DatagramHeader::decode(&encoded)?;
    assert_eq!(decoded, without_reply);
    assert!(payload.is_empty());
    Ok(())
}

/// §16.6 非法头部必须被拒绝。
#[test]
fn test_datagram_header_rejects_invalid_fields() {
    assert!(DatagramHeader::decode(&[0u8; 5]).is_err(), "truncated header");

    let zero_count = fragment_header(1, 0, 0);
    assert!(DatagramHeader::decode(&zero_count.encode()).is_err());

    let index_out_of_range = fragment_header(1, 5, 5);
    assert!(DatagramHeader::decode(&index_out_of_range.encode()).is_err());

    let mut oversized = fragment_header(1, 0, 2).encode().to_vec();
    oversized.extend_from_slice(&vec![0u8; MAX_FRAGMENT_PAYLOAD + 1]);
    assert!(DatagramHeader::decode(&oversized).is_err());
}

/// §16.6 分片数计算与 MTU 的关系。
#[test]
fn test_fragment_count_matches_mtu_budget() -> Result<()> {
    assert_eq!(fragment_count_for(0)?, 1);
    assert_eq!(fragment_count_for(1)?, 1);
    assert_eq!(fragment_count_for(MAX_FRAGMENT_PAYLOAD)?, 1);
    assert_eq!(fragment_count_for(MAX_FRAGMENT_PAYLOAD + 1)?, 2);
    assert_eq!(fragment_count_for(MAX_FRAGMENT_PAYLOAD * 3)?, 3);
    assert!(fragment_count_for(super::MAX_MESSAGE_SIZE + 1).is_err());
    Ok(())
}

/// §16.6 顺序到达的重组。
#[test]
fn test_reassembly_in_order() -> Result<()> {
    let mut reassembler = Reassembler::new();
    let now = Instant::now();
    let window = Duration::from_secs(5);

    assert!(
        reassembler
            .accept(&fragment_header(1, 0, 3), b"aaa", now, window)?
            .is_none()
    );
    assert!(
        reassembler
            .accept(&fragment_header(1, 1, 3), b"bbb", now, window)?
            .is_none()
    );
    let complete = reassembler.accept(&fragment_header(1, 2, 3), b"ccc", now, window)?;
    assert_eq!(complete.as_deref(), Some(&b"aaabbbccc"[..]));
    assert_eq!(reassembler.pending_messages(), 0);
    assert_eq!(reassembler.buffered_bytes(), 0);
    Ok(())
}

/// §16.6 乱序到达的重组。
#[test]
fn test_reassembly_out_of_order() -> Result<()> {
    let mut reassembler = Reassembler::new();
    let now = Instant::now();
    let window = Duration::from_secs(5);

    assert!(
        reassembler
            .accept(&fragment_header(2, 2, 3), b"ccc", now, window)?
            .is_none()
    );
    assert!(
        reassembler
            .accept(&fragment_header(2, 0, 3), b"aaa", now, window)?
            .is_none()
    );
    let complete = reassembler.accept(&fragment_header(2, 1, 3), b"bbb", now, window)?;
    assert_eq!(complete.as_deref(), Some(&b"aaabbbccc"[..]));
    Ok(())
}

/// §16.6 重复分片保留先到的那份, 且不重复计费。
#[test]
fn test_reassembly_ignores_duplicate_fragment() -> Result<()> {
    let mut reassembler = Reassembler::new();
    let now = Instant::now();
    let window = Duration::from_secs(5);

    reassembler.accept(&fragment_header(3, 0, 2), b"aaa", now, window)?;
    let charged = reassembler.buffered_bytes();
    assert!(
        reassembler
            .accept(&fragment_header(3, 0, 2), b"zzz", now, window)?
            .is_none()
    );
    assert_eq!(reassembler.buffered_bytes(), charged);

    let complete = reassembler.accept(&fragment_header(3, 1, 2), b"bbb", now, window)?;
    assert_eq!(complete.as_deref(), Some(&b"aaabbb"[..]));
    Ok(())
}

/// §16.6 分片总数中途变化必须被拒绝。
#[test]
fn test_reassembly_rejects_fragment_count_change() -> Result<()> {
    let mut reassembler = Reassembler::new();
    let now = Instant::now();
    let window = Duration::from_secs(5);

    reassembler.accept(&fragment_header(4, 0, 3), b"aaa", now, window)?;
    assert!(
        reassembler
            .accept(&fragment_header(4, 1, 4), b"bbb", now, window)
            .is_err()
    );
    Ok(())
}

/// §16.6 分片丢失后的清理: 缺片的消息不会被交付, 且超时后释放内存。
#[test]
fn test_reassembly_expires_incomplete_message() -> Result<()> {
    let mut reassembler = Reassembler::new();
    let start = Instant::now();
    let window = Duration::from_secs(5);

    reassembler.accept(&fragment_header(9, 0, 4), b"only the first", start, window)?;
    reassembler.accept(&fragment_header(9, 2, 4), b"and the third", start, window)?;
    assert_eq!(reassembler.pending_messages(), 1);
    assert!(reassembler.buffered_bytes() > 0);

    // 未到期时不清理。
    assert_eq!(
        reassembler.purge_expired(start + Duration::from_secs(4), window),
        0
    );
    assert_eq!(reassembler.pending_messages(), 1);

    // 到期后由下一个数据报顺带清理。
    let later = start + Duration::from_secs(6);
    let complete = reassembler.accept(&fragment_header(10, 0, 1), b"unrelated", later, window)?;
    assert_eq!(complete.as_deref(), Some(&b"unrelated"[..]));
    assert_eq!(reassembler.pending_messages(), 0);
    assert_eq!(reassembler.buffered_bytes(), 0);
    Ok(())
}

/// §16.6 在途消息数达到上限时按先到先淘汰逐出, 防止恶意分片耗尽内存。
#[test]
fn test_reassembly_evicts_oldest_when_full() -> Result<()> {
    let mut reassembler = Reassembler::new();
    let start = Instant::now();
    let window = Duration::from_secs(600);

    for message_id in 0..MAX_REASSEMBLY_MESSAGES as u32 {
        reassembler.accept(
            &fragment_header(message_id, 0, 2),
            b"first half",
            start + Duration::from_millis(message_id as u64),
            window,
        )?;
    }
    assert_eq!(reassembler.pending_messages(), MAX_REASSEMBLY_MESSAGES);
    let budget = reassembler.buffered_bytes();

    // 第 65 条消息把最老的那条挤出去, 表大小保持不变。
    reassembler.accept(
        &fragment_header(9_999, 0, 2),
        b"first half",
        start + Duration::from_secs(1),
        window,
    )?;
    assert_eq!(reassembler.pending_messages(), MAX_REASSEMBLY_MESSAGES);
    assert_eq!(reassembler.buffered_bytes(), budget);

    // 消息 0 已被逐出, 补上它的第二片只会开一条新的未完成消息。
    assert!(
        reassembler
            .accept(
                &fragment_header(0, 1, 2),
                b"second half",
                start + Duration::from_secs(1),
                window,
            )?
            .is_none()
    );
    Ok(())
}

/// §16.6 超大载荷 (2 MiB) 的分片/重组往返, 分片乱序送入。
#[test]
fn test_reassembly_large_payload_out_of_order() -> Result<()> {
    let payload: Vec<u8> = (0..2 * 1024 * 1024)
        .map(|index| (index % 251) as u8)
        .collect();
    let fragments: Vec<&[u8]> = payload.chunks(MAX_FRAGMENT_PAYLOAD).collect();
    let fragment_count = u16::try_from(fragments.len())?;
    assert_eq!(fragment_count, fragment_count_for(payload.len())?);
    assert!(fragment_count > 1000);

    let mut reassembler = Reassembler::new();
    let now = Instant::now();
    let window = Duration::from_secs(60);
    let mut complete = None;

    // 先送奇数片, 再送偶数片。
    for parity in [1usize, 0] {
        for (index, fragment) in fragments.iter().enumerate() {
            if index % 2 != parity {
                continue;
            }
            let header = fragment_header(77, u16::try_from(index)?, fragment_count);
            if let Some(message) = reassembler.accept(&header, fragment, now, window)? {
                complete = Some(message);
            }
        }
    }

    assert_eq!(complete.as_deref(), Some(payload.as_slice()));
    assert_eq!(reassembler.pending_messages(), 0);
    assert_eq!(reassembler.buffered_bytes(), 0);
    Ok(())
}

/// §16.6 重组超时由 RTO 推出, 且被夹在下界与上界之间。
#[test]
fn test_reassembly_timeout_bounds() {
    assert_eq!(
        reassembly_timeout(Duration::from_millis(10)),
        super::REASSEMBLY_TIMEOUT_MIN
    );
    assert_eq!(
        reassembly_timeout(Duration::from_secs(5)),
        super::REASSEMBLY_TIMEOUT_MAX
    );
    assert_eq!(
        reassembly_timeout(Duration::from_millis(500)),
        Duration::from_secs(4)
    );
}

// ============================================================================
// §16.6 RTT、帧率控制与心跳
// ============================================================================

/// §16.6 RTT 估计器: 验证 SRTT 收敛。
#[test]
fn test_rtt_estimator() {
    let mut estimator = RttEstimator::new();
    assert_eq!(estimator.samples(), 0);
    assert!(estimator.rto() >= RTO_MIN);

    estimator.record_rtt(100.0);
    estimator.record_rtt(120.0);
    estimator.record_rtt(110.0);
    assert_eq!(estimator.samples(), 3);

    let srtt = estimator.srtt().as_millis();
    assert!(srtt > 90 && srtt < 130, "SRTT={} out of expected range", srtt);

    let rto = estimator.rto();
    assert!(rto >= RTO_MIN && rto <= super::RTO_MAX, "RTO={:?} out of range", rto);

    // 非法采样不得污染估计器。
    let before = estimator.snapshot();
    estimator.record_rtt(f64::NAN);
    estimator.record_rtt(-1.0);
    assert_eq!(estimator.snapshot(), before);
}

/// §16.6 帧率控制: 发送间隔随 RTT 自适应但受上下界约束。
#[test]
fn test_send_interval() {
    let mut estimator = RttEstimator::new();
    estimator.record_rtt(40.0);
    assert!(estimator.send_interval() >= SEND_INTERVAL_MIN);

    estimator.record_rtt(600.0);
    let interval = estimator.send_interval();
    assert!(interval <= SEND_INTERVAL_MAX);
    assert!(interval >= SEND_INTERVAL_MIN);
}

/// §16.6 心跳与关联超时使用各自独立的计时。
#[test]
fn test_heartbeat_manager_timing() {
    let start = Instant::now();
    let mut manager = HeartbeatManager::new_at(start);

    assert!(!manager.needs_heartbeat_at(start));
    assert!(!manager.needs_heartbeat_at(start + ACK_INTERVAL - Duration::from_millis(1)));
    assert!(manager.needs_heartbeat_at(start + ACK_INTERVAL));

    assert!(!manager.association_expired_at(start + SERVER_ASSOCIATION_TIMEOUT - Duration::from_secs(1)));
    assert!(manager.association_expired_at(start + SERVER_ASSOCIATION_TIMEOUT));

    // 只发不收无法让一个早已消失的对端"保持在线"。
    manager.on_send_at(start + Duration::from_secs(30));
    assert!(!manager.needs_heartbeat_at(start + Duration::from_secs(31)));
    assert!(manager.association_expired_at(start + SERVER_ASSOCIATION_TIMEOUT));

    manager.on_receive_at(start + Duration::from_secs(41));
    assert!(!manager.association_expired_at(start + Duration::from_secs(42)));
    assert_eq!(
        manager.since_last_receive_at(start + Duration::from_secs(42)),
        Duration::from_secs(1)
    );
}

/// §16.6 mosh 风格时间戳: 回显、持有时长补偿与采样。
#[test]
fn test_timestamp_tracker() {
    let epoch = Instant::now();
    let mut tracker = TimestampTracker::new_at(epoch);

    assert_eq!(tracker.now16_at(epoch), 0);
    assert_eq!(tracker.now16_at(epoch + Duration::from_millis(1234)), 1234);
    assert!(tracker.take_reply_at(epoch).is_none(), "nothing to echo yet");

    tracker.record_peer_timestamp_at(1000, epoch + Duration::from_millis(500));
    // 回显值加上本端持有的 20ms, 把本端处理延迟从对端测得的 RTT 里扣掉。
    assert_eq!(
        tracker.take_reply_at(epoch + Duration::from_millis(520)),
        Some(1020)
    );
    assert!(
        tracker.take_reply_at(epoch + Duration::from_millis(521)).is_none(),
        "each peer timestamp is echoed at most once"
    );

    // 持有太久的时间戳不再回显。
    tracker.record_peer_timestamp_at(2000, epoch);
    assert!(tracker.take_reply_at(epoch + Duration::from_millis(2000)).is_none());

    assert_eq!(
        tracker.rtt_sample_at(1000, epoch + Duration::from_millis(1040)),
        Some(40.0)
    );
    assert!(
        tracker
            .rtt_sample_at(0, epoch + Duration::from_millis(60_000))
            .is_none(),
        "implausible samples are discarded instead of poisoning SRTT"
    );
}

// ============================================================================
// §16.6 会话层端到端行为
// ============================================================================

/// §16.6 未分片消息的端到端往返, 并验证服务端漫游到客户端地址。
#[tokio::test]
async fn test_session_round_trip() -> Result<()> {
    let key = [11u8; KEY_SIZE];
    let server = UdpSession::bind(loopback()?, key).await?;
    let client = UdpSession::connect(server.local_addr()?, key).await?;

    client.send_message(b"hello from client").await?;
    let received = timeout(TEST_TIMEOUT, server.recv_message()).await??;
    assert_eq!(received, b"hello from client");
    assert_eq!(
        server.peer_addr().map(|addr| addr.port()),
        Some(client.local_addr()?.port())
    );

    server.send_message(b"hello back").await?;
    let received = timeout(TEST_TIMEOUT, client.recv_message()).await??;
    assert_eq!(received, b"hello back");

    // 第二条消息不得被重放窗口误拒。
    client.send_message(b"and again").await?;
    let received = timeout(TEST_TIMEOUT, server.recv_message()).await??;
    assert_eq!(received, b"and again");
    Ok(())
}

/// §16.6 超过 MTU 的消息端到端往返。
#[tokio::test]
async fn test_session_round_trip_fragmented() -> Result<()> {
    let key = [12u8; KEY_SIZE];
    let server = UdpSession::bind(loopback()?, key).await?;
    let client = UdpSession::connect(server.local_addr()?, key).await?;

    let payload: Vec<u8> = (0..16 * 1024).map(|index| (index % 251) as u8).collect();
    assert!(payload.len() > MTU);

    client.send_message(&payload).await?;
    let received = timeout(TEST_TIMEOUT, server.recv_message()).await??;
    assert_eq!(received, payload);
    assert_eq!(server.pending_reassemblies(), 0);
    Ok(())
}

/// §16.6 线上分片格式: 数据报个数、MTU 上限与分片头字段。
#[tokio::test]
async fn test_fragmentation_wire_format() -> Result<()> {
    let key = [13u8; KEY_SIZE];
    let probe = UdpSocket::bind(loopback()?).await?;
    let client = UdpSession::connect(probe.local_addr()?, key).await?;

    let payload = vec![0x5Au8; MAX_FRAGMENT_PAYLOAD * 3 + 7];
    client.send_message(&payload).await?;

    let datagrams = collect_datagrams(&probe, 4).await?;
    let codec = PacketCodec::new(key, Direction::ServerToClient);
    let mut message_id = None;
    let mut reassembled = Vec::new();

    for (expected_index, datagram) in datagrams.iter().enumerate() {
        assert!(
            datagram.len() <= MTU,
            "datagram of {} bytes exceeds MTU",
            datagram.len()
        );
        let plaintext = codec.decrypt(datagram)?;
        let (header, fragment) = DatagramHeader::decode(&plaintext)?;
        assert_eq!(header.fragment_count, 4);
        assert_eq!(header.fragment_index as usize, expected_index);
        assert!(!header.keepalive);
        match message_id {
            None => message_id = Some(header.message_id),
            Some(id) => assert_eq!(header.message_id, id, "fragments must share a message id"),
        }
        reassembled.extend_from_slice(fragment);
    }
    assert_eq!(reassembled, payload);
    Ok(())
}

/// §16.6 分片乱序到达仍能重组。
#[tokio::test]
async fn test_fragments_arriving_out_of_order() -> Result<()> {
    let key = [14u8; KEY_SIZE];
    let server = UdpSession::bind(loopback()?, key).await?;
    let relay = UdpSocket::bind(loopback()?).await?;
    let client = UdpSession::connect(relay.local_addr()?, key).await?;

    let payload = vec![0xC3u8; MAX_FRAGMENT_PAYLOAD * 4 + 11];
    client.send_message(&payload).await?;

    let mut datagrams = collect_datagrams(&relay, 5).await?;
    datagrams.reverse();
    for datagram in &datagrams {
        relay.send_to(datagram, server.local_addr()?).await?;
    }

    let received = timeout(TEST_TIMEOUT, server.recv_message()).await??;
    assert_eq!(received, payload);
    Ok(())
}

/// §16.6 丢片的消息永不交付, 并在重组超时后释放。
#[tokio::test]
async fn test_lost_fragment_is_never_delivered() -> Result<()> {
    let key = [15u8; KEY_SIZE];
    let server = UdpSession::bind(loopback()?, key).await?;
    let relay = UdpSocket::bind(loopback()?).await?;
    let client = UdpSession::connect(relay.local_addr()?, key).await?;

    let payload = vec![0x42u8; MAX_FRAGMENT_PAYLOAD * 3 + 5];
    client.send_message(&payload).await?;

    // 丢掉第二个分片。
    let datagrams = collect_datagrams(&relay, 4).await?;
    for (index, datagram) in datagrams.iter().enumerate() {
        if index == 1 {
            continue;
        }
        relay.send_to(datagram, server.local_addr()?).await?;
    }

    assert!(
        timeout(QUIET_TIMEOUT, server.recv_message()).await.is_err(),
        "a message missing a fragment must never be delivered"
    );
    assert_eq!(server.pending_reassemblies(), 1);

    // 超时之后由下一条消息顺带清理。
    let rto = server.rtt().rto;
    tokio::time::sleep(reassembly_timeout(rto) + Duration::from_millis(200)).await;
    client.send_message(b"a fresh message").await?;
    let datagrams = collect_datagrams(&relay, 1).await?;
    for datagram in &datagrams {
        relay.send_to(datagram, server.local_addr()?).await?;
    }
    let received = timeout(TEST_TIMEOUT, server.recv_message()).await??;
    assert_eq!(received, b"a fresh message");
    assert_eq!(server.pending_reassemblies(), 0);
    Ok(())
}

/// §16.6 服务端在对端地址未知时必须拒绝发送。
///
/// 早先的实现把 `client_addr` 初始化成服务端自己的绑定地址, 于是首次 send 会把加密包发给自己。
#[tokio::test]
async fn test_server_refuses_to_send_before_peer_is_known() -> Result<()> {
    let key = [16u8; KEY_SIZE];
    let server = UdpSession::bind(loopback()?, key).await?;

    assert_eq!(server.peer_addr(), None);
    assert!(server.send_message(b"nobody to talk to").await.is_err());
    assert!(server.send_heartbeat().await.is_err());
    assert!(!server.send_heartbeat_if_needed().await?);

    // 自己没有收到任何东西。
    assert!(timeout(QUIET_TIMEOUT, server.recv_message()).await.is_err());
    Ok(())
}

/// §16.6 漫游只在数据报通过认证之后发生。
#[tokio::test]
async fn test_roaming_requires_authentication() -> Result<()> {
    let key = [17u8; KEY_SIZE];
    let server = UdpSession::bind(loopback()?, key).await?;
    let client = UdpSession::connect(server.local_addr()?, key).await?;

    client.send_message(b"establish the association").await?;
    let received = timeout(TEST_TIMEOUT, server.recv_message()).await??;
    assert_eq!(received, b"establish the association");
    let client_port = client.local_addr()?.port();
    assert_eq!(server.peer_addr().map(|addr| addr.port()), Some(client_port));

    // 攻击者往同一个端口灌垃圾。
    let attacker = UdpSocket::bind(loopback()?).await?;
    attacker
        .send_to(&[0xFFu8; 64], server.local_addr()?)
        .await?;

    assert!(
        timeout(QUIET_TIMEOUT, server.recv_message()).await.is_err(),
        "unauthenticated datagrams must not be delivered"
    );
    assert_eq!(
        server.peer_addr().map(|addr| addr.port()),
        Some(client_port),
        "an unauthenticated datagram must not hijack the association"
    );

    // 关联仍然可用。
    server.send_message(b"still yours").await?;
    let received = timeout(TEST_TIMEOUT, client.recv_message()).await??;
    assert_eq!(received, b"still yours");
    Ok(())
}

/// §16.6 心跳包不会被交付给上层, 但会刷新对端的接收计时。
#[tokio::test]
async fn test_heartbeat_is_not_delivered_to_the_application() -> Result<()> {
    let key = [18u8; KEY_SIZE];
    let server = UdpSession::bind(loopback()?, key).await?;
    let client = UdpSession::connect(server.local_addr()?, key).await?;

    client.send_message(b"init").await?;
    let received = timeout(TEST_TIMEOUT, server.recv_message()).await??;
    assert_eq!(received, b"init");

    server.send_heartbeat().await?;
    assert!(
        timeout(QUIET_TIMEOUT, client.recv_message()).await.is_err(),
        "a keepalive must not surface as an application message"
    );

    server.send_message(b"real payload").await?;
    let received = timeout(TEST_TIMEOUT, client.recv_message()).await??;
    assert_eq!(received, b"real payload");

    assert!(!client.association_expired());
    assert!(!client.needs_heartbeat());
    assert!(!client.send_heartbeat_if_needed().await?);
    Ok(())
}

/// §16.6 RTT 测量确实接在收发路径上。
#[tokio::test]
async fn test_rtt_is_measured_on_the_data_path() -> Result<()> {
    let key = [19u8; KEY_SIZE];
    let server = UdpSession::bind(loopback()?, key).await?;
    let client = UdpSession::connect(server.local_addr()?, key).await?;

    assert_eq!(client.rtt().samples, 0);
    assert_eq!(server.rtt().samples, 0);

    client.send_message(b"ping").await?;
    timeout(TEST_TIMEOUT, server.recv_message()).await??;
    // 服务端此时还没收到过自己的回显, 因此仍无采样。
    assert_eq!(server.rtt().samples, 0);

    // 服务端回包时带上回显, 客户端由此得到第一个采样。
    server.send_message(b"pong").await?;
    timeout(TEST_TIMEOUT, client.recv_message()).await??;
    assert!(client.rtt().samples >= 1, "client never recorded an RTT sample");

    // 反向同理。
    client.send_message(b"ping again").await?;
    timeout(TEST_TIMEOUT, server.recv_message()).await??;
    assert!(server.rtt().samples >= 1, "server never recorded an RTT sample");

    let snapshot = client.rtt();
    assert!(snapshot.rto >= RTO_MIN && snapshot.rto <= super::RTO_MAX);
    assert!(snapshot.send_interval >= SEND_INTERVAL_MIN);
    assert!(snapshot.send_interval <= SEND_INTERVAL_MAX);
    Ok(())
}

// ============================================================================
// §16.6 Envelope 层
// ============================================================================

fn pane_dirty_envelope(pane_id: &str) -> Envelope {
    Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(proto::envelope::Payload::Notification(Notification {
            event: Some(proto::notification::Event::PaneDirty(PaneDirty {
                pane_id: pane_id.into(),
            })),
        })),
    }
}

fn pane_output_envelope(data: Vec<u8>) -> Envelope {
    Envelope {
        version: Some(mux_protocol::PROTOCOL_VERSION),
        payload: Some(proto::envelope::Payload::Notification(Notification {
            event: Some(proto::notification::Event::PaneOutput(PaneOutputChunk {
                pane_id: "w1:p1".into(),
                data,
            })),
        })),
    }
}

/// §16.6 Envelope 端到端往返 (未分片与分片两种)。
#[tokio::test]
async fn test_envelope_round_trip() -> Result<()> {
    let key = [20u8; KEY_SIZE];
    let server = UdpResilientTransport::bind(loopback()?, key).await?;
    let client = UdpResilientTransport::connect(server.local_addr()?, key).await?;

    let small = pane_dirty_envelope("w1:p1");
    client.send(&small).await?;
    let received = timeout(TEST_TIMEOUT, server.recv()).await??;
    assert_eq!(received, small);

    let large = pane_output_envelope(vec![0x7Fu8; 16 * 1024]);
    client.send(&large).await?;
    let received = timeout(TEST_TIMEOUT, server.recv()).await??;
    assert_eq!(received, large);

    // 服务端此时已经知道对端地址, 可以回包。
    let reply = pane_dirty_envelope("w1:p2");
    server.send(&reply).await?;
    let received = timeout(TEST_TIMEOUT, client.recv()).await??;
    assert_eq!(received, reply);
    Ok(())
}

/// §16.6 加密性能: 验证加密/解密速度合理。
#[test]
fn test_encrypt_decrypt_perf() -> Result<()> {
    let key = [5u8; KEY_SIZE];
    let sender = PacketCodec::new(key, Direction::ClientToServer);
    let receiver = PacketCodec::new(key, Direction::ServerToClient);

    let plaintext = vec![0u8; 1000];
    let start = Instant::now();
    for _ in 0..100 {
        let packet = sender.encrypt(&plaintext)?;
        assert_eq!(receiver.decrypt(&packet)?.len(), plaintext.len());
    }
    let elapsed = start.elapsed();
    assert!(
        elapsed < Duration::from_secs(1),
        "encryption too slow: {:?}",
        elapsed
    );
    Ok(())
}
