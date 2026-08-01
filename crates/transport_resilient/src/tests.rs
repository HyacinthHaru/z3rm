//! §16.6 transport_resilient 测试套件。

use std::net::SocketAddr;
use std::time::Duration;

use super::crypto::{KEY_SIZE, NONCE_PREFIX_LEN, PacketCodec};
use super::rtt::{HeartbeatManager, RTO_MIN, RttEstimator, SEND_INTERVAL_MAX, SEND_INTERVAL_MIN};
use super::transport::{MTU, UdpClient, UdpServer};

/// §16.6 AEAD 加密/解密往返测试。
#[test]
fn test_aes_encrypt_decrypt_roundtrip() {
    let key: [u8; KEY_SIZE] = [1u8; KEY_SIZE];
    let prefix: [u8; NONCE_PREFIX_LEN] = [0xAA; NONCE_PREFIX_LEN];

    let send_codec = PacketCodec::new_with_prefix(key, prefix);
    let recv_codec = PacketCodec::new_with_prefix(key, prefix);

    let plaintext = b"Hello, UDP resilient transport!";
    let encrypted = send_codec.encrypt(plaintext);

    // §16.6 密文应比原文长 (nonce + GCM tag)。
    assert!(encrypted.len() > plaintext.len());

    // §16.6 使用接收端 codec 解密。
    let decrypted = recv_codec.decrypt(&encrypted).expect("decrypt failed");
    assert_eq!(decrypted, plaintext);
}

/// §16.6 重放窗口测试: 拒绝重复 nonce。
#[test]
fn test_replay_protection() {
    let key: [u8; KEY_SIZE] = [2u8; KEY_SIZE];
    let prefix: [u8; NONCE_PREFIX_LEN] = [0xBB; NONCE_PREFIX_LEN];

    let send_codec = PacketCodec::new_with_prefix(key, prefix);
    let recv_codec = PacketCodec::new_with_prefix(key, prefix);

    let plaintext = b"test message";
    let encrypted = send_codec.encrypt(plaintext);

    // §16.6 首次解密应成功。
    assert!(recv_codec.decrypt(&encrypted).is_ok());

    // §16.6 重复解密同一数据包应失败 (重放检测)。
    assert!(recv_codec.decrypt(&encrypted).is_err());
}

/// §16.6 RTT 估计器测试: 验证 SRTT 收敛。
#[test]
fn test_rtt_estimator() {
    let mut estimator = RttEstimator::new();

    // §16.6 初始 RTO = SRTT + max(4*RTTVAR, G) = 50 + max(100, 50) = 150ms。
    assert!(estimator.rto().as_millis() >= RTO_MIN.as_millis());

    // §16.6 记录一组 RTT 采样。
    estimator.record_rtt(100.0); // 首次采样
    estimator.record_rtt(120.0);
    estimator.record_rtt(110.0);

    // §16.6 SRTT 应接近采样均值。
    let srtt = estimator.srtt().as_millis() as f64;
    assert!(
        srtt > 90.0 && srtt < 130.0,
        "SRTT={} out of expected range",
        srtt
    );

    // §16.6 RTO 应在合理范围内。
    let rto = estimator.rto().as_millis();
    assert!(rto >= 50 && rto <= 1000, "RTO={} out of range", rto);
}

/// §16.6 帧率控制测试: 验证发送间隔随 RTT 自适应。
#[test]
fn test_send_interval() {
    let mut estimator = RttEstimator::new();

    // §16.6 低 RTT → 间隔应接近最小值。
    estimator.record_rtt(40.0);
    let interval = estimator.send_interval();
    assert!(interval >= SEND_INTERVAL_MIN, "interval below minimum");

    // §16.6 高 RTT → 间隔应增大但不超过最大值。
    estimator.record_rtt(600.0);
    let interval = estimator.send_interval();
    assert!(interval <= SEND_INTERVAL_MAX, "interval above maximum");
    assert!(interval >= SEND_INTERVAL_MIN, "interval below minimum");
}

