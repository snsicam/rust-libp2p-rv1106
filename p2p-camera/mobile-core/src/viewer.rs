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
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, identify, mdns, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, StreamProtocol, Swarm, PeerId, identity,
};
use libp2p::swarm::behaviour::toggle::Toggle;
use libp2p_stream::{self, Control};
use proto::{
    media_packet::{MediaPacket, MediaTrack, FLAG_VIDEO_KEYFRAME},
    registry::{RegistryMessage, REGISTRY_PROTOCOL},
    stream_protocols,
};
use tokio::sync::mpsc;

use crate::net_diag::{ConnectionQuality, ConnectionType, NatDiagnostic, NatDiagnosis, NatType};

pub const STREAM_READ_BUF: usize = 65536; // 64KB
pub const MDNS_DISCOVERY_TIMEOUT: Duration = Duration::from_secs(5);
pub const RECONNECT_DELAY: Duration = Duration::from_secs(3);

/// 等待首个关键帧的最长时间（最后安全网）。正常情况下门控由 `is_nal_keyframe` 扫描
/// 字节流里的真实 IDR NAL 立即开门（cam 在 `request_idr` 后很快产出真 IDR），不会等到这里。
/// 仅当字节扫描也异常时才兜底强制开门，避免 `!got_first_idr` 门控永久不开 → 黑屏。
/// 3s 足够覆盖一个 GOP 周期（gop=50@25fps≈2s），正常绝不触发。
// 最后安全网: 仅当字节扫描(is_nal_keyframe)也异常时才兜底强制开门。
// 正常情况下 cam 在 request_idr 后很快产出真 IDR(配合短 GOP=15 约 0.75s 即到),
// 本超时不会触发。设为 5s 以覆盖设备仍跑旧 gop=40(自然 IDR 实测~4.7s)的情况,
// 避免超时抢在真实 IDR 之前触发 → 解码器被迫解码无参考 P 帧 → 马赛克。
pub const IDR_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);

