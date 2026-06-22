//! Viewer CLI — 端到端测试工具
//!
//! 用法:
//!   cargo run --example viewer_cli -- \
//!     --relay /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER> \
//!     --camera <GATEWAY_PEER_ID> \
//!     --output output.h265
//!
//! 验证流程:
//!   1. 连接 Relay Server
//!   2. 通过 Circuit 拨号 Gateway
//!   3. 打开视频 stream 接收帧
//!   4. 保存到文件 (可用 ffplay 播放)
//!   5. 打印接收统计

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use clap::Parser;
use futures::{AsyncReadExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, identify, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId,
};
use libp2p_stream;
use proto::{media_packet::MediaPacket, stream_protocols};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

const STREAM_READ_BUF: usize = 65536;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let opt = Opt::parse();

    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let local_peer_id = keypair.public().to_peer_id();
    println!("[Viewer] PeerId: {local_peer_id}");

    // ---- 构建 Swarm ----
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
            Ok(ViewerBehaviour::new(key.public(), relay_client))
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    // ---- 1. 连接 Relay ----
    let relay_addr: Multiaddr = opt.relay.parse().context("Invalid relay address")?;
    println!("[Viewer] Dialing relay: {relay_addr}");
    swarm.dial(relay_addr.clone())?;

    // 等待连接
    wait_for_event(&mut swarm, |e| matches!(
        e,
        SwarmEvent::ConnectionEstablished { .. }
    ), "relay connection")
    .await?;

    // ---- 2. 通过 Circuit 拨号 Gateway ----
    let gateway: PeerId = opt.camera.parse().context("Invalid camera PeerId")?;
    let circuit_addr = relay_addr
        .with(Protocol::P2pCircuit)
        .with(Protocol::P2p(gateway));
    println!("[Viewer] Dialing gateway via circuit: {circuit_addr}");
    swarm.dial(circuit_addr)?;

    wait_for_event(&mut swarm, |e| matches!(
        e,
        SwarmEvent::ConnectionEstablished { peer_id, .. } if *peer_id == gateway
    ), "gateway circuit connection")
    .await?;

    println!("[Viewer] Connected to gateway {gateway}");

    // ---- 3. 打开视频 stream ----
    let mut stream_control = swarm.behaviour().stream.new_control();
    let video_stream = stream_control
        .open_stream(gateway, stream_protocols::VIDEO_PROTOCOL)
        .await
        .context("Failed to open video stream")?;
    println!("[Viewer] Video stream opened");

    // ---- 4. 启动接收任务 ----
    let (tx, mut rx) = mpsc::channel::<MediaPacket>(60);
    tokio::spawn(receive_frames(gateway, video_stream, tx));

    // ---- 5. 主循环: 驱动 swarm + 接收帧写文件 ----
    let mut output_file = if let Some(path) = &opt.output {
        Some(std::fs::File::create(path).context("Failed to create output file")?)
    } else {
        None
    };

    let mut frame_count: u64 = 0;
    let mut bytes_received: u64 = 0;
    let start = std::time::Instant::now();

    println!("[Viewer] Receiving video frames... (Ctrl+C to stop)");

    loop {
        tokio::select! {
            // 驱动 Swarm 事件
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                        dcutr::Event { result: Ok(_), remote_peer_id, .. },
                    )) => {
                        println!("[Viewer] DCUtR direct connection established with {remote_peer_id}");
                    }
                    SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                        dcutr::Event { result: Err(e), remote_peer_id, .. },
                    )) => {
                        tracing::warn!("[Viewer] DCUtR failed with {remote_peer_id}: {e}");
                    }
                    e => {
                        tracing::debug!("[Viewer] Event: {:?}", e);
                    }
                }
            }
            // 接收视频帧
            packet = rx.recv() => {
                let Some(packet) = packet else {
                    println!("[Viewer] Video stream ended");
                    break;
                };

                frame_count += 1;
                bytes_received += packet.data.len() as u64;

                // 写入文件 (H.265 裸流, 可用 ffplay 播放)
                if let Some(file) = &mut output_file {
                    use std::io::Write;
                    // packet.data 是完整的 access unit (已含 start code)
                    // 直接写入，不加额外 start code
                    file.write_all(&packet.data)?;
                    file.flush()?;
                }

                // 每 100 帧打印统计
                if frame_count % 100 == 0 {
                    let elapsed = start.elapsed().as_secs_f64();
                    let fps = frame_count as f64 / elapsed;
                    let kbps = (bytes_received * 8) as f64 / elapsed / 1000.0;
                    let keyframe = if packet.is_keyframe() { "[I]" } else { "   " };
                    println!(
                        "[Viewer] {keyframe} frame #{frame_count} | {fps:.1} fps | {kbps:.0} kbps | ts={}",
                        packet.timestamp_ms
                    );
                }
            }
        }
    }

    let elapsed = start.elapsed().as_secs_f64();
    println!("\n[Viewer] === Summary ===");
    println!("[Viewer] Total frames: {frame_count}");
    println!("[Viewer] Total bytes: {bytes_received}");
    println!("[Viewer] Duration: {elapsed:.1}s");
    println!("[Viewer] Avg fps: {:.1}", frame_count as f64 / elapsed);
    println!("[Viewer] Avg bitrate: {:.0} kbps", (bytes_received * 8) as f64 / elapsed / 1000.0);

    if let Some(path) = &opt.output {
        println!("[Viewer] Output saved to: {}", path.display());
        println!("[Viewer] Play with: ffplay -f hevc {}", path.display());
    }

    Ok(())
}

