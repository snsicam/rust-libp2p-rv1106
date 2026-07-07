//! P2P Viewer 核心逻辑 — 移动端接收侧
//!
//! 负责:
//! 1. 连接 Relay Server (支持多 Relay 并发拨号)
//! 2. 通过 Circuit 拨号 DeviceCam
//! 3. mDNS 局域网发现 (优先于 Relay)
//! 4. DCUtR 直连协商
//! 5. 打开视频/音频 stream
//! 6. 接收 MediaPacket → 送入 Jitter Buffer

use std::net::Ipv4Addr;
use std::time::Duration;

use anyhow::{Context, Result};
use futures::{AsyncReadExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, identify, mdns, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, StreamProtocol, Swarm, PeerId,
};
use libp2p_stream::{self, Control};
use proto::{
    media_packet::MediaPacket,
    stream_protocols,
};
use tokio::sync::mpsc;

use crate::jitter_buffer::AvJitterBuffer;
use crate::net_diag::{ConnectionQuality, ConnectionType, NatDiagnostic, NatDiagnosis};

const STREAM_READ_BUF: usize = 65536; // 64KB
const MDNS_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);

/// 将 stream 名称映射到对应的协议
fn get_video_protocol(stream_type: &str) -> StreamProtocol {
    match stream_type {
        "sub" => stream_protocols::VIDEO_SUB_PROTOCOL,
        "third" => stream_protocols::VIDEO_THIRD_PROTOCOL,
        _ => stream_protocols::VIDEO_MAIN_PROTOCOL,
    }
}

/// 码流选择决策器：根据连接方式和用户偏好决定使用哪个码流
///
/// - "auto" 模式：转发→子码流，直连→主码流
/// - 手动模式：直接使用用户指定的码流
struct StreamTypeResolver {
    /// 用户指定的 stream_type: "auto" | "main" | "sub" | "third"
    stream_type: String,
    /// 当前连接方式
    connection_type: ConnectionType,
}

impl StreamTypeResolver {
    fn new(stream_type: &str) -> Self {
        Self {
            stream_type: stream_type.to_string(),
            connection_type: ConnectionType::Disconnected,
        }
    }

    /// 根据连接方式决定码流协议
    fn resolve(&self) -> StreamProtocol {
        match self.stream_type.as_str() {
            "auto" => {
                // 自动选择：转发→子码流，直连→主码流，未知→子码流（保守策略）
                match self.connection_type {
                    ConnectionType::RelayCircuit => stream_protocols::VIDEO_SUB_PROTOCOL,
                    ConnectionType::QuicDirect
                    | ConnectionType::LanDirect
                    | ConnectionType::TcpDirect => stream_protocols::VIDEO_MAIN_PROTOCOL,
                    ConnectionType::Disconnected => stream_protocols::VIDEO_SUB_PROTOCOL,
                }
            }
            other => get_video_protocol(other),
        }
    }

    /// 更新连接方式（ConnectionEstablished 时调用）
    fn update_connection_type(&mut self, conn_type: ConnectionType) {
        self.connection_type = conn_type;
    }

    /// 判断是否需要码流切换（从转发升级到直连时）
    fn should_upgrade_stream(&self, old_conn: ConnectionType) -> bool {
        self.stream_type == "auto"
            && old_conn.is_relay()
            && self.connection_type.is_direct()
    }

    /// 直连升级后应使用的码流协议（主码流）
    fn upgrade_protocol(&self) -> StreamProtocol {
        stream_protocols::VIDEO_MAIN_PROTOCOL
    }
}

