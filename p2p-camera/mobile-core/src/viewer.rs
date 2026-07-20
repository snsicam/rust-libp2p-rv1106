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
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p_stream::{self, Control};
use proto::{
    media_packet::{MediaPacket, MediaTrack},
    stream_protocols,
};
use tokio::sync::mpsc;

use crate::jitter_buffer::AvJitterBuffer;
use crate::net_diag::{ConnectionQuality, ConnectionType, NatDiagnostic, NatDiagnosis, NatType};

pub const STREAM_READ_BUF: usize = 65536; // 64KB
pub const MDNS_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
pub const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// MediaPlayer 内部事件，用于通知上层（JNI bridge）连接状态变化
#[derive(Debug, Clone)]
pub enum MediaPlayerEvent {
    /// 连接已断开，reason 描述断开原因
    Disconnected { reason: String },
    /// 直连升级成功 (DCUtR 或 LAN direct)
    DirectUpgraded { via_lan: bool },
}

/// 将 stream 名称映射到对应的协议
pub fn get_video_protocol(stream_type: &str) -> StreamProtocol {
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
pub struct MediaPlayer {
    swarm: Swarm<ViewerBehaviour>,
    stream_control: Control,
    jitter: AvJitterBuffer,
    video_sender: mpsc::Sender<MediaPacket>,
    video_receiver: mpsc::Receiver<MediaPacket>,
    audio_sender: mpsc::Sender<MediaPacket>,
    audio_receiver: mpsc::Receiver<MediaPacket>,
    event_sender: mpsc::Sender<MediaPlayerEvent>,
    event_receiver: mpsc::Receiver<MediaPlayerEvent>,
    nat_diagnostic: NatDiagnostic,
    connection_quality: ConnectionQuality,
    device_cam_peer_id: Option<PeerId>,
    lan_direct_attempted: bool,
    stream_resolver: StreamTypeResolver,
    video_abort_handle: Option<tokio::task::AbortHandle>,
    audio_abort_handle: Option<tokio::task::AbortHandle>,
    relay_connection_id: Option<libp2p::swarm::ConnectionId>,
    /// 保存连接参数用于重连
    connect_params: Option<ConnectParams>,
    pub connected: bool,
    /// 对称型 NAT 检测标志：连接后由 net_diag 判定。
    /// 一旦确认为 Symmetric，重连时禁用 DCUtR（重建 Swarm）。
    symmetric_detected: bool,
    /// 保存密钥对，重连重建 Swarm 时复用，保证 PeerId 不变
    keypair: libp2p::identity::Keypair,
}

/// 保存连接参数，用于断连后自动重连
#[derive(Clone)]
struct ConnectParams {
    relay_addrs: Vec<String>,
    device_cam_peer_id: String,
    enable_mdns: bool,
    stream_type: String,
}

impl MediaPlayer {
    /// 创建新的 Viewer 实例
    ///
    /// `enable_dcutr`: 是否启用 DCUtR 直连打洞。
    /// 默认应传 `true`：锥形/EIM NAT（含多数 4G）可打洞成功，能省下中继带宽。
    /// 仅当本端确认为 **Symmetric NAT** 时才传 `false`（或保持默认 `true`，
    /// 由 `poll_swarm` 在连接后检测，重连时通过重建 Swarm 自动禁用 DCUtR）。
    /// 不再以粗粒度的 `4g` 标志禁用，避免误杀可打洞的锥形 4G。
    pub async fn new(enable_dcutr: bool) -> Result<Self> {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let swarm = Self::build_swarm(&keypair, enable_dcutr)?;
        let stream_control = swarm.behaviour().stream.new_control();

        let (video_sender, video_receiver) = mpsc::channel::<MediaPacket>(60);
        let (audio_sender, audio_receiver) = mpsc::channel::<MediaPacket>(200);
        let (event_sender, event_receiver) = mpsc::channel::<MediaPlayerEvent>(32);

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
            event_sender,
            event_receiver,
            nat_diagnostic: NatDiagnostic::new(0, Vec::new()),
            connection_quality: ConnectionQuality::default(),
            device_cam_peer_id: None,
            lan_direct_attempted: false,
            stream_resolver: StreamTypeResolver::new("auto"),
            video_abort_handle: None,
            audio_abort_handle: None,
            relay_connection_id: None,
            connect_params: None,
            connected: false,
            symmetric_detected: false,
            keypair,
        })
    }

    /// 构建 Swarm（含可选的 DCUtR 行为）
    ///
    /// `enable_dcutr = false` 时通过 `Toggle::from(None)` 禁用 DCUtR 行为，
    /// 用于本端确认为 Symmetric NAT 后的重连（避免每次重连都无效打洞 ~17s）。
    /// 复用传入的 `keypair` 以保证 PeerId 在重连前后一致。
    fn build_swarm(
        keypair: &libp2p::identity::Keypair,
        enable_dcutr: bool,
    ) -> Result<Swarm<ViewerBehaviour>> {
        let swarm = libp2p::SwarmBuilder::with_existing_identity(keypair.clone())
            .with_tokio()
            .with_tcp(
                tcp::Config::default().nodelay(true),
                noise::Config::new,
                libp2p::yamux::Config::default,
            )?
            .with_quic()
            .with_relay_client(noise::Config::new, libp2p::yamux::Config::default)?
            .with_behaviour(|key, relay_client| {
                ViewerBehaviour::new(key.public(), relay_client, enable_dcutr)
            })?
            // idle timeout 120s: DCUtR handler 在重试期间需要 keep-alive，0 会导致连接被意外关闭
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
            .build();
        Ok(swarm)
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

        // 保存连接参数用于重连
        self.connect_params = Some(ConnectParams {
            relay_addrs: relay_addrs.to_vec(),
            device_cam_peer_id: device_cam_peer_id.to_string(),
            enable_mdns,
            stream_type: stream_type.to_string(),
        });

        // 初始化码流选择器
        self.stream_resolver = StreamTypeResolver::new(stream_type);
        self.lan_direct_attempted = false;

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
        let event_sender = self.event_sender.clone();

        let video_handle = tokio::spawn(
            Self::receive_frames(device_cam, video_stream, video_sender, event_sender.clone())
        ).abort_handle();
        self.video_abort_handle = Some(video_handle);
        let audio_handle = tokio::spawn(
            Self::receive_frames(device_cam, audio_stream, audio_sender, event_sender)
        ).abort_handle();
        self.audio_abort_handle = Some(audio_handle);

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

    /// 获取下一个音频帧 (供 Native UI 层轮询)
    pub fn poll_audio_frame(&mut self) -> Option<MediaPacket> {
        // 先尝试从 Jitter Buffer 取
        if let Some(frame) = self.jitter.next_audio() {
            return Some(frame);
        }
        // 再尝试从接收 channel 取新包送入 jitter
        while let Ok(packet) = self.audio_receiver.try_recv() {
            self.jitter.push(packet);
        }
        self.jitter.next_audio()
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

                    // 对称型 NAT 检测：一旦确认，重连时禁用 DCUtR（重建 Swarm），
                    // 避免每次重连都进行 ~17s 的无效打洞。
                    if diag.nat_type == NatType::Symmetric && !self.symmetric_detected {
                        self.symmetric_detected = true;
                        tracing::warn!(
                            "[Viewer] Symmetric NAT confirmed → DCUtR will be DISABLED on next reconnect (Swarm rebuilt without dcutr)"
                        );
                    }
                    // 汇总日志：本端 NAT 类型 + 当前 DCUtR 实际状态
                    // 注意：symmetric_detected=true 只能说明"下次重连会禁用"，当前连接的 DCUtR
                    // 仍然活跃（因为 Swarm 创建时已确定）。另外检查当前 swarm 中 dcutr 是否真被禁用。
                    let dcutr_currently_enabled = self.swarm.behaviour().dcutr.is_enabled();
                    tracing::info!(
                        "[Viewer] === NAT type: {} ({}) | DCUtR currently {} | will skip on reconnect: {} ===",
                        diag.nat_type.short_name(),
                        if diag.is_4g { "4G/CGNAT" } else { "broadband" },
                        if dcutr_currently_enabled { "enabled" } else { "DISABLED" },
                        self.symmetric_detected
                    );

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
                        // 连接策略日志
                        let (strategy, reason) = self.nat_diagnostic.connection_strategy();
                        tracing::info!("[Viewer] Connection strategy: {} - {}", strategy.name(), reason);
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
                                        Self::receive_frames(device_cam, new_stream, self.video_sender.clone(), self.event_sender.clone())
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
                        self.connected = false;

                        // 通知上层断连
                        let is_device_cam = self.device_cam_peer_id == Some(peer_id);
                        let reason = if is_device_cam {
                            "DeviceCam connection closed".to_string()
                        } else {
                            format!("Connection to {peer_id} closed")
                        };
                        let _ = self.event_sender.send(MediaPlayerEvent::Disconnected {
                            reason,
                        }).await;
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

    /// 设置 4G 网络标志（4G 模块的 IP 可能是 RFC1918 私有地址，无法通过 IP 启发式检测）
    pub fn set_force_4g(&mut self, force: bool) {
        self.nat_diagnostic.set_force_4g(force);
    }

    /// 轮询内部事件（非阻塞），供上层检测断连/直连升级等
    pub fn poll_event(&mut self) -> Option<MediaPlayerEvent> {
        self.event_receiver.try_recv().ok()
    }

    /// 自动重连：使用保存的连接参数重新连接
    ///
    /// 不需要重建 Swarm，底层 transport 仍然可用。
    /// 重新拨号 Relay → Circuit → 打开 stream。
    pub async fn reconnect(&mut self) -> Result<()> {
        let params = self.connect_params.clone()
            .ok_or_else(|| anyhow::anyhow!("No connect params saved, cannot reconnect"))?;

        tracing::info!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
        tokio::time::sleep(RECONNECT_DELAY).await;

        // 若已确认 Symmetric NAT，重建 Swarm 并禁用 DCUtR，
        // 避免每次重连都进行 ~17s 的无效打洞（Toggle 无法运行时切换，只能重建）。
        if self.symmetric_detected {
            tracing::info!("[Viewer] Rebuilding Swarm with DCUtR DISABLED (Symmetric NAT confirmed)");
            self.swarm = Self::build_swarm(&self.keypair, false)?;
            self.stream_control = self.swarm.behaviour().stream.new_control();
            // 重置 NAT 诊断状态：新 Swarm 需要重新通过 Identify 观测地址
            self.nat_diagnostic = NatDiagnostic::new(0, Vec::new());
        }

        // 停止旧的接收任务
        if let Some(h) = self.video_abort_handle.take() { h.abort(); }
        if let Some(h) = self.audio_abort_handle.take() { h.abort(); }

        // 清空 jitter buffer 中的旧数据
        self.jitter.clear();

        self.connect(
            &params.relay_addrs,
            &params.device_cam_peer_id,
            params.enable_mdns,
            &params.stream_type,
        ).await
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

    /// 等待首个关键帧的最长时间（最后安全网）。正常情况下门控由 `is_nal_keyframe` 扫描
    /// 字节流里的真实 IDR NAL 立即开门（cam 在 `request_idr` 后很快产出真 IDR），不会等到这里。
    /// 仅当字节扫描也异常时才兜底强制开门，避免 `!got_first_idr` 门控永久不开 → 黑屏。
    /// 3s 足够覆盖一个 GOP 周期（gop=50@25fps≈2s），正常绝不触发。
    const IDR_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

    /// 从 Annex B 裸流扫描 NAL，判断是否为关键帧（IRAP）。
    /// H.265: IRAP NAL type 16..=21 (BLA/IDR/CRA)；H.264: IDR NAL type 5。
    /// 与 C 侧 `rk_camera.c` 的 `is_keyframe_h265/h264` 判定范围一致，但在 viewer 侧**独立扫描
    /// 字节**，不依赖对端 `is_keyframe()` 标志位。日志实证：cam 发的 IDR 数据字节里确有 IRAP
    /// NAL（ffmpeg 能据此恢复解码），但 `is_keyframe()` 标志常不可靠（8s 内检测不到），
    /// 故必须用本函数扫字节兜底——这是之前版本快速起播的关键，删掉它改用 `is_keyframe()`
    /// 正是起播变慢（卡满超时）的根因。
    fn is_nal_keyframe(data: &[u8]) -> bool {
        let len = data.len();
        let mut i = 0;
        while i + 4 < len {
            let hdr_off = if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 0 && data[i + 3] == 1 {
                i + 4
            } else if data[i] == 0 && data[i + 1] == 0 && data[i + 2] == 1 {
                i + 3
            } else {
                i += 1;
                continue;
            };
            let b = data[hdr_off];
            let h265_type = (b >> 1) & 0x3F;
            if (16..=21).contains(&h265_type) {
                return true;
            }
            let h264_type = b & 0x1F;
            if h264_type == 5 {
                return true;
            }
            i = hdr_off + 1;
        }
        false
    }

    /// 从 stream 持续读取帧，送入 channel
    /// EOF 或读错误时通过 event_sender 发送 Disconnected 通知
    async fn receive_frames(
        peer_id: PeerId,
        mut stream: libp2p::swarm::Stream,
        sender: mpsc::Sender<MediaPacket>,
        event_sender: mpsc::Sender<MediaPlayerEvent>,
    ) {
        let mut buf = bytes::BytesMut::with_capacity(STREAM_READ_BUF);
        let mut read_buf = vec![0u8; STREAM_READ_BUF];
        // HEVC 初始帧容错：丢弃首个 IDR 前的非关键帧，避免解码器缺少参考帧导致
        // "Player error" (Android MediaCodec / ffmpeg 均无法从无参考的 P/B 帧开始解码)。
        // 收到第一个关键帧后恢复正常转发。
        let mut got_first_idr = false;
        // 收到首个视频包的时间，用于 IDR 等待超时兜底
        let mut first_video_at: Option<std::time::Instant> = None;

        loop {
            match stream.read(&mut read_buf).await {
                Ok(0) => {
                    tracing::warn!("[Viewer] Stream EOF from {peer_id}");
                    let _ = event_sender.send(MediaPlayerEvent::Disconnected {
                        reason: format!("Stream EOF from {peer_id}"),
                    }).await;
                    break;
                }
                Ok(n) => {
                    buf.extend_from_slice(&read_buf[..n]);

                    // 尝试解码所有完整的包
                    while let Some(packet) = MediaPacket::try_decode(&mut buf) {
                        // 仅对视频流做 IDR 容错；音频始终透传。
                        // 关键帧判定: 仅用 viewer 自身字节扫描 `Self::is_nal_keyframe`
                        // (H.265 IRAP 16-21 / H.264 IDR 5), receiver 自包含、不信任对端 flag。
                        // cam 侧已不再计算 keyframe 标志(见 rk_video_source.rs), JNI 桥也不转发该 flag,
                        // 故此处为唯一权威判定。收到首个 IDR 前的非关键帧直接丢弃(解码器无参考帧会报错),
                        // 收到首个 IDR 后恢复正常转发。
                        if packet.track == MediaTrack::Video && !got_first_idr {
                            if first_video_at.is_none() {
                                first_video_at = Some(std::time::Instant::now());
                            }
                            let is_kf = Self::is_nal_keyframe(&packet.data);
                            let waited = first_video_at.map(|t| t.elapsed());
                            if is_kf || waited.map_or(false, |w| w >= Self::IDR_WAIT_TIMEOUT) {
                                got_first_idr = true;
                                if is_kf {
                                    tracing::info!(
                                        "[Viewer] First IDR received, video decode started (dropped pre-IDR non-keyframes)"
                                    );
                                } else {
                                    tracing::warn!(
                                        "[Viewer] No IDR within {:?}, forwarding video anyway (cam keyframe flag may be unreliable)",
                                        Self::IDR_WAIT_TIMEOUT
                                    );
                                }
                            } else {
                                // 首个 IDR 前的非关键帧：解码器无参考帧，直接丢弃
                                tracing::debug!(
                                    "[Viewer] Drop pre-IDR non-keyframe ({} bytes) to avoid decode error",
                                    packet.data.len()
                                );
                                continue;
                            }
                        }
                        if sender.send(packet).await.is_err() {
                            break; // 接收端已关闭
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!("Stream read error from {peer_id}: {e}");
                    let _ = event_sender.send(MediaPlayerEvent::Disconnected {
                        reason: format!("Stream read error from {peer_id}: {e}"),
                    }).await;
                    break;
                }
            }
        }
    }
}

#[derive(NetworkBehaviour)]
pub struct ViewerBehaviour {
    pub relay_client: relay::client::Behaviour,
    /// DCUtR 直连打洞行为。4G/CGNAT 网络下打洞必然失败，用 Toggle 禁用以避免
    /// 约 17 秒的无谓超时等待和转发链路上的写阻塞。
    pub dcutr: Toggle<dcutr::Behaviour>,
    pub identify: identify::Behaviour,
    pub ping: ping::Behaviour,
    pub stream: libp2p_stream::Behaviour,
    pub mdns: mdns::tokio::Behaviour,
}

impl ViewerBehaviour {
    pub fn new(
        local_public_key: libp2p::identity::PublicKey,
        relay_client: relay::client::Behaviour,
        enable_dcutr: bool,
    ) -> Self {
        let peer_id = local_public_key.to_peer_id();
        Self {
            relay_client,
            dcutr: Toggle::from(if enable_dcutr {
                Some(dcutr::Behaviour::new(peer_id))
            } else {
                None
            }),
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
        enable_dcutr: bool,
    ) -> Self {
        let peer_id = local_public_key.to_peer_id();
        Self {
            relay_client,
            dcutr: Toggle::from(if enable_dcutr {
                Some(dcutr::Behaviour::new(peer_id))
            } else {
                None
            }),
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
pub fn is_same_subnet(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let a = u32::from(a);
    let b = u32::from(b);
    (a & 0xFFFFFF00) == (b & 0xFFFFFF00)
}
