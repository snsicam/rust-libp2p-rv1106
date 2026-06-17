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

use std::{
    collections::HashMap,
    error::Error,
    path::PathBuf,
    time::Duration,
};

use anyhow::{Context, Result};
use behaviour::Behaviour;
use bytes::Bytes;
use clap::Parser;
use crossbeam_channel::Sender;
use futures::StreamExt;
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    identity, noise,
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
async fn main() -> Result<(), Box<dyn Error>> {
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

    // ---- 初始化媒体源 (文件 or SDK) ----
    let (video_tx, mut video_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);
    let (audio_tx, _audio_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);

    if let Some(video_path) = &opt.video_file {
        let data = std::fs::read(video_path)
            .context("Failed to read video file")?;
        let source = media_source::FileVideoSource::from_file(data);
        source.spawn(broadcast_sender_to_crossbeam(video_tx.clone()));
        println!("[Gateway] Video source: file ({:?})", video_path);
    } else {
        println!("[Gateway] Video source: NONE (waiting for stream requests)");
    }

    // 音频源: 模拟静音 (for 原型)
    if opt.enable_audio {
        let source = media_source::SilenceAudioSource::new(16000, 1);
        source.spawn(broadcast_sender_to_crossbeam(audio_tx.clone()));
        println!("[Gateway] Audio source: silence (16kHz mono)");
    }

    // ---- Stream 控制 ----
    let stream_control = swarm.behaviour().new_stream_control();

    // 注册入站协议
    let mut incoming_video = stream_control
        .accept(stream_protocols::VIDEO_PROTOCOL)
        .context("Failed to accept video protocol")?;

    let mut incoming_audio = stream_control
        .accept(stream_protocols::AUDIO_PROTOCOL)
        .context("Failed to accept audio protocol")?;

    // ---- 事件循环 ----
    let mut viewer_listeners: HashMap<PeerId, tokio::sync::oneshot::Sender<()>> = HashMap::new();

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
                        Ok(conn_id) => {
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
            video = incoming_video.select_next_some() => {
                let (peer_id, stream) = video;
                let mut rx = video_rx.subscribe();
                let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
                viewer_listeners.insert(peer_id, kill_tx);

                println!("[Gateway] New video viewer: {peer_id}");
                tokio::spawn(stream_video_to_viewer(peer_id, stream, rx, kill_rx));
            }

            // 新的音频 stream 请求
            audio = incoming_audio.select_next_some() => {
                let (peer_id, stream) = audio;
                let mut rx = audio_tx.subscribe();
                let (kill_tx, kill_rx) = tokio::sync::oneshot::channel::<()>();
                viewer_listeners.insert(peer_id, kill_tx);

                println!("[Gateway] New audio viewer: {peer_id}");
                tokio::spawn(stream_audio_to_viewer(peer_id, stream, rx, kill_rx));
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
    kill_rx: tokio::sync::oneshot::Receiver<()>,
) {
    tokio::pin!(kill_rx);
    loop {
        tokio::select! {
            _ = &mut kill_rx => {
                let _ = stream.close().await;
                break;
            }
            packet = source.recv() => {
                match packet {
                    Ok(packet) => {
                        let encoded = packet.encode();
                        use futures::AsyncWriteExt;
                        if let Err(e) = stream.write_all(&encoded).await {
                            tracing::warn!("Write to {peer_id} failed: {e}");
                            break;
                        }
                        if let Err(e) = stream.flush().await {
                            tracing::warn!("Flush to {peer_id} failed: {e}");
                            break;
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("Video stream to {peer_id} lagged by {n} frames");
                        // 继续, 跳过丢失的帧
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        }
    }
    tracing::info!("Video stream to {peer_id} ended");
}

/// 发送音频帧到指定 viewer
async fn stream_audio_to_viewer(
    peer_id: PeerId,
    mut stream: libp2p::swarm::Stream,
    mut source: broadcast::Receiver<MediaPacket>,
    kill_rx: tokio::sync::oneshot::Receiver<()>,
) {
    // 同视频逻辑, 但容忍更大的滞后 (音频帧小)
    tokio::pin!(kill_rx);
    loop {
        tokio::select! {
            _ = &mut kill_rx => {
                let _ = stream.close().await;
                break;
            }
            packet = source.recv() => {
                match packet {
                    Ok(packet) => {
                        let data = packet.encode();
                        use futures::AsyncWriteExt;
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
        }
    }
}

/// 将 broadcast sender 包装为 crossbeam Sender
/// (用于从 std::thread 的媒体源发送到 tokio broadcast)
fn broadcast_sender_to_crossbeam(tx: broadcast::Sender<MediaPacket>) -> Sender<MediaPacket> {
    let (c_tx, c_rx) = crossbeam_channel::bounded::<MediaPacket>(BROADCAST_CAPACITY);

    // 后台任务: crossbeam → broadcast
    tokio::spawn(async move {
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

    /// 视频裸流文件 (H.265) — 代替 SDK 回调
    #[arg(long)]
    video_file: Option<PathBuf>,

    /// 启用模拟音频 (静音)
    #[arg(long, default_value_t = false)]
    enable_audio: bool,
}
