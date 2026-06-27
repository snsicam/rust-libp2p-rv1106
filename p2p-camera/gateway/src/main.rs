//! P2P Camera Gateway — 运行在 RV1106 上的媒体网关
//!
//! 职责:
//! 1. 连接 Relay Server 并在其上预约 (Circuit Relay v2 Reservation)
//! 2. 通过 DCUtR 与 Viewer 协商直连
//! 3. 接受 Viewer 的视频/音频 stream 请求
//! 4. 从媒体源 (SDK/文件) 读取帧并通过 stream 发送
//!
//! 自动重连: Relay 断开时自动重新连接 + 重新预约，媒体源不受影响。
//!
//! 固定身份: 首次运行自动生成 Ed25519 密钥并保存到 key_file，
//!           后续启动从文件读取，保证 PeerId 不变。
//!
//! 用法:
//!   cargo run -- \
//!     --relay /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER> \
//!     --mode listen \
//!     --video test.h265       # 可选: 视频文件 (代替 SDK 回调)

mod behaviour;
mod media_source;
#[cfg(feature = "rv1106")]
mod rk_video_source;

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use behaviour::Behaviour;
use bytes::Bytes;
use clap::Parser;
use crossbeam_channel::Sender;
use futures::{AsyncWriteExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, identify, identity, noise, relay,
    swarm::SwarmEvent,
    tcp,
    PeerId,
};
use proto::{
    media_packet::MediaPacket,
    stream_protocols,
};
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

// broadcast channel 容量: 缓冲约 2 秒的视频帧 (25fps * 2)
const BROADCAST_CAPACITY: usize = 100;

// 重连间隔
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let opt = Opt::parse();

    validate_gateway_config(&opt);

    // ---- 初始化媒体源 (文件 or RV1106 SDK) ----
    // 媒体源独立于 P2P 连接，在重连期间持续运行
    let (video_tx, _video_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);
    let (audio_tx, _audio_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);

    // 参数集缓存 (VPS/SPS/PPS) — 新 viewer 连接时先发送这些，避免 "PPS id out of range"
    let param_sets: Option<std::sync::Arc<std::sync::Mutex<Option<Vec<Vec<u8>>>>>>;

    #[cfg(feature = "rv1106")]
    {
        // RV1106 真实摄像头
        let width = opt.width.unwrap_or(1920);
        let height = opt.height.unwrap_or(1080);
        let fps = opt.fps.unwrap_or(25);
        let bitrate = opt.bitrate.unwrap_or(4096);
        println!("[Gateway] Video source: RV1106 camera {}x{} @{}fps {}kbps", width, height, fps, bitrate);
        let source = rk_video_source::RkVideoSource::new(width, height, fps, bitrate);
        param_sets = Some(source.param_sets_handle());
        let (_, _start_tx) = source.spawn(broadcast_sender_to_crossbeam(video_tx.clone()));
        // RV1106 模式下视频源自动开始，不需要 start trigger
    }

    #[cfg(not(feature = "rv1106"))]
    {
        if let Some(video_path) = &opt.video_file {
            let data = std::fs::read(video_path)
                .context("Failed to read video file")?;
            println!("[Gateway] Video file: {:?} ({} bytes)", video_path, data.len());
            let source = media_source::FileVideoSource::from_file(data);
            param_sets = Some(source.param_sets_handle());
            // 文件源在第一个 viewer 连接时启动 (循环播放模式)
            let (_stop_tx, _start_tx) = source.spawn(broadcast_sender_to_crossbeam(video_tx.clone()));
            // 立即开始播放 (不再等第一个 viewer)
            let _ = _start_tx.send(());
            println!("[Gateway] Video source: file ({:?}) — started", video_path);
        } else {
            println!("[Gateway] Video source: NONE (waiting for stream requests)");
            param_sets = None;
        }
    }

    // 音频源
    #[cfg(feature = "rv1106")]
    {
        if opt.enable_audio {
            let source = rk_video_source::RkAudioSource::new(16000);
            source.spawn(broadcast_sender_to_crossbeam(audio_tx.clone()));
            println!("[Gateway] Audio source: RV1106 AI (16kHz mono)");
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        if opt.enable_audio {
            let source = media_source::SilenceAudioSource::new(16000, 1);
            source.spawn(broadcast_sender_to_crossbeam(audio_tx.clone()));
            println!("[Gateway] Audio source: silence (16kHz mono)");
        }
    }

    // ---- 加载/生成固定身份密钥 (保证 PeerId 不变) ----
    let keypair = load_or_create_keypair(&opt.key_file)?;
    let peer_id = keypair.public().to_peer_id();
    println!("[Gateway] PeerId: {peer_id}");

    // ---- 重连循环: Relay 断开时自动重连 ----
    let relay_addr: Multiaddr = opt.relay.parse()
        .context("Invalid relay address")?;

    let mut reconnect_attempt = 0u64;

    loop {
        reconnect_attempt += 1;
        if reconnect_attempt > 1 {
            tracing::warn!("[Gateway] Reconnecting to relay (attempt {reconnect_attempt})...");
            tokio::time::sleep(RECONNECT_DELAY).await;
        }

        match run_gateway_session(
            keypair.clone(),
            relay_addr.clone(),
            video_tx.clone(),
            audio_tx.clone(),
            param_sets.clone(),
            opt.udp_port,
            opt.external_ip.clone(), // 克隆以避免移动
        ).await {
            Ok(()) => break, // 正常退出
            Err(e) => {
                tracing::error!("[Gateway] Session ended: {e}");
                // 继续循环 → 自动重连
            }
        }
    }

    Ok(())
}