/// 从 stream 持续读取帧
async fn receive_frames(
    peer_id: PeerId,
    mut stream: libp2p::swarm::Stream,
    sender: mpsc::Sender<MediaPacket>,
) {
    let mut buf = BytesMut::with_capacity(STREAM_READ_BUF);
    let mut read_buf = vec![0u8; STREAM_READ_BUF];

    loop {
        match stream.read(&mut read_buf).await {
            Ok(0) => {
                println!("[Viewer] Stream EOF from {peer_id}");
                break;
            }
            Ok(n) => {
                buf.extend_from_slice(&read_buf[..n]);
                // 尝试解码所有完整的包
                while let Some(packet) = MediaPacket::try_decode(&mut buf) {
                    if sender.send(packet).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("[Viewer] Stream read error: {e}");
                break;
            }
        }
    }
}

/// 等待特定事件 (带超时)
async fn wait_for_event(
    swarm: &mut libp2p::Swarm<ViewerBehaviour>,
    predicate: impl Fn(&SwarmEvent<ViewerBehaviourEvent>) -> bool,
    label: &str,
) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            anyhow::bail!("Timeout waiting for {label}");
        }

        let event = tokio::time::timeout(remaining, swarm.select_next_some())
            .await
            .context("Timeout waiting for {label}")?;

        if predicate(&event) {
            return Ok(());
        }

        // 处理错误事件
        if let SwarmEvent::OutgoingConnectionError { error, .. } = &event {
            tracing::warn!("[Viewer] Connection error ({label}): {error}");
        }
    }
}

// ---- NetworkBehaviour ----

#[derive(NetworkBehaviour)]
struct ViewerBehaviour {
    relay_client: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    identify: identify::Behaviour,
    ping: ping::Behaviour,
    stream: libp2p_stream::Behaviour,
}

impl ViewerBehaviour {
    fn new(
        local_public_key: libp2p::identity::PublicKey,
        relay_client: relay::client::Behaviour,
    ) -> Self {
        let peer_id = local_public_key.to_peer_id();
        Self {
            relay_client,
            dcutr: dcutr::Behaviour::new(peer_id),
            identify: identify::Behaviour::new(
                identify::Config::new(
                    "/p2p-camera-viewer/1.0.0".to_string(),
                    local_public_key,
                ),
            ),
            ping: ping::Behaviour::new(
                ping::Config::default()
                    .with_interval(Duration::from_secs(10)),
            ),
            stream: libp2p_stream::Behaviour::new(),
        }
    }
}

// ---- CLI ----

#[derive(Debug, Parser)]
#[command(name = "viewer-cli")]
struct Opt {
    /// Relay Server 地址
    #[arg(long)]
    relay: String,

    /// 摄像头 (Gateway) PeerId
    #[arg(long)]
    camera: String,

    /// 输出文件路径 (H.265 裸流, 可选)
    #[arg(long)]
    output: Option<std::path::PathBuf>,
}
