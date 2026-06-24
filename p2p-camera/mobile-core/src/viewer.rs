//! P2P Viewer 核心逻辑 — 移动端接收侧
//!
//! 负责:
//! 1. 连接 Relay Server
//! 2. 通过 Circuit 拨号 Gateway
//! 3. DCUtR 直连协商
//! 4. 打开视频/音频 stream
//! 5. 接收 MediaPacket → 送入 Jitter Buffer

use std::time::Duration;

use anyhow::{Context, Result};
use futures::{AsyncReadExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, identify, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, Swarm, PeerId,
};
use libp2p_stream::{self, Control};
use proto::{
    media_packet::MediaPacket,
    stream_protocols,
};
use tokio::sync::mpsc;

use crate::jitter_buffer::AvJitterBuffer;

const STREAM_READ_BUF: usize = 65536; // 64KB

/// P2P Viewer — 对外暴露的核心结构
pub struct P2pViewer {
    swarm: Swarm<ViewerBehaviour>,
    stream_control: Control,
    jitter: AvJitterBuffer,
    /// 视频帧 → UI 层
    video_sender: mpsc::Sender<MediaPacket>,
    /// UI 层 → 视频帧 (给 Native 层轮询)
    video_receiver: mpsc::Receiver<MediaPacket>,
    /// 音频帧 → UI 层
    audio_sender: mpsc::Sender<MediaPacket>,
    /// UI 层 → 音频帧
    audio_receiver: mpsc::Receiver<MediaPacket>,
    /// 连接状态
    pub connected: bool,
}

impl P2pViewer {
    /// 创建新的 Viewer 实例
    pub async fn new() -> Result<Self> {
        let keypair = libp2p::identity::Keypair::generate_ed25519();

        let swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_quic()
            .with_relay_client(noise::Config::new, libp2p::yamux::Config::default)?
            .with_behaviour(|key, relay_client| {
                Ok(ViewerBehaviour::new(key.public(), relay_client))
            })?
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
            .build();

        let stream_control = swarm.behaviour().stream.new_control();

        let (video_sender, video_receiver) = mpsc::channel::<MediaPacket>(60);
        let (audio_sender, audio_receiver) = mpsc::channel::<MediaPacket>(200);

        Ok(Self {
            swarm,
            stream_control,
            jitter: AvJitterBuffer::new(
                Duration::from_millis(100),  // 视频缓冲 100ms
                Duration::from_millis(50),   // 音频缓冲 50ms
            ),
            video_sender,
            video_receiver,
            audio_sender,
            audio_receiver,
            connected: false,
        })
    }

    /// 连接 Relay 并通过 Circuit 拨号 Gateway
    pub async fn connect(
        &mut self,
        relay_addr: &str,
        gateway_peer_id: &str,
    ) -> Result<()> {
        // 1. 连接 Relay
        let relay: Multiaddr = relay_addr.parse()?;
        self.swarm.dial(relay.clone())?;

        // 等待连接建立
        self.wait_for_connection().await?;

        // 2. 通过 Circuit 拨号 Gateway
        let gateway: PeerId = gateway_peer_id.parse()?;
        let circuit_addr = relay
            .with(Protocol::P2pCircuit)
            .with(Protocol::P2p(gateway));

        self.swarm.dial(circuit_addr)?;
        self.wait_for_connection().await?;

        // 3. 打开视频 stream
        let video_stream = self.stream_control
            .open_stream(gateway, stream_protocols::VIDEO_PROTOCOL)
            .await
            .context("Failed to open video stream")?;

        // 4. 打开音频 stream
        let audio_stream = self.stream_control
            .open_stream(gateway, stream_protocols::AUDIO_PROTOCOL)
            .await
            .context("Failed to open audio stream")?;

        println!("[Viewer] Video + Audio streams opened");

        // 5. 启动接收任务
        let video_sender = self.video_sender.clone();
        let audio_sender = self.audio_sender.clone();

        tokio::spawn(Self::receive_frames(gateway, video_stream, video_sender));
        tokio::spawn(Self::receive_frames(gateway, audio_stream, audio_sender));

        self.connected = true;
        Ok(())
    }

    /// 获取下一个视频帧 (供 Native UI 层轮询)
    pub fn poll_video_frame(&mut self) -> Option<MediaPacket> {
        // 先尝试从 Jitter Buffer 取
        if let Some(frame) = self.jitter.next_video() {
            return Some(frame);
        }
        // 再尝试从接收 channel 取新包送入 jitter
        while let Ok(packet) = self.video_receiver.try_recv() {
            self.jitter.push(packet);
        }
        self.jitter.next_video()
    }

    /// 驱动 Swarm 事件循环 (需要定期调用)
    pub async fn poll_swarm(&mut self) {
        if let Some(event) = self.swarm.next().await {
            match event {
                SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                    dcutr::Event { result: Ok(_), remote_peer_id, .. },
                )) => {
                    println!("[Viewer] Direct connection established with {remote_peer_id}");
                }
                _ => {
                    tracing::debug!("Viewer swarm event: {:?}", event);
                }
            }
        }
    }

    // ---- 内部方法 ----

    async fn wait_for_connection(&mut self) -> Result<()> {
        loop {
            match self.swarm.select_next_some().await {
                SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                    println!("[Viewer] Connected to {peer_id}");
                    return Ok(());
                }
                SwarmEvent::OutgoingConnectionError { error, .. } => {
                    anyhow::bail!("Connection failed: {error}");
                }
                e => {
                    tracing::debug!("Viewer event: {:?}", e);
                }
            }
        }
    }

    /// 从 stream 持续读取帧，送入 channel
    async fn receive_frames(
        peer_id: PeerId,
        mut stream: libp2p::swarm::Stream,
        sender: mpsc::Sender<MediaPacket>,
    ) {
        let mut buf = bytes::BytesMut::with_capacity(STREAM_READ_BUF);
        let mut read_buf = vec![0u8; STREAM_READ_BUF];

        loop {
            match stream.read(&mut read_buf).await {
                Ok(0) => break, // EOF
                Ok(n) => {
                    buf.extend_from_slice(&read_buf[..n]);

                    // 尝试解码所有完整的包
                    while let Some(packet) = MediaPacket::try_decode(&mut buf) {
                        if sender.send(packet).await.is_err() {
                            break; // 接收端已关闭
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Stream read error from {peer_id}: {e}");
                    break;
                }
            }
        }
    }
}

#[derive(NetworkBehaviour)]
pub struct ViewerBehaviour {
    pub relay_client: relay::client::Behaviour,
    pub dcutr: dcutr::Behaviour,
    pub identify: identify::Behaviour,
    pub ping: libp2p::ping::Behaviour,
    pub stream: libp2p_stream::Behaviour,
}

impl ViewerBehaviour {
    pub fn new(
        local_public_key: libp2p::identity::PublicKey,
        relay_client: relay::client::Behaviour,
    ) -> Self {
        let peer_id = local_public_key.to_peer_id();
        Self {
            relay_client,  // 使用 builder 传入的
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