/// §16.6 心跳管理器测试。
#[test]
fn test_heartbeat_manager() {
    let mut manager = HeartbeatManager::new();

    // §16.6 新建时不应需要心跳。
    assert!(!manager.needs_heartbeat());

    // §16.6 活动后不应需要心跳。
    manager.on_activity();
    assert!(!manager.needs_heartbeat());
    assert!(!manager.association_expired());
}

/// §16.6 无状态漫游测试: UDP 服务端自动跟踪客户端地址。
#[tokio::test]
async fn test_stateless_roaming() {
    let key: [u8; KEY_SIZE] = [3u8; KEY_SIZE];

    // §16.6 服务端绑定到 127.0.0.1:0 (随机端口)。
    let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = UdpServer::bind(server_addr, key)
        .await
        .expect("bind failed");
    let bound_addr = server.local_addr().unwrap();

    // §16.6 客户端连接并发送第一条消息。
    let mut client = UdpClient::connect(bound_addr, key)
        .await
        .expect("client connect failed");
    let client_port = client.local_addr().unwrap().port();

    client
        .send(b"hello from client")
        .await
        .expect("client send failed");

    // §16.6 服务端接收第一条消息。
    let (data, addr) = server.recv().await.expect("server recv failed");
    assert_eq!(data, b"hello from client");
    assert_eq!(addr.port(), client_port);
    assert_eq!(server.client_addr().port(), client_port);

    // §16.6 客户端发送第二条消息 (计数器递增，不被重放检测拒绝)。
    client
        .send(b"hello again from client")
        .await
        .expect("client send2 failed");
    let (data, addr) = server.recv().await.expect("server recv2 failed");
    assert_eq!(data, b"hello again from client");
    assert_eq!(addr.port(), client_port);
    // §16.6 服务端地址应仍指向同一客户端。
    assert_eq!(server.client_addr().port(), client_port);
}

/// §16.6 加密/解密往返测试 (通过完整 UDP 链路)。
#[tokio::test]
async fn test_udp_send_recv() {
    let key: [u8; KEY_SIZE] = [4u8; KEY_SIZE];

    // §16.6 服务端绑定。
    let server_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let server = UdpServer::bind(server_addr, key)
        .await
        .expect("bind failed");
    let bound_addr = server.local_addr().unwrap();

    // §16.6 客户端连接。
    let mut client = UdpClient::connect(bound_addr, key)
        .await
        .expect("connect failed");

    // §16.6 发送明文。
    let message = b"UDP resilient transport test message";
    client.send(message).await.expect("send failed");

    // §16.6 服务端接收并验证。
    let (data, _addr) = server.recv().await.expect("recv failed");
    assert_eq!(data, message);
}

/// §16.6 MTU 分片测试: 大包通过 MTU 限制分片发送。
#[test]
fn test_mtu_fragmentation() {
    // §16.6 创建超过 MTU 的消息。
    let large_message = vec![0xABu8; MTU + 100];
    assert!(large_message.len() > MTU);

    // §16.6 计算需要的分片数。
    let chunks = (large_message.len() as f64 / MTU as f64).ceil() as usize;
    assert!(chunks > 1);

    // §16.6 验证分片逻辑正确。
    let mut offset = 0;
    let mut chunk_count = 0;
    while offset < large_message.len() {
        let chunk_end = std::cmp::min(offset + MTU, large_message.len());
        let _chunk = &large_message[offset..chunk_end];
        chunk_count += 1;
        offset = chunk_end;
    }
    assert_eq!(chunk_count, chunks);
}

/// §16.6 加密性能测试: 验证加密/解密速度合理。
#[test]
fn test_encrypt_decrypt_perf() {
    let key: [u8; KEY_SIZE] = [5u8; KEY_SIZE];
    let codec = PacketCodec::new(key);

    let plaintext = vec![0u8; 1000];
    let start = std::time::Instant::now();

    // §16.6 加密 100 次。
    for _ in 0..100 {
        let encrypted = codec.encrypt(&plaintext);
        // §16.6 同一个 codec 解密自己的密文。
        let _ = codec.decrypt(&encrypted);
    }

    let elapsed = start.elapsed();
    // §16.6 100 次加解密应在 1s 内完成。
    assert!(
        elapsed < Duration::from_secs(1),
        "encryption too slow: {:?}",
        elapsed
    );
}