/// 从文件加载密钥，不存在则生成新密钥并保存
fn load_or_create_keypair(key_file: &PathBuf) -> Result<identity::Keypair> {
    if key_file.exists() {
        let data = std::fs::read(key_file)
            .with_context(|| format!("Failed to read key file: {}", key_file.display()))?;
        let keypair = identity::Keypair::from_protobuf_encoding(&data)
            .with_context(|| format!("Failed to decode key file: {}", key_file.display()))?;
        println!("[Gateway] Loaded identity from {}", key_file.display());
        Ok(keypair)
    } else {
        let keypair = identity::Keypair::generate_ed25519();
        let data = keypair.to_protobuf_encoding()
            .context("Failed to encode keypair")?;
        if let Some(parent) = key_file.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("Failed to create dir: {}", parent.display()))?;
        }
        std::fs::write(key_file, &data)
            .with_context(|| format!("Failed to write key file: {}", key_file.display()))?;
        println!("[Gateway] Generated new identity → {}", key_file.display());
        Ok(keypair)
    }
}

/// 一次完整的 Gateway 会话: 连接 Relay → 预约 → 接受 stream 请求
///
/// 返回 Err 表示需要重连
async fn run_gateway_session(
    keypair: identity::Keypair,
    relay_addr: Multiaddr,
    video_tx: broadcast::Sender<MediaPacket>,
    audio_tx: broadcast::Sender<MediaPacket>,
    _param_sets: Option<std::sync::Arc<std::sync::Mutex<Option<Vec<Vec<u8>>>>>>, // 添加下划线前缀消除警告
    udp_port: Option<u16>,
    external_ip: Option<String>,
) -> Result<()> {
    let peer_id = keypair.public().to_peer_id();

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default(),
            noise::Config::new,
            libp2p::yamux::Config::default,
        )?
        .with_quic()
        .with_relay_client(noise::Config::new, libp2p::yamux::Config::default)?
        .with_behaviour(|key, relay_client| {
            let identify_config = identify::Config::new(
                "/p2p-camera-gateway/1.0.0".to_string(),
                key.public().clone(),
            )
            .with_push_listen_addr_updates(true);
            Ok(Behaviour::new_with_identify_config(
                key.public().clone(),
                relay_client,
                identify_config,
            ))
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();

    println!("[Gateway] PeerId: {peer_id}");
    tracing::info!("[Gateway] push_listen_addr_updates enabled for DCUtR");

    let mut connection_times: HashMap<PeerId, Instant> = HashMap::new();

    // ---- 监听本地 QUIC (固定端口，若指定) ----
    let udp_port = udp_port.unwrap_or(0);
    let udp_addr = format!("/ip4/0.0.0.0/udp/{}/quic-v1", udp_port).parse()
        .context("Invalid local QUIC listen addr")?;
    swarm.listen_on(udp_addr)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()
        .context("Invalid local TCP listen addr")?)?;
    println!("[Gateway] Listening on QUIC (port {}) and TCP",
        if udp_port != 0 { udp_port.to_string() } else { "random".to_string() });

    // ---- 手动添加外部地址（若指定） ----
    if let Some(ip) = external_ip {
        if udp_port == 0 {
            anyhow::bail!("--external-ip requires --udp-port to be fixed (to know the external UDP port)");
        }
        let ext_addr: Multiaddr = format!("/ip4/{}/udp/{}/quic-v1", ip, udp_port).parse()
            .context("Invalid external address")?;
        swarm.add_external_address(ext_addr);
        println!("[Gateway] Added external address: /ip4/{}/udp/{}/quic-v1", ip, udp_port);
    }

    // ---- 连接 Relay Server ----
    swarm.dial(relay_addr.clone())?;
    println!("[Gateway] Dialing relay: {relay_addr}");

    // 等待与 Relay 的连接建立
    wait_for_connection(&mut swarm).await?;

    // ---- 在 Relay 上预约 ----
    let reservation_id = swarm.listen_on(
        relay_addr.with(Protocol::P2pCircuit),
    )?;
    println!("[Gateway] Requesting relay reservation...");

    // 等待预约成功
    wait_for_reservation(&mut swarm, reservation_id).await?;

    println!("[Gateway] Relay reservation confirmed!");
    println!("[Gateway] External address: /p2p-circuit/p2p/{peer_id}");

    // ---- Stream 控制 ----
    let mut stream_control = swarm.behaviour().new_stream_control();

    // 注册入站协议
    let mut incoming_video = stream_control
        .accept(stream_protocols::VIDEO_PROTOCOL)
        .context("Failed to accept video protocol")?;

    let mut incoming_audio = stream_control
        .accept(stream_protocols::AUDIO_PROTOCOL)
        .context("Failed to accept audio protocol")?;

    // ---- 事件循环 ----
    // 注: stream 任务在出错/EOF 时自然结束；Relay 连接断开时返回 Err 触发重连。
    loop {
        tokio::select! {
            // Swarm 事件
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(behaviour::BehaviourEvent::RelayClient(
                        relay::client::Event::ReservationReqAccepted { .. },
                    )) => {
                        println!("[Gateway] Relay reservation accepted!");
                    }

                    SwarmEvent::Behaviour(behaviour::BehaviourEvent::Dcutr(
                        dcutr::Event { remote_peer_id, result, .. },
                    )) => match result {
                        Ok(_conn_id) => {
                            tracing::info!("[Gateway] DCUtR direct connection established with {remote_peer_id}");
                            tracing::info!("[Gateway] Direct connection upgrade successful - switching from relay to direct connection");
                        }
                        Err(err) => {
                            let err_str = err.to_string();
                            tracing::warn!("[Gateway] DCUtR failed with {remote_peer_id}: {err}");
                            tracing::warn!("[Gateway] Hole punch failed - continuing to use relay connection");
                            if err_str.contains("timeout") {
                                tracing::warn!("[Gateway] DCUtR failure cause: NAT type incompatibility or firewall blocking UDP");
                            } else if err_str.contains("IO error") || err_str.contains("connection refused") || err_str.contains("network unreachable") {
                                tracing::warn!("[Gateway] DCUtR failure cause: network unreachable or connection refused");
                            }
                        }
                    },

                    SwarmEvent::Behaviour(behaviour::BehaviourEvent::Identify(
                        identify::Event::Received { info, .. },
                    )) => {
                        tracing::info!("[Gateway] Identify received from peer:");
                        tracing::info!("  - Observed address: {}", info.observed_addr);
                        tracing::info!("  - Listen addresses ({} total):", info.listen_addrs.len());
                        for (i, addr) in info.listen_addrs.iter().enumerate() {
                            tracing::info!("    [{}]: {}", i, addr);
                        }
                        if let Some(Protocol::Ip4(ip)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                            if ip.is_private() {
                                tracing::warn!("[Gateway] WARNING: Observed address is private IP ({}) - DCUtR may fail!", ip);
                            } else {
                                tracing::info!("[Gateway] Observed address is public IP ({}) - good for DCUtR", ip);
                            }
                        }
                        if info.observed_addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                            tracing::info!("[Gateway] Observed address protocol: QUIC - good for DCUtR hole punching");
                        } else if info.observed_addr.iter().any(|p| matches!(p, Protocol::Tcp(_))) {
                            tracing::warn!("[Gateway] Observed address protocol: TCP only - DCUtR will produce TCP candidates, hole punching unlikely to succeed");
                        }
                    }

                    SwarmEvent::ListenerClosed {
                        listener_id,
                        reason: Err(e),
                        ..
                    } if listener_id == reservation_id => {
                        // Relay 预约丢失 → 返回 Err 触发重连
                        return Err(anyhow::anyhow!("Relay reservation lost: {e}"));
                    }

                    SwarmEvent::NewListenAddr { address, .. } => {
                        println!("[Gateway] Listening on: {address}");
                    }

                    SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                        let role = if endpoint.is_dialer() { "outgoing" } else { "incoming" };
                        let addr = endpoint.get_remote_address().clone();
                        connection_times.insert(peer_id, Instant::now());
                        tracing::info!("[Gateway] Connection established:");
                        tracing::info!("  - Peer ID: {peer_id}");
                        tracing::info!("  - Role: {role}");
                        tracing::info!("  - Remote address: {addr}");
                        if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                            tracing::info!("  - Type: Relay Circuit connection");
                        } else if addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                            tracing::info!("  - Type: QUIC direct connection");
                        } else {
                            tracing::info!("  - Type: Other connection");
                        }
                    }

                    SwarmEvent::ConnectionClosed { peer_id, endpoint, cause, num_established, .. } => {
                        let role = if endpoint.is_dialer() { "outgoing" } else { "incoming" };
                        let addr = endpoint.get_remote_address().clone();
                        let duration = connection_times.remove(&peer_id)
                            .map(|t| t.elapsed())
                            .map(|d| format!("{:.1}s", d.as_secs_f64()))
                            .unwrap_or_else(|| "unknown".to_string());
                        tracing::warn!("[Gateway] Connection closed:");
                        tracing::warn!("  - Peer ID: {peer_id}");
                        tracing::warn!("  - Role: {role}");
                        tracing::warn!("  - Remote address: {addr}");
                        tracing::warn!("  - Connection duration: {duration}");
                        if let Some(cause) = cause {
                            tracing::warn!("  - Cause: {cause}");
                        }
                        tracing::warn!("  - Remaining established connections: {num_established}");
                        if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                            tracing::warn!("  - Type: Relay Circuit connection - may trigger reconnect");
                        } else if addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                            tracing::warn!("  - Type: QUIC direct connection - falling back to relay");
                        }
                    }

                    _ => {
                        tracing::debug!("Event: {:?}", event);
                    }
                }
            }

            // 新的视频 stream 请求
            video = incoming_video.next() => {
                if let Some((peer_id, stream)) = video {
                    let rx = video_tx.subscribe();
                    println!("[Gateway] New video viewer: {peer_id}");
                    #[cfg(feature = "rv1106")]
                    let init_nals = rk_video_source::get_param_sets();
                    #[cfg(not(feature = "rv1106"))]
                    let init_nals = _param_sets.as_ref().and_then(|ps| {
                        ps.lock().ok()?.as_ref().map(|v| v.clone())
                    }).unwrap_or_default();
                    tokio::spawn(stream_video_to_viewer(peer_id, stream, rx, init_nals));
                } else {
                    // incoming stream accept 关闭 → 连接已断开，返回 Err 触发重连
                    return Err(anyhow::anyhow!("Video stream accept channel closed"));
                }
            }

            // 新的音频 stream 请求
            audio = incoming_audio.next() => {
                if let Some((peer_id, stream)) = audio {
                    let rx = audio_tx.subscribe();
                    println!("[Gateway] New audio viewer: {peer_id}");
                    tokio::spawn(stream_audio_to_viewer(peer_id, stream, rx));
                } else {
                    return Err(anyhow::anyhow!("Audio stream accept channel closed"));
                }
            }
        }
    }
}