/// 从 Annex B 裸流扫描 NAL，判断是否为关键帧（IRAP）。
/// H.265: IRAP NAL type 16..=21 (BLA/IDR/CRA)；H.264: IDR NAL type 5。
/// 与 C 侧 `rk_camera.c` 的 `is_keyframe_h265/h264` 判定范围一致，但在 viewer 侧**独立扫描
/// 字节**，不依赖对端 `is_keyframe()` 标志位。日志实证：cam 发的 IDR 数据字节里确有 IRAP
/// NAL（ffmpeg 能据此恢复解码），但 `is_keyframe()` 标志常不可靠（8s 内检测不到），
/// 故必须用本函数扫字节兜底——这是之前版本快速起播的关键，删掉它改用 `is_keyframe()`
/// 正是起播变慢（卡满超时）的根因。
pub fn is_nal_keyframe(data: &[u8]) -> bool {
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

/// MediaPlayer 内部事件，用于通知上层（JNI bridge）连接状态变化
#[derive(Debug, Clone)]
pub enum MediaPlayerEvent {
    /// 连接已断开，reason 描述断开原因
    Disconnected { reason: String },
    /// 直连升级成功 (DCUtR 或 LAN direct)
    DirectUpgraded { via_lan: bool },
    /// 码流 EOF（连接可能仍存活），窗口恢复时触发重连
    StreamEOF { reason: String },
    /// NAT 类型诊断更新
    NatDiagnosis { local_nat: NatType, remote_nat: Option<String> },
    /// DCUtR 打洞反复失败，通知上层在后续重连中禁用 DCUtR
    DcutrBackoff,
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

/// MediaPlayer::connect 的可选参数
#[derive(Clone)]
pub struct ConnectOptions {
    /// 本地 QUIC 监听端口 (0 = 随机)
    pub udp_port: u16,
    /// 网络类型 ("wifi" | "4g")，影响 NAT 诊断和 DCUtR 策略
    pub network_type: String,
    /// 是否禁用音频流
    pub no_audio: bool,
}

impl Default for ConnectOptions {
    fn default() -> Self {
        Self {
            udp_port: 0,
            network_type: "wifi".to_string(),
            no_audio: false,
        }
    }
}

/// P2P Viewer — 对外暴露的核心结构
pub struct MediaPlayer {
    swarm: Swarm<ViewerBehaviour>,
    stream_control: Control,
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
    /// DCUtR 打洞失败累计
    dcutr_fail_count: u32,
    /// 本地 IP 列表（从 NewListenAddr 收集）
    local_ips: Vec<Ipv4Addr>,
    /// 本地 QUIC 监听端口
    local_quic_port: u16,
    /// 对端 NAT 类型推断
    remote_nat_hint: Option<String>,
    /// 网络类型 (wifi/4g)
    network_type: String,
    /// 是否禁用音频
    no_audio: bool,
    /// UDP 监听端口
    udp_port: u16,
    /// mDNS 发现的 device-cam 地址缓存（key = PeerId, value = 最新的非 relay Multiaddr）
    mdns_cache: std::collections::HashMap<libp2p::PeerId, Multiaddr>,
    /// 控制通道流 (用于发送控制请求)
    control_stream: Option<libp2p::Stream>,
    /// 当前已连接的 relay peer (用于 serial→peer_id 解析查询)
    connected_relay: Option<PeerId>,
    /// 本地静态 serial→peer_id 映射 (来自配置 serial_map)。
    /// 命中时无需连接 Relay 即可解析 SN，适合局域网 / Relay 不可达场景。
    serial_map: std::collections::HashMap<String, String>,
}

/// 保存连接参数，用于断连后自动重连
#[derive(Clone)]
struct ConnectParams {
    relay_addrs: Vec<String>,
    device_cam_peer_id: String,
    enable_mdns: bool,
    stream_type: String,
    no_audio: bool,
    serial_map: std::collections::HashMap<String, String>,
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
        Self::new_with_options(enable_dcutr, ConnectOptions::default()).await
    }

    /// 带选项的构造函数
    pub async fn new_with_options(enable_dcutr: bool, options: ConnectOptions) -> Result<Self> {
        let keypair = libp2p::identity::Keypair::generate_ed25519();
        let swarm = Self::build_swarm(&keypair, enable_dcutr)?;
        let stream_control = swarm.behaviour().stream.new_control();

        let (video_sender, video_receiver) = mpsc::channel::<MediaPacket>(60);
        let (audio_sender, audio_receiver) = mpsc::channel::<MediaPacket>(200);
        let (event_sender, event_receiver) = mpsc::channel::<MediaPlayerEvent>(32);

        let mut player = Self {
            swarm,
            stream_control,
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
            dcutr_fail_count: 0,
            local_ips: Vec::new(),
            local_quic_port: 0,
            remote_nat_hint: None,
            network_type: options.network_type.clone(),
            no_audio: options.no_audio,
            udp_port: options.udp_port,
            mdns_cache: std::collections::HashMap::new(),
            control_stream: None,
            connected_relay: None,
            serial_map: std::collections::HashMap::new(),
        };

        // 监听本地 QUIC (指定端口或随机)
        let udp_port = options.udp_port;
        let udp_addr = format!("/ip4/0.0.0.0/udp/{}/quic-v1", udp_port).parse()
            .context("Invalid local QUIC listen addr")?;
        player.swarm.listen_on(udp_addr)?;
        player.swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()
            .context("Invalid local TCP listen addr")?)?;
        println!("[Viewer] Listening on QUIC (port {}) and TCP",
            if udp_port != 0 { udp_port.to_string() } else { "random".to_string() });

        if options.network_type == "4g" {
            player.nat_diagnostic.set_force_4g(true);
            tracing::info!("[Viewer] Network type: 4G (forced by config)");
        }

        Ok(player)
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
            // QUIC max_idle_timeout 默认 10s 过短, 与 device-cam / relay-server 保持一致 (30s)。
            // QUIC 实际生效值取两端协商的较小者, 因此三端必须同步调整。
            .with_quic_config(|mut c| {
                c.max_idle_timeout = 30_000;
                c.keep_alive_interval = Duration::from_secs(3);
                c
            })
            .with_relay_client(noise::Config::new, libp2p::yamux::Config::default)?
            .with_behaviour(|key, relay_client| {
                ViewerBehaviour::new(key.public(), relay_client, enable_dcutr)
            })?
            // swarm idle timeout: 10min。QUIC keep-alive(3s) 已负责 NAT 保活,
            // 此处只防"真正死连接永久挂着"; 不能设 0 (DCUtR 重试期间需要 keep-alive, 0 会误关)。
            // 原 120s 会把"无子协议活跃"的 relay 中转连接误杀 → 每 ~2min 整轮重连 → 波及 LAN 直连。
            .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(600)))
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
        serial_map: &std::collections::HashMap<String, String>,
        stream_type: &str,
    ) -> Result<()> {
        self.connect_with_options(
            relay_addrs,
            device_cam_peer_id,
            enable_mdns,
            serial_map,
            stream_type,
            ConnectOptions::default(),
        ).await
    }

    /// 连接（带选项）
    pub async fn connect_with_options(
        &mut self,
        relay_addrs: &[String],
        device_cam_peer_id: &str,
        enable_mdns: bool,
        serial_map: &std::collections::HashMap<String, String>,
        stream_type: &str,
        options: ConnectOptions,
    ) -> Result<()> {
        // 判定输入是完整 PeerId 还是短序列号 (serial)
        // serial 形如树莓派 /proc/cpuinfo 的 Serial (如 e33700a6620dfddc)，
        // viewer 可仅凭它经 relay 注册表解析出真实 PeerId 再连接，无需手抄长串。
        // 预解析目标 peer：完整 PeerId 直连，或经本地 serial_map 提前解析。
        // 提前解析使 mDNS 循环期间即可直接匹配目标，避免等待整个 mDNS/relay 阶段结束。
        let known_device_cam: Option<PeerId> = if let Ok(pid) = device_cam_peer_id.parse::<PeerId>() {
            Some(pid)
        } else {
            match serial_map.get(device_cam_peer_id) {
                Some(pid_str) => match pid_str.parse::<PeerId>() {
                    Ok(pid) => {
                        println!("[Viewer] Resolved serial {} -> peer {} via local serial_map", device_cam_peer_id, pid);
                        Some(pid)
                    }
                    Err(_) => {
                        anyhow::bail!(
                            "serial_map 中 '{}' 的值 '{}' 不是合法 PeerId，请检查配置",
                            device_cam_peer_id, pid_str
                        );
                    }
                },
                None => None,
            }
        };
        if known_device_cam.is_none() {
            println!(
                "[Viewer] Input '{}' is not a PeerId — treating as serial, will resolve via relay registry",
                device_cam_peer_id
            );
        }

        // 保存连接参数用于重连
        self.serial_map = serial_map.clone();
        self.connect_params = Some(ConnectParams {
            relay_addrs: relay_addrs.to_vec(),
            device_cam_peer_id: device_cam_peer_id.to_string(),
            enable_mdns,
            stream_type: stream_type.to_string(),
            no_audio: options.no_audio,
            serial_map: serial_map.clone(),
        });

        // 保存选项
        self.no_audio = options.no_audio;
        self.network_type = options.network_type.clone();
        self.udp_port = options.udp_port;

        // 初始化码流选择器
        self.stream_resolver = StreamTypeResolver::new(stream_type);
        self.lan_direct_attempted = false;
        self.dcutr_fail_count = 0;

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

        // 优先使用 mDNS 缓存中已有的 device-cam 地址（重连时缓存可能已有）
        if enable_mdns {
            if let Some(dc) = known_device_cam {
                if let Some(cached_addr) = self.mdns_cache.get(&dc) {
                    tracing::info!("[Viewer] Using cached mDNS address for {dc}: {cached_addr}");
                    mdns_discovered_addr = Some(cached_addr.clone());
                }
            }
        }

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
                                self.connected_relay = Some(peer_id);
                            }
                        }
                        SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                            mdns::Event::Discovered(peers),
                        )) => {
                            for (peer_id, addr) in peers {
                                tracing::info!("[Viewer] mDNS discovered peer {peer_id} at {addr}");
                                // 更新 mDNS 缓存
                                let is_relay = addr.iter().any(|p| matches!(p, Protocol::P2pCircuit));
                                if !is_relay {
                                    let is_quic = addr.iter().any(|p| matches!(p, Protocol::QuicV1));
                                    let entry = self.mdns_cache.entry(peer_id);
                                    if let std::collections::hash_map::Entry::Occupied(mut e) = entry {
                                        let existing_is_quic = e.get().iter().any(|p| matches!(p, Protocol::QuicV1));
                                        if is_quic || !existing_is_quic {
                                            e.insert(addr.clone());
                                        }
                                    } else {
                                        entry.or_insert(addr.clone());
                                    }
                                }
                                if let Some(dc) = known_device_cam {
                                    if peer_id == dc && mdns_discovered_addr.is_none() {
                                        tracing::info!("[Viewer] mDNS found target DeviceCam {peer_id} at {addr}");
                                        mdns_discovered_addr = Some(addr);
                                    }
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

        // known_device_cam 已在连接前预解析完成（完整 PeerId 直连 / serial_map 命中）；
        // 此处仅处理需经 relay registry 解析的剩余场景。
        let connected_relay_peer = connected_relay.as_ref().map(|(p, _)| *p);
        let device_cam = if let Some(pid) = known_device_cam {
            pid
        } else {
            let relay_pid = connected_relay_peer.ok_or_else(|| {
                anyhow::anyhow!(
                    "无法解析序列号 '{}'：未连接到任何 Relay（请检查 relays 配置或补充 serial_map）",
                    device_cam_peer_id
                )
            })?;
            println!("[Viewer] Resolving serial {} via relay {} ...", device_cam_peer_id, relay_pid);
            let (pid, _pk) = self.query_registry(relay_pid, device_cam_peer_id).await?;
            println!("[Viewer] Resolved serial {} -> peer {}", device_cam_peer_id, pid);
            pid
        };
        self.device_cam_peer_id = Some(device_cam);
        self.connected_relay = connected_relay_peer;

        // serial/relay 注册表场景下，mDNS 循环期间 known_device_cam 尚为空，
        // 未能在 Discovered 事件中直接匹配目标；但 mdns_cache 已缓存该 peer 的 LAN 地址，这里回退使用。
        if mdns_discovered_addr.is_none() && enable_mdns {
            if let Some(cached_addr) = self.mdns_cache.get(&device_cam) {
                tracing::info!("[Viewer] Using cached mDNS address for {device_cam}: {cached_addr}");
                mdns_discovered_addr = Some(cached_addr.clone());
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

        // 启动视频接收任务（必须在 audio 之前！）
        let video_sender = self.video_sender.clone();
        let event_sender = self.event_sender.clone();

        let video_handle = tokio::spawn(
            Self::receive_frames(device_cam, video_stream, video_sender, event_sender.clone())
        ).abort_handle();
        self.video_abort_handle = Some(video_handle);

        // 打开音频 stream（可选）
        if !self.no_audio {
            match self.stream_control.open_stream(device_cam, stream_protocols::AUDIO_PROTOCOL).await {
                Ok(audio_stream) => {
                    println!("[Viewer] Audio stream opened");
                    let audio_sender = self.audio_sender.clone();
                    let audio_handle = tokio::spawn(
                        Self::receive_frames(device_cam, audio_stream, audio_sender, event_sender)
                    ).abort_handle();
                    self.audio_abort_handle = Some(audio_handle);
                }
                Err(e) => {
                    println!("[Viewer] Audio stream open failed (non-fatal): {e}");
                }
            }
        }

        // 打开控制通道 stream（可选，失败不阻塞）
        match self.stream_control.open_stream(device_cam, stream_protocols::CONTROL_PROTOCOL).await {
            Ok(control_stream) => {
                println!("[Viewer] Control stream opened");
                self.control_stream = Some(control_stream);
            }
            Err(e) => {
                println!("[Viewer] Control stream open failed (non-fatal): {e}");
            }
        }

        println!("[Viewer] Video stream opened (stream={})", resolved_name);

        self.connected = true;
        Ok(())
    }

    /// 经 relay 注册表把短序列号解析成真实 PeerId。
    ///
    /// 同时用返回的公钥验签，确认 `(serial → peer_id)` 绑定真实无误
    /// (防止中间人返回伪造的 peer_id)。仅当输入是 serial 时才需调用。
    pub async fn query_registry(
        &mut self,
        relay_peer: PeerId,
        serial: &str,
    ) -> Result<(PeerId, identity::PublicKey)> {
        let mut stream = self
            .stream_control
            .open_stream(relay_peer, REGISTRY_PROTOCOL)
            .await
            .map_err(|e| anyhow::anyhow!("open registry stream to relay: {e}"))?;
        let msg = RegistryMessage::Query {
            serial: serial.to_string(),
        };
        stream.write_all(&msg.encode()).await?;
        stream.flush().await?;

        // 读取 relay 响应 (注册表消息很小，单次或少量读取即可)
        let mut buf: Vec<u8> = Vec::with_capacity(256);
        let mut tmp = [0u8; 1024];
        loop {
            let n = stream.read(&mut tmp).await?;
            if n == 0 {
                break;
            }
            buf.extend_from_slice(&tmp[..n]);
            if RegistryMessage::decode(&buf).is_ok() {
                break;
            }
            if buf.len() >= 4096 {
                break;
            }
        }
        if buf.is_empty() {
            anyhow::bail!("registry stream closed without response");
        }
        let resp = RegistryMessage::decode(&buf)?;
        match resp {
            RegistryMessage::Response {
                peer_id,
                pubkey,
                signature,
            } => {
                let claimed = PeerId::from_bytes(&peer_id)
                    .map_err(|e| anyhow::anyhow!("invalid peer_id from registry: {e}"))?;
                let pk = identity::PublicKey::try_decode_protobuf(&pubkey)
                    .map_err(|e| anyhow::anyhow!("invalid pubkey from registry: {e}"))?;
                if pk.to_peer_id() != claimed {
                    anyhow::bail!("registry: pubkey/peer_id 不匹配");
                }
                let payload = RegistryMessage::sign_payload(serial, &peer_id);
                if !pk.verify(&payload, &signature) {
                    anyhow::bail!("registry: 签名无效，绑定可能被篡改");
                }
                Ok((claimed, pk))
            }
            RegistryMessage::NotFound => {
                anyhow::bail!("serial '{serial}' 未在 relay 注册表中找到 (相机是否已上线？)")
            }
            RegistryMessage::Error { message } => anyhow::bail!("relay registry 错误: {message}"),
            _ => anyhow::bail!("registry: 意外的响应类型"),
        }
    }

    /// 获取下一个视频帧 (供 Native UI 层轮询)
    /// 直接从 channel 取帧，不经过 Jitter Buffer。
    /// 实时监控场景对延迟敏感，Jitter Buffer 的缓冲和音视频同步
    /// 会引入额外延迟和帧丢弃，导致马赛克。
    pub fn poll_video_frame(&mut self) -> Option<MediaPacket> {
        self.video_receiver.try_recv().ok()
    }

    /// 获取下一个音频帧 (供 Native UI 层轮询)
    pub fn poll_audio_frame(&mut self) -> Option<MediaPacket> {
        self.audio_receiver.try_recv().ok()
    }

    /// 驱动 Swarm 事件循环 (需要定期调用)
    pub async fn poll_swarm(&mut self) {
        // 加超时避免长时间阻塞导致帧积压：Swarm 事件通常很频繁，
        // 但在连接空闲时可能长时间无事件，此时应返回让调用方处理帧
        let event = tokio::select! {
            event = self.swarm.next() => event,
            _ = tokio::time::sleep(std::time::Duration::from_millis(5)) => return,
        };
        if let Some(event) = event {
            match event {
                SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                    mdns::Event::Discovered(peers),
                )) => {
                    for (peer_id, addr) in peers {
                        tracing::info!("[Viewer] mDNS discovered peer {peer_id} at {addr}");
                        // 缓存非 relay 的 mDNS 地址（优先 QUIC）
                        let is_relay = addr.iter().any(|p| matches!(p, Protocol::P2pCircuit));
                        if !is_relay {
                            let is_quic = addr.iter().any(|p| matches!(p, Protocol::QuicV1));
                            let entry = self.mdns_cache.entry(peer_id);
                            if let std::collections::hash_map::Entry::Occupied(mut e) = entry {
                                // 优先保留 QUIC 地址
                                let existing_is_quic = e.get().iter().any(|p| matches!(p, Protocol::QuicV1));
                                if is_quic || !existing_is_quic {
                                    e.insert(addr);
                                }
                            } else {
                                entry.or_insert(addr);
                            }
                        }
                    }
                }
                SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                    mdns::Event::Expired(peers),
                )) => {
                    for (peer_id, _addr) in peers {
                        tracing::debug!("[Viewer] mDNS peer expired: {peer_id}");
                        // 注意：不立即从缓存移除，因为 Expired 可能只是某个地址过期
                        // 缓存会在下次 Discovered 时更新
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
                    let local_nat = self.nat_diagnostic.diagnose().nat_type.short_name().to_string();
                    let remote_nat = self.remote_nat_hint.clone().unwrap_or_else(|| "Unknown".to_string());
                    tracing::warn!("[Viewer] DCUtR hole punch FAILED with {remote_peer_id}: {err}");
                    tracing::warn!("[Viewer] NAT context: local={}, remote={}", local_nat, remote_nat);
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
                    // 失败退避：累计打洞失败，达到阈值则通知上层下次重连禁用 DCUtR
                    self.dcutr_fail_count += 1;
                    if self.dcutr_fail_count >= 2 {
                        tracing::warn!(
                            "[Viewer] DCUtR 已失败 {} 次，对本端无效（如 4G/CGNAT 入站 UDP 被屏蔽）。\
                             将通知上层在下次重连禁用 DCUtR，仅走中继电路，避免无效打洞干扰视频流。",
                            self.dcutr_fail_count
                        );
                        let _ = self.event_sender.send(MediaPlayerEvent::DcutrBackoff).await;
                    }
                }
                SwarmEvent::Behaviour(ViewerBehaviourEvent::Identify(
                    identify::Event::Received { info, peer_id: identify_peer_id, .. },
                )) => {
                    tracing::info!("[Viewer] Identify: observed_addr={}, listen_addrs={}",
                        info.observed_addr,
                        info.listen_addrs.len());

                    // NAT 诊断：记录观测地址
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
                        // NAT 端口映射检测
                        if let Some(Protocol::Udp(observed_port)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                            let local_port = self.udp_port;
                            if local_port != 0 && observed_port != local_port {
                                tracing::warn!("[Viewer] NAT port mapping detected: local UDP port {} → observed UDP port {}", local_port, observed_port);
                                tracing::warn!("[Viewer] This may indicate symmetric NAT, which prevents DCUtR hole-punching");
                                tracing::info!("[Viewer] Consider configuring port forwarding: router maps external UDP {} → internal UDP {}", observed_port, local_port);
                            } else if local_port != 0 && observed_port == local_port {
                                tracing::info!("[Viewer] Observed UDP port {} matches local QUIC port - good for DCUtR", observed_port);
                            }
                        }
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
                    let dcutr_currently_enabled = self.swarm.behaviour().dcutr.is_enabled();
                    tracing::info!(
                        "[Viewer] === NAT type: {} ({}) | DCUtR currently {} | will skip on reconnect: {} ===",
                        diag.nat_type.short_name(),
                        if diag.is_4g { "4G/CGNAT" } else { "broadband" },
                        if dcutr_currently_enabled { "enabled" } else { "DISABLED" },
                        self.symmetric_detected
                    );

                    // 发送 NAT 诊断事件
                    let _ = self.event_sender.send(MediaPlayerEvent::NatDiagnosis {
                        local_nat: diag.nat_type,
                        remote_nat: self.remote_nat_hint.clone(),
                    }).await;

                    // 对端 NAT 类型推断 + 局域网直连检测（仅对 device-cam 的 Identify 事件）
                    if let Some(device_cam) = self.device_cam_peer_id {
                        if identify_peer_id == device_cam {
                            // 对端 NAT 类型推断
                            if let Some(Protocol::Udp(observed_port)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                                let remote_quic_port = info.listen_addrs.iter()
                                    .filter(|a| a.iter().any(|p| matches!(p, Protocol::QuicV1)))
                                    .filter_map(|a| a.iter().find(|p| matches!(p, Protocol::Udp(_))))
                                    .find_map(|p| if let Protocol::Udp(port) = p { Some(port) } else { None });

                                if let Some(remote_port) = remote_quic_port {
                                    if observed_port == remote_port {
                                        self.remote_nat_hint = Some("Cone".to_string());
                                        tracing::info!("[Viewer] Remote peer NAT hint: Cone (observed port {} matches listen port {})", observed_port, remote_port);
                                    } else {
                                        self.remote_nat_hint = Some("Symmetric?".to_string());
                                        tracing::warn!("[Viewer] Remote peer NAT hint: possibly Symmetric (observed port {} != listen port {})", observed_port, remote_port);
                                    }
                                }
                            }

                            // 局域网直连检测
                            if !self.connection_quality.direct_upgraded && !self.lan_direct_attempted {
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
                                        // 本端能拿到本地 IP 时仍要求同子网; 拿不到时(典型为 Android
                                        // 绑 0.0.0.0, NewListenAddr 上报 0.0.0.0 被 is_unspecified 跳过
                                        // → local_ips 为空)则只要对端是私网地址就尝试局域网直连。
                                        // 否则会漏掉"真机与 cam 同 LAN 却一直走 relay 拿 sub"的场景:
                                        // 桌面 viewer 靠 mDNS 直连拿 main, 手机 mDNS 在 Android 上不可靠,
                                        // 仅靠此处 Identify 后续的局域网直连升级, 旧逻辑因 local_ips 空
                                        // 被判为非 LAN → 永远卡在 sub。
                                        if local_ips.is_empty() {
                                            true
                                        } else {
                                            local_ips.iter().any(|local_ip| is_same_subnet(*local_ip, remote_ip))
                                        }
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

                                    // 发送直连升级事件
                                    let is_lan = self.connection_quality.connection_type == ConnectionType::LanDirect;
                                    let _ = self.event_sender.send(MediaPlayerEvent::DirectUpgraded { via_lan: is_lan }).await;

                                    // 直连升级成功后，关闭中继连接
                                    // 防止 DCUtR 基于中继连接继续尝试打洞，导致视频卡顿
                                    if let Some(relay_conn_id) = self.relay_connection_id.take() {
                                        tracing::info!("[Viewer] Closing relay circuit connection after direct upgrade (prevents DCUtR interference)");
                                        self.swarm.close_connection(relay_conn_id);
                                    }

                                    // 音频流也升级到直连
                                    if !self.no_audio {
                                        match self.stream_control.open_stream(device_cam, stream_protocols::AUDIO_PROTOCOL).await {
                                            Ok(new_stream) => {
                                                if let Some(h) = self.audio_abort_handle.take() { h.abort(); }
                                                let handle = tokio::spawn(
                                                    Self::receive_frames(device_cam, new_stream, self.audio_sender.clone(), self.event_sender.clone())
                                                ).abort_handle();
                                                self.audio_abort_handle = Some(handle);
                                                println!("[Viewer] Audio stream upgraded to direct connection");
                                            }
                                            Err(e) => {
                                                println!("[Viewer] Failed to open direct audio stream: {e}");
                                            }
                                        }
                                    }

                                    // 控制流也升级到直连
                                    match self.stream_control.open_stream(device_cam, stream_protocols::CONTROL_PROTOCOL).await {
                                        Ok(new_stream) => {
                                            self.control_stream = Some(new_stream);
                                            println!("[Viewer] Control stream upgraded to direct connection");
                                        }
                                        Err(e) => {
                                            println!("[Viewer] Failed to open direct control stream: {e}");
                                        }
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
                SwarmEvent::NewListenAddr { address, .. } => {
                    // 将本地 QUIC 地址注入为 DCUtR 候选地址
                    let is_quic = address.iter().any(|p| matches!(p, Protocol::QuicV1));
                    let is_relayed = address.iter().any(|p| matches!(p, Protocol::P2pCircuit));
                    if is_quic && !is_relayed {
                        if let Some(Protocol::Ip4(ip)) = address.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                            if !ip.is_loopback() && !ip.is_unspecified() {
                                self.swarm.add_external_address(address.clone());
                                tracing::info!("[Viewer] Added local address as DCUtR candidate: {address}");

                                // 收集本地 IP 和端口用于 NAT 诊断
                                if !self.local_ips.contains(&ip) {
                                    self.local_ips.push(ip);
                                }
                                if let Some(Protocol::Udp(port)) = address.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                                    self.local_quic_port = port;
                                }
                                // 延迟初始化 NatDiagnostic（需要端口和 IP）
                                if self.nat_diagnostic.observed_history_is_empty() && self.local_quic_port != 0 && !self.local_ips.is_empty() {
                                    let mut diag = NatDiagnostic::new(self.local_quic_port, self.local_ips.clone());
                                    if self.network_type == "4g" {
                                        diag.set_force_4g(true);
                                    }
                                    self.nat_diagnostic = diag;
                                    tracing::info!("[Viewer] NAT diagnostic initialized: port={}, ips={:?}", self.local_quic_port, self.local_ips);
                                }
                            }
                        }
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

        // 始终重建 Swarm（与老版本 run_viewer_session 行为一致）：
        // 1. 新 Swarm = 新 mDNS 实例 → mDNS 从零发现 → 重连能走 LAN Direct
        // 2. 旧 Swarm 的 mDNS 不会重新触发 Discovered 事件（peer 已知），导致重连走 Relay
        // 3. 若已确认 Symmetric NAT，禁用 DCUtR 避免无效打洞
        let enable_dcutr = !self.symmetric_detected;
        if !enable_dcutr {
            tracing::info!("[Viewer] Rebuilding Swarm with DCUtR DISABLED (Symmetric NAT confirmed)");
        }
        self.swarm = Self::build_swarm(&self.keypair, enable_dcutr)?;
        self.stream_control = self.swarm.behaviour().stream.new_control();
        // 重置 NAT 诊断状态：新 Swarm 需要重新通过 Identify 观测地址
        self.nat_diagnostic = NatDiagnostic::new(0, Vec::new());

        // 关键：重建 Swarm 后必须 listen_on，否则 mDNS/DCUtR/QUIC 全部失效
        let udp_addr = format!("/ip4/0.0.0.0/udp/{}/quic-v1", self.udp_port).parse()
            .context("Invalid local QUIC listen addr")?;
        self.swarm.listen_on(udp_addr)?;
        self.swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()
            .context("Invalid local TCP listen addr")?)?;

        // 停止旧的接收任务
        if let Some(h) = self.video_abort_handle.take() { h.abort(); }
        if let Some(h) = self.audio_abort_handle.take() { h.abort(); }
        self.control_stream = None;

        // 清空 channel 中旧 session 的残留帧，避免新 session 取到旧 P 帧导致花屏
        while self.video_receiver.try_recv().is_ok() {}
        while self.audio_receiver.try_recv().is_ok() {}

        self.connect_with_options(
            &params.relay_addrs,
            &params.device_cam_peer_id,
            params.enable_mdns,
            &params.serial_map,
            &params.stream_type,
            ConnectOptions {
                udp_port: self.udp_port,
                network_type: self.network_type.clone(),
                no_audio: params.no_audio,
            },
        ).await?;

        // connect_with_options 执行期间 receive_frames 已在产出帧，
        // 这些帧积压在 channel 中会导致延迟。丢弃积压帧，只保留最新关键帧及其后续帧。
        self.drain_to_latest_keyframe();

        Ok(())
    }

    /// 优雅关闭当前连接（abort 接收任务 + disconnect peer），但不断开 Swarm
    pub fn shutdown(&mut self) {
        if let Some(h) = self.video_abort_handle.take() { h.abort(); }
        if let Some(h) = self.audio_abort_handle.take() { h.abort(); }
        self.control_stream = None;
        // 清空 channel 残留帧
        while self.video_receiver.try_recv().is_ok() {}
        while self.audio_receiver.try_recv().is_ok() {}
        if let Some(device_cam) = self.device_cam_peer_id {
            let _ = self.swarm.disconnect_peer_id(device_cam);
        }
        self.connected = false;
        println!("[Viewer] Session shutdown, closing connection to device");
    }

    /// 发送控制请求并等待响应 (5s 超时)
    pub async fn send_control(
        &mut self,
        req: &proto::control::ControlRequest,
    ) -> Result<proto::control::ControlResponse> {
        use proto::control::{read_frame, write_frame};

        // 若控制流断开, 下次请求时尝试重新打开 (满足 spec §5.1.3)
        if self.control_stream.is_none() {
            let device_cam = match self.device_cam_peer_id {
                Some(id) => id,
                None => return Err(anyhow::anyhow!("not connected to device-cam")),
            };
            match self.stream_control.open_stream(device_cam, stream_protocols::CONTROL_PROTOCOL).await {
                Ok(s) => {
                    println!("[Viewer] Control stream reopened on demand");
                    self.control_stream = Some(s);
                }
                Err(e) => return Err(anyhow::anyhow!("control stream not ready: {e}")),
            }
        }

        let stream = self.control_stream.as_mut()
            .ok_or_else(|| anyhow::anyhow!("control stream not ready"))?;

        // 编码并发送请求 (write_frame 自带 [4B len] 帧头, 直接传 JSON, 勿再套 encode_request 以免双重封装)
        let json = serde_json::to_vec(req)?;
        write_frame(stream, &json).await?;

        // 读取响应 (5s 超时)
        let payload = tokio::time::timeout(
            Duration::from_secs(5),
            read_frame(stream),
        ).await
            .map_err(|_| anyhow::anyhow!("control request timeout"))??;

        let resp: proto::control::ControlResponse = serde_json::from_slice(&payload)?;
        Ok(resp)
    }

    /// 查询摄像头已合成的 AVI 文件列表 (走控制通道 JSON 返回)
    pub async fn list_snapshots(&mut self) -> Result<Vec<String>> {
        let req = proto::control::ControlRequest::ListSnapshots;
        let resp = self.send_control(&req).await?;
        if !resp.ok {
            let err = resp.error.clone().unwrap_or_else(|| "unknown error".to_string());
            return Err(anyhow::anyhow!(err));
        }
        Ok(resp.avi_files.unwrap_or_default())
    }

    /// 下载指定 AVI 文件: 经控制通道请求 (通知设备端推送), 然后接收设备端经
    /// 独立 FILE_PROTOCOL 入站 stream 推来的分块数据存盘。
    ///
    /// 设计: 设备端 `download_file` 收到控制请求后会**主动 open_stream(FILE_PROTOCOL)**
    /// 把文件推给本端, 因此本端必须先 `accept` 注册入站监听, 再发控制请求触发推送,
    /// 最后从入站流读取分块。切勿反过来用 open_stream, 否则对端未注册 accept 会报
    /// "remote peer does not support /p2p-camera/file/1.0.0"。
    pub async fn download_file(
        &mut self,
        name: &str,
        save_path: &std::path::Path,
    ) -> Result<u64> {
        use proto::control::read_frame;
        use tokio::io::AsyncWriteExt;

        let _device_cam = match self.device_cam_peer_id {
            Some(id) => id,
            None => return Err(anyhow::anyhow!("not connected to device-cam")),
        };

        // 1) 先注册 FILE_PROTOCOL 入站监听 (必须在发控制请求之前, 否则推送流会被拒)
        let mut incoming = self
            .stream_control
            .accept(stream_protocols::FILE_PROTOCOL)
            .map_err(|e| anyhow::anyhow!("register file accept failed: {e}"))?;

        // 2) 控制通道请求下载 (触发设备端主动推送)
        let req = proto::control::ControlRequest::DownloadFile { name: name.to_string() };
        let head = self.send_control(&req).await?;
        if !head.ok {
            let err = head.error.clone().unwrap_or_else(|| "download rejected".to_string());
            return Err(anyhow::anyhow!(err));
        }

        // 3) 等待设备端推来的入站文件流
        let (_peer, mut file_stream) = incoming
            .next()
            .await
            .ok_or_else(|| anyhow::anyhow!("file stream closed before data"))?;

        let mut out = tokio::fs::File::create(save_path).await?;
        let mut total = 0u64;
        loop {
            match read_frame(&mut file_stream).await {
                Ok(chunk) => {
                    if chunk.is_empty() {
                        break; // EOF 标记 (设备端关流或发空帧)
                    }
                    out.write_all(&chunk).await?;
                    total += chunk.len() as u64;
                }
                Err(_) => break, // 设备端关流 = 传输结束
            }
        }
        out.flush().await?;
        drop(incoming); // 停止接受后续入站流
        Ok(total)
    }

    /// 通过控制通道从摄像头读取当前编码参数 (真正读取设备端配置, 非本地缓存)
    pub async fn get_encoder_config(&mut self, stream: &str) -> Result<proto::control::EncoderConfig> {
        let req = proto::control::ControlRequest::GetEncoderConfig { stream: stream.to_string() };
        tracing::info!("[Viewer] >>> GetEncoderConfig stream={stream}");
        let resp = self.send_control(&req).await?;
        if !resp.ok {
            let err = resp.error.clone().unwrap_or_else(|| "unknown error".to_string());
            tracing::warn!("[Viewer] <<< GetEncoderConfig failed: {err}");
            return Err(anyhow::anyhow!(err));
        }
        match resp.encoder_config {
            Some(cfg) => {
                tracing::info!(
                    "[Viewer] <<< GetEncoderConfig response stream={stream}: {} {}x{} fps={}/{} bitrate={}kbps gop={} rc={}/{} gop_mode={} h264_profile={} smart={} rotation={}",
                    cfg.output_data_type, cfg.width, cfg.height,
                    cfg.dst_frame_rate_num, cfg.dst_frame_rate_den, cfg.max_rate,
                    cfg.gop, cfg.rc_mode, cfg.rc_quality, cfg.gop_mode,
                    cfg.h264_profile, cfg.smart, cfg.rotation
                );
                Ok(cfg)
            }
            None => Err(anyhow::anyhow!("control response missing encoder_config")),
        }
    }

    /// 通过控制通道把编码参数下发到摄像头 (热改 + INI 持久化, 由 DeviceCam 端执行)
    pub async fn set_encoder_config(&mut self, stream: &str, config: &proto::control::EncoderConfig) -> Result<()> {
        let req = proto::control::ControlRequest::SetEncoderConfig {
            stream: stream.to_string(),
            config: config.clone(),
        };
        tracing::info!(
            "[Viewer] >>> SetEncoderConfig stream={stream}: {} {}x{} fps={}/{} bitrate={}kbps gop={} rc={}/{} gop_mode={} h264_profile={} smart={} rotation={}",
            config.output_data_type, config.width, config.height,
            config.dst_frame_rate_num, config.dst_frame_rate_den, config.max_rate,
            config.gop, config.rc_mode, config.rc_quality, config.gop_mode,
            config.h264_profile, config.smart, config.rotation
        );
        let resp = self.send_control(&req).await?;
        if !resp.ok {
            let err = resp.error.clone().unwrap_or_else(|| "unknown error".to_string());
            tracing::warn!("[Viewer] <<< SetEncoderConfig failed: {err}");
            return Err(anyhow::anyhow!(err));
        }
        tracing::info!("[Viewer] <<< SetEncoderConfig applied stream={stream}");
        Ok(())
    }

    /// 通过控制通道从摄像头读取当前图像参数 (真正读取设备端配置, 非本地缓存)
    pub async fn get_image_config(&mut self, cam_id: u32) -> Result<proto::control::ImageConfig> {
        let req = proto::control::ControlRequest::GetImageConfig { cam_id };
        tracing::info!("[Viewer] >>> GetImageConfig cam_id={cam_id}");
        let resp = self.send_control(&req).await?;
        if !resp.ok {
            let err = resp.error.clone().unwrap_or_else(|| "unknown error".to_string());
            tracing::warn!("[Viewer] <<< GetImageConfig failed: {err}");
            return Err(anyhow::anyhow!(err));
        }
        match resp.image_config {
            Some(cfg) => {
                tracing::info!("[Viewer] <<< GetImageConfig response cam_id={cam_id}");
                Ok(cfg)
            }
            None => Err(anyhow::anyhow!("control response missing image_config")),
        }
    }

    /// 通过控制通道把图像参数下发到摄像头 (ISP AIQ 热改 + INI 持久化, 由 DeviceCam 端执行)
    pub async fn set_image_config(&mut self, cam_id: u32, config: &proto::control::ImageConfig) -> Result<()> {
        let req = proto::control::ControlRequest::SetImageConfig {
            cam_id,
            config: config.clone(),
        };
        tracing::info!("[Viewer] >>> SetImageConfig cam_id={cam_id}");
        let resp = self.send_control(&req).await?;
        if !resp.ok {
            let err = resp.error.clone().unwrap_or_else(|| "unknown error".to_string());
            tracing::warn!("[Viewer] <<< SetImageConfig failed: {err}");
            return Err(anyhow::anyhow!(err));
        }
        tracing::info!("[Viewer] <<< SetImageConfig applied cam_id={cam_id}");
        Ok(())
    }

    /// 获取本地 IP 列表
    pub fn local_ips(&self) -> &[Ipv4Addr] {
        &self.local_ips
    }

    /// 获取本地 QUIC 端口
    pub fn local_quic_port(&self) -> u16 {
        self.local_quic_port
    }

    /// 设置 DCUtR 启用/禁用（供上层在收到 DcutrBackoff 后调用）
    /// 注意：Toggle 无法运行时切换，此方法仅标记标志，重连时通过 rebuild Swarm 生效
    pub fn set_enable_dcutr(&mut self, enable: bool) {
        if !enable {
            self.symmetric_detected = true;
        }
    }

    // ---- 内部方法 ----

    /// 丢弃 channel 中积压的旧帧，只保留最新关键帧及其后续帧。
    /// 重连后调用，避免 connect_with_options 期间积压的帧导致延迟。
    fn drain_to_latest_keyframe(&mut self) {
        // 收集 channel 中所有视频帧
        let mut frames: Vec<MediaPacket> = Vec::new();
        while let Ok(packet) = self.video_receiver.try_recv() {
            frames.push(packet);
        }

        if frames.is_empty() {
            return;
        }

        let total = frames.len();

        // 找到最后一个关键帧的位置，只保留从该关键帧开始的帧
        let last_kf_pos = frames.iter().rposition(|p| p.is_keyframe());
        if let Some(pos) = last_kf_pos {
            let kept: Vec<MediaPacket> = frames.into_iter().skip(pos).collect();
            let dropped = total - kept.len();
            // 重新放回 channel
            for packet in kept {
                if self.video_sender.try_send(packet).is_err() {
                    break;
                }
            }
            if dropped > 0 {
                tracing::debug!("[Viewer] Drained {dropped} stale frames, kept from latest keyframe");
            }
        }
        // 如果没有关键帧，丢弃所有帧（下一轮 poll_video_frame 会取到新帧）
    }

    /// 等待与 DeviceCam 的连接建立，并判断连接方式（转发/直连）
    ///
    /// 在 ConnectionEstablished 事件中分析 Multiaddr 判断连接方式，
    /// 返回 ConnectionType 用于码流自动选择。
    async fn wait_for_connection_and_classify(&mut self) -> Result<ConnectionType> {
        let device_cam = self.device_cam_peer_id
            .ok_or_else(|| anyhow::anyhow!("device_cam_peer_id not set"))?;

        loop {
            match self.swarm.select_next_some().await {
                SwarmEvent::ConnectionEstablished { peer_id, endpoint, connection_id, .. } => {
                    // 只关注 device-cam 的连接事件，忽略其他 peer（如 relay）
                    if peer_id != device_cam {
                        tracing::debug!("[Viewer] Ignoring connection to non-target peer {peer_id}");
                        continue;
                    }

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
                SwarmEvent::OutgoingConnectionError { peer_id: Some(peer_id), error, .. } => {
                    if peer_id == device_cam {
                        anyhow::bail!("Connection to device-cam failed: {error}");
                    }
                    tracing::warn!("[Viewer] Connection error to {peer_id}: {error}");
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
    /// EOF 时发送 StreamEOF 通知，读错误时发送 Disconnected 通知
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
                    let _ = event_sender.send(MediaPlayerEvent::StreamEOF {
                        reason: format!("Stream EOF from {peer_id}"),
                    }).await;
                    break;
                }
                Ok(n) => {
                    buf.extend_from_slice(&read_buf[..n]);

                    // 尝试解码所有完整的包
                    while let Some(mut packet) = MediaPacket::try_decode(&mut buf) {
                        // 视频包: receiver 自包含地扫描一次关键帧, 结果写入 packet.flags 的
                        // FLAG_VIDEO_KEYFRAME bit, 下游 (drain / media_viewer / JNI) 用 is_keyframe()
                        // 直接复用, 避免每帧重复字节扫描。不信任 cam 端 flag (见 rk_video_source.rs),
                        // 此处为唯一权威判定。
                        if packet.track == MediaTrack::Video {
                            if is_nal_keyframe(&packet.data) {
                                packet.flags |= FLAG_VIDEO_KEYFRAME;
                            }
                            if !got_first_idr {
                                if first_video_at.is_none() {
                                    first_video_at = Some(std::time::Instant::now());
                                }
                                let waited = first_video_at.map(|t| t.elapsed());
                                if packet.is_keyframe() || waited.map_or(false, |w| w >= IDR_WAIT_TIMEOUT) {
                                    got_first_idr = true;
                                    if packet.is_keyframe() {
                                        tracing::info!(
                                            "[Viewer] First IDR received, video decode started (dropped pre-IDR non-keyframes)"
                                        );
                                    } else {
                                        tracing::warn!(
                                            "[Viewer] No IDR within {:?}, forwarding video anyway (cam keyframe flag may be unreliable)",
                                            IDR_WAIT_TIMEOUT
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