/// P2P Viewer — 对外暴露的核心结构
pub struct P2pViewer {
    swarm: Swarm<ViewerBehaviour>,
    stream_control: Control,
    jitter: AvJitterBuffer,
    video_sender: mpsc::Sender<MediaPacket>,
    video_receiver: mpsc::Receiver<MediaPacket>,
    audio_sender: mpsc::Sender<MediaPacket>,
    audio_receiver: mpsc::Receiver<MediaPacket>,
    nat_diagnostic: NatDiagnostic,
    connection_quality: ConnectionQuality,
    device_cam_peer_id: Option<PeerId>,
    lan_direct_attempted: bool,
    stream_resolver: StreamTypeResolver,
    video_abort_handle: Option<tokio::task::AbortHandle>,
    relay_connection_id: Option<libp2p::swarm::ConnectionId>,
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
                ViewerBehaviour::new(key.public(), relay_client)
            })?
            // idle timeout 120s: DCUtR handler 在重试期间需要 keep-alive，0 会导致连接被意外关闭
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
            .build();

        let stream_control = swarm.behaviour().stream.new_control();

        let (video_sender, video_receiver) = mpsc::channel::<MediaPacket>(60);
        let (audio_sender, audio_receiver) = mpsc::channel::<MediaPacket>(200);

        Ok(Self {
            swarm,
            stream_control,
            jitter: AvJitterBuffer::new(
                Duration::from_millis(100),
                Duration::from_millis(50),
            ),
            video_sender,
            video_receiver,
            audio_sender,
            audio_receiver,
            nat_diagnostic: NatDiagnostic::new(0, Vec::new()),
            connection_quality: ConnectionQuality::default(),
            device_cam_peer_id: None,
            lan_direct_attempted: false,
            stream_resolver: StreamTypeResolver::new("auto"),
            video_abort_handle: None,
            relay_connection_id: None,
            connected: false,
        })
    }

    /// 连接 Relay 并通过 Circuit 拨号 DeviceCam
    ///
    /// 支持多 Relay 并发拨号和 mDNS 局域网发现:
    /// - 同时拨号所有 Relay，第一个成功即用
    /// - 如果 enable_mdns，并行监听 mDNS 发现事件
    /// - mDNS 发现目标 DeviceCam 时，优先使用 LAN 直连
    /// - mDNS 5 秒超时后回退到 Relay circuit
    /// - stream_type: "auto" | "main" | "sub" | "third" 选择请求哪个码流
    ///   "auto" 模式下根据连接方式自动选择：转发→子码流，直连→主码流
    pub async fn connect(
        &mut self,
        relay_addrs: &[String],
        device_cam_peer_id: &str,
        enable_mdns: bool,
        stream_type: &str,
    ) -> Result<()> {
        let device_cam: PeerId = device_cam_peer_id.parse()?;
        self.device_cam_peer_id = Some(device_cam);

        // 初始化码流选择器
        self.stream_resolver = StreamTypeResolver::new(stream_type);

        // 解析所有 Relay 地址
        let relay_multiaddrs: Vec<Multiaddr> = relay_addrs.iter()
            .map(|s| s.parse::<Multiaddr>())
            .collect::<std::result::Result<Vec<_>, _>>()?;

        // 同时拨号所有 Relay
        for addr in &relay_multiaddrs {
            if let Err(e) = self.swarm.dial(addr.clone()) {
                tracing::warn!("[Viewer] Failed to dial relay {addr}: {e}");
            }
        }

        // 并行等待: mDNS 发现 或 Relay 连接
        let mdns_deadline = tokio::time::Instant::now() + MDNS_DISCOVERY_TIMEOUT;
        let mut connected_relay: Option<(PeerId, Multiaddr)> = None;
        let mut mdns_discovered_addr: Option<Multiaddr> = None;
        let mut relay_error_count: usize = 0;
        let total_relays = relay_multiaddrs.len();

        loop {
            let now = tokio::time::Instant::now();
            let mdns_expired = !enable_mdns || now >= mdns_deadline;

            // 如果 mDNS 发现了目标，优先使用
            if mdns_discovered_addr.is_some() {
                break;
            }

            // 如果 mDNS 超时且已有 relay 连接，使用 relay
            if mdns_expired && connected_relay.is_some() {
                break;
            }

            // 如果所有 relay 都失败了且 mDNS 也超时/禁用，退出
            if relay_error_count >= total_relays && mdns_expired {
                break;
            }

            let remaining = if connected_relay.is_none() && mdns_discovered_addr.is_none() {
                Duration::from_secs(30)
            } else {
                Duration::from_millis(100) // 短暂等待后退出
            };

            match tokio::time::timeout(remaining, self.swarm.select_next_some()).await {
                Ok(event) => {
                    match event {
                        SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                            let addr = endpoint.get_remote_address().clone();
                            // 检查是否为 Relay 连接
                            let is_relay = relay_multiaddrs.iter().any(|r| {
                                r.iter().any(|p| matches!(p, Protocol::P2p(pid) if pid == peer_id))
                            });
                            if is_relay && connected_relay.is_none() {
                                tracing::info!("[Viewer] Connected to relay {peer_id}");
                                connected_relay = Some((peer_id, addr));
                            }
                        }
                        SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                            mdns::Event::Discovered(peers),
                        )) => {
                            for (peer_id, addr) in peers {
                                tracing::info!("[Viewer] mDNS discovered peer {peer_id} at {addr}");
                                if peer_id == device_cam && mdns_discovered_addr.is_none() {
                                    tracing::info!("[Viewer] mDNS found target DeviceCam {peer_id} at {addr}");
                                    mdns_discovered_addr = Some(addr);
                                }
                            }
                        }
                        SwarmEvent::OutgoingConnectionError { peer_id: Some(peer_id), error, .. } => {
                            let is_relay = relay_multiaddrs.iter().any(|r| {
                                r.iter().any(|p| matches!(p, Protocol::P2p(pid) if pid == peer_id))
                            });
                            if is_relay {
                                tracing::warn!("[Viewer] Failed to connect to relay {peer_id}: {error}");
                                relay_error_count += 1;
                            } else {
                                tracing::warn!("[Viewer] Connection error: {error}");
                            }
                        }
                        SwarmEvent::OutgoingConnectionError { error, .. } => {
                            tracing::warn!("[Viewer] Connection error: {error}");
                        }
                        _ => {
                            tracing::debug!("Viewer event: {:?}", event);
                        }
                    }
                }
                Err(_) => {
                    // timeout, check if we should break
                    if mdns_discovered_addr.is_some() || connected_relay.is_some() {
                        break;
                    }
                }
            }
        }

        // 优先使用 mDNS 发现的 LAN 直连
        if let Some(lan_addr) = mdns_discovered_addr {
            tracing::info!("[Viewer] Using mDNS-discovered LAN address: {lan_addr}");
            self.swarm.dial(lan_addr)?;
            let conn_type = self.wait_for_connection_and_classify().await?;
            self.stream_resolver.update_connection_type(conn_type);
        } else if let Some((_relay_peer_id, relay_addr)) = connected_relay {
            // 通过 Relay circuit 拨号 DeviceCam
            let circuit_addr = relay_addr
                .with(Protocol::P2pCircuit)
                .with(Protocol::P2p(device_cam));
            tracing::info!("[Viewer] Dialing DeviceCam via circuit: {circuit_addr}");
            self.swarm.dial(circuit_addr)?;
            let conn_type = self.wait_for_connection_and_classify().await?;
            self.stream_resolver.update_connection_type(conn_type);
        } else {
            anyhow::bail!("Failed to connect: no relay connection and no mDNS discovery");
        }

        // 根据连接方式选择码流
        let video_protocol = self.stream_resolver.resolve();
        let resolved_name = if video_protocol == stream_protocols::VIDEO_MAIN_PROTOCOL { "main" }
            else if video_protocol == stream_protocols::VIDEO_SUB_PROTOCOL { "sub" }
            else { "third" };
        tracing::info!(
            "[Viewer] Stream selected: {} (connection={:?}, stream_type={})",
            resolved_name,
            self.stream_resolver.connection_type,
            stream_type
        );

        let video_stream = self.stream_control
            .open_stream(device_cam, video_protocol)
            .await
            .context("Failed to open video stream")?;

        // 打开音频 stream
        let audio_stream = self.stream_control
            .open_stream(device_cam, stream_protocols::AUDIO_PROTOCOL)
            .await
            .context("Failed to open audio stream")?;

        println!("[Viewer] Video + Audio streams opened");

        // 启动接收任务
        let video_sender = self.video_sender.clone();
        let audio_sender = self.audio_sender.clone();

        let video_handle = tokio::spawn(Self::receive_frames(device_cam, video_stream, video_sender)).abort_handle();
        self.video_abort_handle = Some(video_handle);
        tokio::spawn(Self::receive_frames(device_cam, audio_stream, audio_sender));

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
                SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                    mdns::Event::Discovered(peers),
                )) => {
                    for (peer_id, addr) in peers {
                        tracing::info!("[Viewer] mDNS discovered peer {peer_id} at {addr}");
                    }
                }
                SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                    dcutr::Event { result: Ok(_), remote_peer_id, .. },
                )) => {
                    tracing::info!("[Viewer] DCUtR direct connection established with {remote_peer_id}");
                    tracing::info!("[Viewer] Direct Connection Upgrade successful - switching from relay to direct connection");
                    self.connection_quality.direct_upgraded = true;
                    self.connection_quality.connection_type = ConnectionType::QuicDirect;
                    self.connection_quality.last_dcutr_result = Some(Ok(()));
                }
                SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                    dcutr::Event { result: Err(err), remote_peer_id, .. },
                )) => {
                    let err_str = err.to_string();
                    tracing::warn!("[Viewer] DCUtR failed with {remote_peer_id}: {err}");
                    if err_str.contains("timeout") {
                        tracing::warn!("[Viewer] DCUtR failure cause: NAT type incompatibility or firewall blocking UDP");
                    } else if err_str.contains("IO error") || err_str.contains("connection refused") || err_str.contains("network unreachable") {
                        tracing::warn!("[Viewer] DCUtR failure cause: network unreachable or connection refused");
                    }
                    let diag = self.nat_diagnostic.diagnose();
                    tracing::warn!("[Viewer] Suggestion: {}", diag.dcutr_suggestion);
                    self.connection_quality.last_dcutr_result = Some(Err(err_str));
                    // 快速降级确认：DCUtR 失败后确认 Relay Circuit 仍在工作
                    if let Some(device_cam) = self.device_cam_peer_id {
                        if self.swarm.is_connected(&device_cam) {
                            tracing::info!("[Viewer] Fallback: Relay circuit is still active, video/audio will continue via relay");
                        } else {
                            tracing::warn!("[Viewer] Fallback: Relay circuit may be lost, connection may drop soon");
                        }
                    }
                }
                SwarmEvent::Behaviour(ViewerBehaviourEvent::Identify(
                    identify::Event::Received { info, peer_id: identify_peer_id, .. },
                )) => {
                    tracing::info!("[Viewer] Identify: observed_addr={}", info.observed_addr);
                    self.nat_diagnostic.record_observed(&info.observed_addr);
                    if let Some(Protocol::Ip4(ip)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                        if ip.is_private() {
                            tracing::warn!("[Viewer] WARNING: Observed address is private IP ({}) - DCUtR may fail!", ip);
                        } else {
                            tracing::info!("[Viewer] Observed address is public IP ({}) - good for DCUtR", ip);
                        }
                    }
                    if info.observed_addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                        tracing::info!("[Viewer] Observed address protocol: QUIC - good for DCUtR hole punching");
                    } else if info.observed_addr.iter().any(|p| matches!(p, Protocol::Tcp(_))) {
                        tracing::warn!("[Viewer] Observed address protocol: TCP only - DCUtR will produce TCP candidates, hole punching unlikely to succeed");
                    }
                    let diag = self.nat_diagnostic.diagnose();
                    tracing::info!("[Viewer] NAT diagnosis: {}", diag.nat_type.description());
                    if diag.is_4g {
                        tracing::info!("[Viewer] 4G/CGNAT network detected");
                    }
                    tracing::info!("[Viewer] DCUtR suggestion: {}", diag.dcutr_suggestion);

                    // 局域网直连检测：检查对端 listen_addrs 中是否有与本地 IP 同子网的 QUIC 地址
                    if let Some(device_cam) = self.device_cam_peer_id {
                        if identify_peer_id == device_cam
                            && !self.connection_quality.direct_upgraded
                            && !self.lan_direct_attempted
                        {
                            let local_ips = self.nat_diagnostic.local_ips();
                            let lan_addrs: Vec<Multiaddr> = info.listen_addrs.iter()
                                .filter(|a| a.iter().any(|p| matches!(p, Protocol::QuicV1)))
                                .filter(|a| !a.iter().any(|p| matches!(p, Protocol::P2pCircuit)))
                                .filter(|a| {
                                    if let Some(Protocol::Ip4(ip)) = a.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                                        ip.is_private() && !ip.is_loopback()
                                    } else {
                                        false
                                    }
                                })
                                .filter(|a| {
                                    if let Some(Protocol::Ip4(remote_ip)) = a.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                                        local_ips.iter().any(|local_ip| is_same_subnet(*local_ip, remote_ip))
                                    } else {
                                        false
                                    }
                                })
                                .cloned()
                                .collect();

                            if !lan_addrs.is_empty() {
                                self.lan_direct_attempted = true;
                                for addr in &lan_addrs {
                                    tracing::info!("[Viewer] LAN direct: detected same-subnet address {addr}, dialing...");
                                }
                                if let Err(e) = self.swarm.dial(lan_addrs[0].clone()) {
                                    tracing::warn!("[Viewer] LAN direct dial failed: {e}");
                                }
                            }
                        }
                    }
                }
                SwarmEvent::ConnectionEstablished { peer_id, endpoint, connection_id, .. } => {
                    let addr = endpoint.get_remote_address().clone();
                    self.connection_quality.active_connections += 1;

                    // 记录旧连接方式，用于判断是否需要码流切换
                    let old_conn_type = self.stream_resolver.connection_type;

                    if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                        self.connection_quality.connection_type = ConnectionType::RelayCircuit;
                        self.stream_resolver.update_connection_type(ConnectionType::RelayCircuit);
                        // 记录中继连接 ID，直连升级后关闭
                        self.relay_connection_id = Some(connection_id);

                        // DCUtR 尝试前预测：在 circuit 连接建立后立即输出 NAT 上下文
                        let prediction = self.nat_diagnostic.dcutr_prediction();
                        if prediction.likely_success {
                            tracing::info!("[Viewer] DCUtR prediction: likely SUCCESS - {}", prediction.reason);
                        } else {
                            tracing::warn!("[Viewer] DCUtR prediction: likely FAIL - {}", prediction.reason);
                        }
                    } else if addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                        let is_lan = addr.iter().any(|p| {
                            if let Protocol::Ip4(ip) = p { ip.is_private() } else { false }
                        });
                        if is_lan {
                            self.connection_quality.connection_type = ConnectionType::LanDirect;
                            self.connection_quality.direct_upgraded = true;
                            self.stream_resolver.update_connection_type(ConnectionType::LanDirect);
                            tracing::info!("[Viewer] LAN direct connection established with {peer_id}");
                        } else {
                            self.connection_quality.connection_type = ConnectionType::QuicDirect;
                            self.stream_resolver.update_connection_type(ConnectionType::QuicDirect);
                        }
                    }

                    // 直连升级码流切换：从转发升级到直连时，从子码流切换到主码流
                    if let Some(device_cam) = self.device_cam_peer_id {
                        if peer_id == device_cam && self.stream_resolver.should_upgrade_stream(old_conn_type) {
                            let main_protocol = self.stream_resolver.upgrade_protocol();
                            match self.stream_control.open_stream(device_cam, main_protocol).await {
                                Ok(new_stream) => {
                                    if let Some(h) = self.video_abort_handle.take() { h.abort(); }
                                    let handle = tokio::spawn(
                                        Self::receive_frames(device_cam, new_stream, self.video_sender.clone())
                                    ).abort_handle();
                                    self.video_abort_handle = Some(handle);
                                    tracing::info!("[Viewer] Stream upgraded: sub → main (direct connection established)");

                                    // 直连升级成功后，关闭中继连接
                                    // 防止 DCUtR 基于中继连接继续尝试打洞，导致视频卡顿
                                    if let Some(relay_conn_id) = self.relay_connection_id.take() {
                                        tracing::info!("[Viewer] Closing relay circuit connection after direct upgrade (prevents DCUtR interference)");
                                        self.swarm.close_connection(relay_conn_id);
                                    }
                                }
                                Err(e) => {
                                    tracing::warn!("[Viewer] Failed to open main stream on direct connection, staying on sub: {e}");
                                }
                            }
                        }
                    }

                    tracing::debug!("[Viewer] Connection established: {peer_id}, active={}", self.connection_quality.active_connections);
                }
                SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                    self.connection_quality.active_connections = num_established as usize;
                    if num_established == 0 {
                        tracing::warn!("[Viewer] All connections to {peer_id} closed");
                        self.connection_quality.connection_type = ConnectionType::Disconnected;
                        self.connection_quality.direct_upgraded = false;
                    } else {
                        tracing::info!("[Viewer] Connection to {peer_id} closed, {num_established} remaining");
                    }
                }
                _ => {
                    tracing::debug!("Viewer swarm event: {:?}", event);
                }
            }
        }
    }

    pub fn nat_diagnosis(&self) -> Option<NatDiagnosis> {
        if self.nat_diagnostic.observed_history_is_empty() {
            None
        } else {
            Some(self.nat_diagnostic.diagnose())
        }
    }

    pub fn connection_quality(&self) -> &ConnectionQuality {
        &self.connection_quality
    }

    // ---- 内部方法 ----

    /// 等待与 DeviceCam 的连接建立，并判断连接方式（转发/直连）
    ///
    /// 在 ConnectionEstablished 事件中分析 Multiaddr 判断连接方式，
    /// 返回 ConnectionType 用于码流自动选择。
    async fn wait_for_connection_and_classify(&mut self) -> Result<ConnectionType> {
        loop {
            match self.swarm.select_next_some().await {
                SwarmEvent::ConnectionEstablished { peer_id, endpoint, connection_id, .. } => {
                    let addr = endpoint.get_remote_address().clone();
                    println!("[Viewer] Connected to {peer_id}");

                    // 判断连接方式
                    let conn_type = if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                        // 记录中继连接 ID，直连升级后关闭
                        self.relay_connection_id = Some(connection_id);
                        ConnectionType::RelayCircuit
                    } else if addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                        let is_lan = addr.iter().any(|p| {
                            if let Protocol::Ip4(ip) = p { ip.is_private() } else { false }
                        });
                        if is_lan {
                            ConnectionType::LanDirect
                        } else {
                            ConnectionType::QuicDirect
                        }
                    } else {
                        ConnectionType::TcpDirect
                    };

                    tracing::info!("[Viewer] Connection type: {}", conn_type.description());
                    return Ok(conn_type);
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
    pub ping: ping::Behaviour,
    pub stream: libp2p_stream::Behaviour,
    pub mdns: mdns::tokio::Behaviour,
}

impl ViewerBehaviour {
    pub fn new(
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
                )
                .with_push_listen_addr_updates(true),
            ),
            ping: ping::Behaviour::new(
                ping::Config::new()
                    .with_interval(Duration::from_secs(5)),
            ),
            stream: libp2p_stream::Behaviour::new(),
            mdns: mdns::tokio::Behaviour::new(
                mdns::Config::default(),
                peer_id,
            )
            .expect("Failed to initialize mDNS"),
        }
    }

    pub fn new_with_identify_config(
        local_public_key: libp2p::identity::PublicKey,
        relay_client: relay::client::Behaviour,
        identify_config: identify::Config,
    ) -> Self {
        let peer_id = local_public_key.to_peer_id();
        Self {
            relay_client,
            dcutr: dcutr::Behaviour::new(peer_id),
            identify: identify::Behaviour::new(identify_config),
            ping: ping::Behaviour::new(
                ping::Config::new()
                    .with_interval(Duration::from_secs(5)),
            ),
            stream: libp2p_stream::Behaviour::new(),
            mdns: mdns::tokio::Behaviour::new(
                mdns::Config::default(),
                peer_id,
            )
            .expect("Failed to initialize mDNS"),
        }
    }
}

/// 检查两个 IPv4 地址是否在同一 /24 子网
fn is_same_subnet(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let a = u32::from(a);
    let b = u32::from(b);
    (a & 0xFFFFFF00) == (b & 0xFFFFFF00)
}