/// 等待与 Relay 的连接建立 (带超时)
async fn wait_for_connection(
    swarm: &mut libp2p::Swarm<Behaviour>,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("Timeout waiting for relay connection");
        }
        match tokio::time::timeout(remaining, swarm.select_next_some()).await {
            Ok(SwarmEvent::ConnectionEstablished { peer_id, .. }) => {
                println!("[Gateway] Connected to relay {peer_id}");
                return Ok(());
            }
            Ok(SwarmEvent::OutgoingConnectionError { error, .. }) => {
                anyhow::bail!("Failed to connect to relay: {error}");
            }
            Ok(SwarmEvent::NewListenAddr { address, .. }) => {
                println!("[Gateway] Listening on: {address}");
            }
            Err(_elapsed) => {
                anyhow::bail!("Timeout waiting for relay connection");
            }
            _ => {}
        }
    }
}

/// 等待 Relay 预约确认
async fn wait_for_reservation(
    swarm: &mut libp2p::Swarm<Behaviour>,
    reservation_id: libp2p::core::transport::ListenerId,
) -> Result<()> {
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::Behaviour(behaviour::BehaviourEvent::RelayClient(
                relay::client::Event::ReservationReqAccepted { .. },
            )) => {
                return Ok(());
            }
            SwarmEvent::ListenerClosed {
                listener_id,
                reason: Err(e),
                ..
            } if listener_id == reservation_id => {
                anyhow::bail!("Reservation request rejected: {e}");
            }
            SwarmEvent::NewListenAddr { address, .. } => {
                println!("[Gateway] Listening on: {address}");
            }
            _ => {}
        }
    }
}

