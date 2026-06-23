//! P2P Camera Gateway — 运行在 RV1106 上的媒体网关
//!
//! 职责:
//! 1. 连接 Relay Server 并在其上预约 (Circuit Relay v2 Reservation)
//! 2. 通过 DCUtR 与 Viewer 协商直连
//! 3. 接受 Viewer 的视频/音频 stream 请求
//! 4. 从媒体源 (SDK/文件) 读取帧并通过 stream 发送
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

use std::time::Duration;

use anyhow::{Context, Result};
use behaviour::Behaviour;
use bytes::Bytes;
use clap::Parser;
use crossbeam_channel::Sender;
use futures::{AsyncWriteExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, identity, noise, relay,
    swarm::SwarmEvent,
    tcp, yamux,
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

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let opt = Opt::parse();

    // ---- 构建 Swarm ----
    let keypair = identity::Keypair::generate_ed25519();
    let peer_id = keypair.public().to_peer_id();

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay_client| {
            Ok(Behaviour::new(key.public(), relay_client))
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();

    println!("[Gateway] PeerId: {peer_id}");

    // ---- 连接 Relay Server ----
    let relay_addr: Multiaddr = opt.relay.parse()
        .context("Invalid relay address")?;

    swarm.dial(relay_addr.clone())?;
    println!("[Gateway] Dialing relay: {relay_addr}");

    // 等待与 Relay 的连接建立
    wait_for_connection(&mut swarm, |_| true).await?;

    // ---- 在 Relay 上预约 ----
    let reservation_id = swarm.listen_on(
        relay_addr.with(Protocol::P2pCircuit),
    )?;
    println!("[Gateway] Requesting relay reservation...");

    // 等待预约成功
    wait_for_reservation(&mut swarm, reservation_id).await?;

    println!("[Gateway] Relay reservation confirmed!");
    println!("[Gateway] External address: /p2p-circuit/p2p/{peer_id}");

    // ---- 初始化媒体源 (文件 or RV1106 SDK) ----
    let (video_tx, _video_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);
    let (audio_tx, _audio_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);

    // 参数集缓存 (VPS/SPS/PPS) — 新 viewer 连接时先发送这些，避免 "PPS id out of range"
    let param_sets: Option<std::sync::Arc<std::sync::Mutex<Option<Vec<Vec<u8>>>>>>;
    // 视频源启动触发器 — 第一个 viewer 连接时才开始播放 (从文件头)
    let mut video_start_trigger: Option<crossbeam_channel::Sender<()>>;

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
        let (_, start_tx) = source.spawn(broadcast_sender_to_crossbeam(video_tx.clone()));
        video_start_trigger = Some(start_tx);
    }

    #[cfg(not(feature = "rv1106"))]
    {
        if let Some(video_path) = &opt.video_file {
            let data = std::fs::read(video_path)
                .context("Failed to read video file")?;
            println!("[Gateway] Video file: {:?} ({} bytes)", video_path, data.len());
            let source = media_source::FileVideoSource::from_file(data);
            param_sets = Some(source.param_sets_handle());
            let (_, start_tx) = source.spawn(broadcast_sender_to_crossbeam(video_tx.clone()));
            video_start_trigger = Some(start_tx);
            println!("[Gateway] Video source: file ({:?}) — waiting for first viewer to start", video_path);
        } else {
            println!("[Gateway] Video source: NONE (waiting for stream requests)");
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
    // 注: 原型阶段不主动断开 viewer，stream 任务在出错/EOF 时自然结束。
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
                            println!("[Gateway] DCUtR direct connection established with {remote_peer_id}");
                        }
                        Err(err) => {
                            tracing::warn!("DCUtR failed with {remote_peer_id}: {err}");
                        }
                    },

                    SwarmEvent::ListenerClosed {
                        listener_id,
                        reason: Err(e),
                        ..
                    } if listener_id == reservation_id => {
                        anyhow::bail!("Relay reservation failed: {e}");
                    }

                    SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                        println!("[Gateway] Connection established: {peer_id}");
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
                    // 第一个 viewer 连接时触发视频源开始播放 (从文件头)
                    if let Some(tx) = video_start_trigger.take() {
                        let _ = tx.send(());
                    }
                    // 先发送缓存的 VPS/SPS/PPS，让 viewer 立即能解码
                    let init_nals = param_sets.as_ref().and_then(|ps| {
                        ps.lock().ok()?.as_ref().map(|v| v.clone())
                    }).unwrap_or_default();
                    tokio::spawn(stream_video_to_viewer(peer_id, stream, rx, init_nals));
                } else { break Ok(()); }
            }

            // 新的音频 stream 请求
            audio = incoming_audio.next() => {
                if let Some((peer_id, stream)) = audio {
                    let rx = audio_tx.subscribe();
                    println!("[Gateway] New audio viewer: {peer_id}");
                    tokio::spawn(stream_audio_to_viewer(peer_id, stream, rx));
                } else { break Ok(()); }
            }
        }
    }
}

/// 等待与对方的连接建立 (用于 Relay 连接)
async fn wait_for_connection(
    swarm: &mut libp2p::Swarm<Behaviour>,
    _predicate: impl Fn(&PeerId) -> bool,
) -> Result<()> {
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                println!("[Gateway] Connected to {peer_id}");
                return Ok(());
            }
            SwarmEvent::OutgoingConnectionError { error, .. } => {
                anyhow::bail!("Failed to connect: {error}");
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
                tracing::warn!("Video stream to {peer_id} lagged by {n} frames");
                // 继续, 跳过丢失的帧
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
}