/// 发送视频帧到指定 viewer
async fn stream_video_to_viewer(
    peer_id: PeerId,
    mut stream: libp2p::swarm::Stream,
    mut source: broadcast::Receiver<MediaPacket>,
    init_nals: Vec<Vec<u8>>,
) {
    let mut frame_count: u64 = 0;

    // 先发送 VPS/SPS/PPS (让 viewer 立即能解码，不必等下一个 IDR)
    for nal in &init_nals {
        // init_nals 是原始 NAL data (不含 start code)，需要加 start code
        let mut au_with_sc = Vec::with_capacity(4 + nal.len());
        au_with_sc.extend_from_slice(&[0, 0, 0, 1]);
        au_with_sc.extend_from_slice(nal);
        let packet = MediaPacket::video(0, true, Bytes::from(au_with_sc));
        let encoded = packet.encode();
        if let Err(e) = stream.write_all(&encoded).await {
            tracing::warn!("Init NAL write to {peer_id} failed: {e}");
            return;
        }
    }
    if !init_nals.is_empty() {
        if let Err(e) = stream.flush().await {
            tracing::warn!("Init flush to {peer_id} failed: {e}");
            return;
        }
        println!("[Gateway] Sent {} init NALs to {peer_id}", init_nals.len());
    }

    loop {
        match source.recv().await {
            Ok(packet) => {
                let encoded = packet.encode();
                if let Err(e) = stream.write_all(&encoded).await {
                    tracing::warn!("Write to {peer_id} failed: {e}");
                    break;
                }
                if let Err(e) = stream.flush().await {
                    tracing::warn!("Flush to {peer_id} failed: {e}");
                    break;
                }
                frame_count += 1;
                if frame_count == 1 {
                    println!("[Gateway] First frame sent to {peer_id} ({} bytes, keyframe={})",
                        encoded.len(), packet.is_keyframe());
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                tracing::warn!("Video stream to {peer_id} lagged by {n} frames, requesting IDR");
                // 丢帧后请求 IDR，让 viewer 解码器在下一个关键帧重新同步
                #[cfg(feature = "rv1106")]
                rk_video_source::request_idr();
            }
            Err(broadcast::error::RecvError::Closed) => {
                println!("[Gateway] Broadcast closed for {peer_id} after {frame_count} frames");
                break;
            }
        }
    }
    let _ = stream.close().await;
    println!("[Gateway] Video stream to {peer_id} ended ({frame_count} frames sent)");
}

/// 发送音频帧到指定 viewer
async fn stream_audio_to_viewer(
    peer_id: PeerId,
    mut stream: libp2p::swarm::Stream,
    mut source: broadcast::Receiver<MediaPacket>,
) {
    loop {
        match source.recv().await {
            Ok(packet) => {
                let data = packet.encode();
                if let Err(e) = stream.write_all(&data).await {
                    tracing::warn!("Audio write to {peer_id} failed: {e}");
                    break;
                }
            }
            Err(broadcast::error::RecvError::Lagged(_)) => {
                // 音频丢帧可接受
            }
            Err(broadcast::error::RecvError::Closed) => break,
        }
    }
    let _ = stream.close().await;
    tracing::info!("Audio stream to {peer_id} ended");
}

/// 将 broadcast sender 包装为 crossbeam Sender
/// (用于从 std::thread 的媒体源发送到 tokio broadcast)
fn broadcast_sender_to_crossbeam(tx: broadcast::Sender<MediaPacket>) -> Sender<MediaPacket> {
    let (c_tx, c_rx) = crossbeam_channel::bounded::<MediaPacket>(BROADCAST_CAPACITY);

    // 用 spawn_blocking 而非 tokio::spawn — c_rx.recv() 是阻塞调用，
    // 在 tokio::spawn 里会永久占用一个 async worker 线程。
    tokio::task::spawn_blocking(move || {
        while let Ok(packet) = c_rx.recv() {
            if tx.send(packet).is_err() {
                break;
            }
        }
    });

    c_tx
}

#[derive(Debug, Parser)]
#[command(name = "p2p-camera gateway")]
struct Opt {
    /// Relay Server 地址
    #[arg(long)]
    relay: String,

    /// 运行模式 (listen = 作为媒体源等待 viewer 连接)
    #[arg(long, default_value = "listen")]
    mode: String,

    /// 身份密钥文件 (protobuf 格式)
    /// 首次运行自动生成，后续启动从此文件读取以保证 PeerId 不变
    #[arg(long, default_value = "gateway.key")]
    key_file: PathBuf,

    /// 视频裸流文件 (H.265) — 代替 SDK 回调 (非 rv1106 feature)
    #[cfg(not(feature = "rv1106"))]
    #[arg(long)]
    video_file: Option<std::path::PathBuf>,

    /// 启用模拟音频 (静音)
    #[arg(long, default_value_t = false)]
    enable_audio: bool,

    /// [rv1106] 视频宽度
    #[cfg(feature = "rv1106")]
    #[arg(long)]
    width: Option<u32>,

    /// [rv1106] 视频高度
    #[cfg(feature = "rv1106")]
    #[arg(long)]
    height: Option<u32>,

    /// [rv1106] 帧率
    #[cfg(feature = "rv1106")]
    #[arg(long)]
    fps: Option<u32>,

    /// [rv1106] 码率 (kbps)
    #[cfg(feature = "rv1106")]
    #[arg(long)]
    bitrate: Option<u32>,

    /// QUIC UDP 监听端口（若固定，便于端口映射）
    #[arg(long)]
    udp_port: Option<u16>,

    /// 手动指定公网 IP（配合 --udp-port 使用）
    #[arg(long)]
    external_ip: Option<String>,
}

fn validate_gateway_config(opt: &Opt) {
    let relay_str = &opt.relay;
    if relay_str.contains("/tcp/") && !relay_str.contains("/quic-v1") {
        tracing::warn!("[Gateway] WARNING: Using TCP relay connection - DCUtR will only produce TCP candidates, hole punching unlikely to succeed. Use /udp/<port>/quic-v1 instead");
    } else if relay_str.contains("/quic-v1") {
        tracing::info!("[Gateway] Relay connection protocol: QUIC - good for DCUtR hole punching");
    }

    if let Some(ref ip) = opt.external_ip {
        if let Ok(addr) = ip.parse::<std::net::IpAddr>() {
            if addr.is_ipv4() {
                let v4 = match addr {
                    std::net::IpAddr::V4(v4) => v4,
                    _ => unreachable!(),
                };
                if v4.is_private() {
                    tracing::error!("[Gateway] ERROR: External IP {} is a private IP - must be a public IP for DCUtR", ip);
                    std::process::exit(1);
                }
            }
        } else {
            tracing::error!("[Gateway] ERROR: Invalid external IP address: {}", ip);
            std::process::exit(1);
        }
    }

    if let Some(port) = opt.udp_port {
        if port == 0 {
            tracing::warn!("[Gateway] WARNING: Using random UDP port - cannot configure port forwarding for DCUtR");
        }
    }
}
