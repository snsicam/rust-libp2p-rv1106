//! P2P Camera DeviceCam — 运行在 RV1106 上的媒体网关
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
mod config;
mod media_source;
mod net_diag;
#[cfg(feature = "rv1106")]
mod rk_video_source;

use std::collections::HashMap;
use std::net::Ipv4Addr;
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
use net_diag::{DcutrPrediction, NatDiagnostic, NatType};
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
    println!("[DeviceCam] p2p-camera device-cam v{} ({})", env!("CARGO_PKG_VERSION"), env!("BUILD_TIME"));

    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let opt = Opt::parse();

    // ---- 加载配置文件 ----
    let mut cfg = config::Config::load(&opt.config).unwrap_or_else(|e| {
        eprintln!("[DeviceCam] {e}");
        std::process::exit(1);
    });

    // 命令行参数覆盖配置文件
    let cli_overrides = config::CliOverrides {
        relay: opt.relay,
        mode: opt.mode,
        key_file: opt.key_file,
        enable_audio: opt.enable_audio,
        udp_port: opt.udp_port,
        width: opt.width,
        height: opt.height,
        fps: opt.fps,
        bitrate: opt.bitrate,
        video_file: None, // handled separately below
    };
    cfg.apply_cli_overrides(&cli_overrides);

    // video_file 只在非 rv1106 模式下有效
    #[cfg(not(feature = "rv1106"))]
    if let Some(ref vf) = opt.video_file {
        cfg.video_file = Some(vf.clone());
    }

    if cfg.relay.is_empty() {
        eprintln!("[DeviceCam] Error: relay address is empty. Edit {} or use --relay", opt.config.display());
        std::process::exit(1);
    }

    validate_device_cam_config(&cfg);

    // ---- 初始化媒体源 (文件 or RV1106 SDK) ----
    // 媒体源独立于 P2P 连接，在重连期间持续运行
    let (video_tx, _video_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);
    let (audio_tx, _audio_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);

    // 参数集缓存 (VPS/SPS/PPS) — 新 viewer 连接时先发送这些，避免 "PPS id out of range"
    let param_sets: Option<std::sync::Arc<std::sync::Mutex<Option<Vec<Vec<u8>>>>>>;

    #[cfg(feature = "rv1106")]
    {
        // RV1106 真实摄像头
        let width = cfg.width;
        let height = cfg.height;
        let fps = cfg.fps;
        let bitrate = cfg.bitrate;
        println!("[DeviceCam] Video source: RV1106 camera {}x{} @{}fps {}kbps", width, height, fps, bitrate);
        let source = rk_video_source::RkVideoSource::new(width, height, fps, bitrate);
        param_sets = Some(source.param_sets_handle());
        let (_, _start_tx) = source.spawn(broadcast_sender_to_crossbeam(video_tx.clone()));
        // RV1106 模式下视频源自动开始，不需要 start trigger
    }

    #[cfg(not(feature = "rv1106"))]
    {
        if let Some(video_path) = &cfg.video_file {
            let data = std::fs::read(video_path)
                .context("Failed to read video file")?;
            println!("[DeviceCam] Video file: {:?} ({} bytes)", video_path, data.len());
            let source = media_source::FileVideoSource::from_file(data);
            param_sets = Some(source.param_sets_handle());
            // 文件源在第一个 viewer 连接时启动 (循环播放模式)
            let (_stop_tx, _start_tx) = source.spawn(broadcast_sender_to_crossbeam(video_tx.clone()));
            // 立即开始播放 (不再等第一个 viewer)
            let _ = _start_tx.send(());
            println!("[DeviceCam] Video source: file ({:?}) — started", video_path);
        } else {
            println!("[DeviceCam] Video source: NONE (waiting for stream requests)");
            param_sets = None;
        }
    }

    // 音频源
    #[cfg(feature = "rv1106")]
    {
        if cfg.enable_audio {
            let source = rk_video_source::RkAudioSource::new(16000);
            source.spawn(broadcast_sender_to_crossbeam(audio_tx.clone()));
            println!("[DeviceCam] Audio source: RV1106 AI (16kHz mono)");
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        if cfg.enable_audio {
            let source = media_source::SilenceAudioSource::new(16000, 1);
            source.spawn(broadcast_sender_to_crossbeam(audio_tx.clone()));
            println!("[DeviceCam] Audio source: silence (16kHz mono)");
        }
    }

    // ---- 加载/生成固定身份密钥 (保证 PeerId 不变) ----
    let keypair = load_or_create_keypair(&cfg.key_file)?;
    let peer_id = keypair.public().to_peer_id();
    println!("[DeviceCam] PeerId: {peer_id}");

    // ---- 重连循环: Relay 断开时自动重连 ----
    let relay_addr: Multiaddr = cfg.relay.parse()
        .context("Invalid relay address")?;

    let mut reconnect_attempt = 0u64;

    loop {
        reconnect_attempt += 1;
        if reconnect_attempt > 1 {
            tracing::warn!("[DeviceCam] Reconnecting to relay (attempt {reconnect_attempt})...");
            tokio::time::sleep(RECONNECT_DELAY).await;
        }

        match run_device_cam_session(
            keypair.clone(),
            relay_addr.clone(),
            video_tx.clone(),
            audio_tx.clone(),
            param_sets.clone(),
            cfg.udp_port,
        ).await {
            Ok(()) => break, // 正常退出
            Err(e) => {
                tracing::error!("[DeviceCam] Session ended: {e}");
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
        println!("[DeviceCam] Loaded identity from {}", key_file.display());
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
        println!("[DeviceCam] Generated new identity → {}", key_file.display());
        Ok(keypair)
    }
}

/// 一次完整的 DeviceCam 会话: 连接 Relay → 预约 → 接受 stream 请求
///
/// 返回 Err 表示需要重连
async fn run_device_cam_session(
    keypair: identity::Keypair,
    relay_addr: Multiaddr,
    video_tx: broadcast::Sender<MediaPacket>,
    audio_tx: broadcast::Sender<MediaPacket>,
    _param_sets: Option<std::sync::Arc<std::sync::Mutex<Option<Vec<Vec<u8>>>>>>, // 添加下划线前缀消除警告
    udp_port: Option<u16>,
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
                "/p2p-camera-device-cam/1.0.0".to_string(),
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

    println!("[DeviceCam] PeerId: {peer_id}");
    tracing::info!("[DeviceCam] push_listen_addr_updates enabled for DCUtR");

    let mut connection_times: HashMap<PeerId, Instant> = HashMap::new();
    // 记录每个 peer 的连接类型 (用于 stream accept 时输出)
    let mut peer_conn_type: HashMap<PeerId, String> = HashMap::new();

    // ---- NAT 诊断状态 ----
    let mut nat_diagnostic: Option<NatDiagnostic> = None;
    let mut local_nat_type: Option<NatType> = None;
    let mut local_ips: Vec<Ipv4Addr> = Vec::new();
    let mut local_quic_port: u16 = 0;

    // ---- 监听本地 QUIC (固定端口，若指定) ----
    let udp_port = udp_port.unwrap_or(0);
    let udp_addr = format!("/ip4/0.0.0.0/udp/{}/quic-v1", udp_port).parse()
        .context("Invalid local QUIC listen addr")?;
    swarm.listen_on(udp_addr)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()
        .context("Invalid local TCP listen addr")?)?;
    println!("[DeviceCam] Listening on QUIC (port {}) and TCP",
        if udp_port != 0 { udp_port.to_string() } else { "random".to_string() });

    // ---- 外部地址由 identify 协议自动发现（公网 IP）和 NewListenAddr 事件自动注入（本地 IP） ----

    // ---- 连接 Relay Server ----
    // 从 relay 地址中提取 PeerId，用于区分 relay 连接和 viewer 连接
    let relay_peer_id: PeerId = relay_addr.iter()
        .find_map(|p| match p {
            Protocol::P2p(peer_id) => Some(peer_id),
            _ => None,
        })
        .ok_or_else(|| anyhow::anyhow!("Relay address must contain PeerId"))?;

    swarm.dial(relay_addr.clone())?;
    println!("[DeviceCam] Dialing relay: {relay_addr}");

    // 等待与 Relay 的连接建立
    wait_for_connection(&mut swarm).await?;

    // ---- 在 Relay 上预约 ----
    let reservation_id = swarm.listen_on(
        relay_addr.with(Protocol::P2pCircuit),
    )?;
    println!("[DeviceCam] Requesting relay reservation...");

    // 等待预约成功
    wait_for_reservation(&mut swarm, reservation_id).await?;

    println!("[DeviceCam] Relay reservation confirmed!");
    println!("[DeviceCam] External address: /p2p-circuit/p2p/{peer_id}");

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
                        println!("[DeviceCam] Relay reservation accepted!");
                    }

                    SwarmEvent::Behaviour(behaviour::BehaviourEvent::Dcutr(
                        dcutr::Event { remote_peer_id, result, .. },
                    )) => match result {
                        Ok(_conn_id) => {
                            tracing::info!("[DeviceCam] DCUtR direct connection established with {remote_peer_id}");
                            tracing::info!("[DeviceCam] Direct connection upgrade successful - switching from relay to direct connection");
                        }
                        Err(err) => {
                            let local_nat = local_nat_type.map(|t| t.short_name()).unwrap_or("Unknown");
                            tracing::warn!("[DeviceCam] DCUtR failed with {remote_peer_id}: {err}");
                            tracing::warn!("[DeviceCam] NAT context: local={}", local_nat);
                            if let Some(ref diag) = nat_diagnostic {
                                let result = diag.diagnose();
                                tracing::warn!("[DeviceCam] Suggestion: {}", result.dcutr_suggestion);
                            } else {
                                let err_str = err.to_string();
                                if err_str.contains("timeout") {
                                    tracing::warn!("[DeviceCam] DCUtR failure cause: NAT type incompatibility or firewall blocking UDP");
                                } else if err_str.contains("IO error") || err_str.contains("connection refused") || err_str.contains("network unreachable") {
                                    tracing::warn!("[DeviceCam] DCUtR failure cause: network unreachable or connection refused");
                                }
                                tracing::warn!("[DeviceCam] If both peers are behind symmetric NAT, DCUtR cannot succeed. Consider:");
                                tracing::warn!("[DeviceCam]   1. Configure port forwarding on router (map external UDP port → device internal UDP port)");
                                tracing::warn!("[DeviceCam]   2. Use --external-ip and --udp-port to advertise correct external address");
                            }
                            // 快速降级确认：DCUtR 失败后确认 Relay Circuit 仍在工作
                            let has_relay = swarm.is_connected(&relay_peer_id);
                            if has_relay {
                                tracing::info!("[DeviceCam] Fallback: Relay circuit is still active, video/audio will continue via relay");
                            } else {
                                tracing::warn!("[DeviceCam] Fallback: Relay connection may be lost, reconnection may be needed");
                            }
                        }
                    },

                    SwarmEvent::Behaviour(behaviour::BehaviourEvent::Identify(
                        identify::Event::Received { info, .. },
                    )) => {
                        tracing::info!("[DeviceCam] Identify received from peer:");
                        tracing::info!("  - Observed address: {}", info.observed_addr);
                        tracing::info!("  - Listen addresses ({} total):", info.listen_addrs.len());
                        for (i, addr) in info.listen_addrs.iter().enumerate() {
                            tracing::info!("    [{}]: {}", i, addr);
                        }

                        // NAT 诊断：记录观测地址
                        if let Some(ref mut diag) = nat_diagnostic {
                            diag.record_observed(&info.observed_addr);
                            let result = diag.diagnose();
                            tracing::info!("[DeviceCam] NAT diagnosis: {}", result.nat_type.description());
                            if result.is_4g {
                                tracing::info!("[DeviceCam] 4G/CGNAT network detected");
                            }
                            tracing::info!("[DeviceCam] DCUtR suggestion: {}", result.dcutr_suggestion);
                            local_nat_type = Some(result.nat_type);
                        }

                        // 局域网直连检测：检查对端 listen_addrs 中是否有与本地 IP 同子网的 QUIC 地址
                        {
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
                                for addr in &lan_addrs {
                                    tracing::info!("[DeviceCam] LAN direct: detected same-subnet peer address {addr}");
                                }
                                tracing::info!("[DeviceCam] LAN direct: peer is on the same subnet, viewer may dial us directly");
                            }
                        }

                        if let Some(Protocol::Ip4(ip)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                            if ip.is_private() {
                                tracing::warn!("[DeviceCam] WARNING: Observed address is private IP ({}) - DCUtR may fail!", ip);
                            } else {
                                tracing::info!("[DeviceCam] Observed address is public IP ({}) - good for DCUtR", ip);
                            }
                        }
                        if info.observed_addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                            tracing::info!("[DeviceCam] Observed address protocol: QUIC - good for DCUtR hole punching");
                            // NAT 端口映射检测: observed_addr 的 UDP 端口与本地监听端口不一致时，可能是对称型 NAT
                            if let Some(Protocol::Udp(observed_port)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                                if udp_port != 0 && observed_port != udp_port {
                                    tracing::warn!("[DeviceCam] NAT port mapping detected: local UDP port {} → observed UDP port {}", udp_port, observed_port);
                                    tracing::warn!("[DeviceCam] This may indicate symmetric NAT, which prevents DCUtR hole-punching");
                                    tracing::info!("[DeviceCam] Consider configuring port forwarding: router maps external UDP {} → internal UDP {}", observed_port, udp_port);
                                } else if udp_port != 0 && observed_port == udp_port {
                                    tracing::info!("[DeviceCam] Observed UDP port {} matches local QUIC port - good for DCUtR", observed_port);
                                }
                            }
                        } else if info.observed_addr.iter().any(|p| matches!(p, Protocol::Tcp(_))) {
                            tracing::warn!("[DeviceCam] Observed address protocol: TCP only - DCUtR will produce TCP candidates, hole punching unlikely to succeed");
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
                        println!("[DeviceCam] Listening on: {address}");

                        // 将本地 QUIC 地址注入为 DCUtR 候选地址
                        // 同 NAT 场景下，本地地址可直接路由，解决 NAT hairpin 不支持的问题
                        // 使用黑名单策略：排除回环和未指定地址，其余本地网卡地址均可作为候选
                        // （包括 172.32.x.x 等 is_private() 不覆盖的 VPN/Docker 地址）
                        let is_quic = address.iter().any(|p| matches!(p, Protocol::QuicV1));
                        let is_relayed = address.iter().any(|p| matches!(p, Protocol::P2pCircuit));
                        if is_quic && !is_relayed {
                            if let Some(Protocol::Ip4(ip)) = address.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                                if !ip.is_loopback() && !ip.is_unspecified() {
                                    swarm.add_external_address(address.clone());
                                    tracing::info!("[DeviceCam] Added local address as DCUtR candidate: {address}");

                                    // 收集本地 IP 和端口用于 NAT 诊断
                                    if !local_ips.contains(&ip) {
                                        local_ips.push(ip);
                                    }
                                    if let Some(Protocol::Udp(port)) = address.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                                        local_quic_port = port;
                                    }
                                    // 延迟初始化 NatDiagnostic
                                    if nat_diagnostic.is_none() && local_quic_port != 0 {
                                        nat_diagnostic = Some(NatDiagnostic::new(local_quic_port, local_ips.clone()));
                                        tracing::info!("[DeviceCam] NAT diagnostic initialized: port={}, ips={:?}", local_quic_port, local_ips);
                                    }
                                }
                            }
                        }
                    }

                    SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                        let role = if endpoint.is_dialer() { "outgoing" } else { "incoming" };
                        let addr = endpoint.get_remote_address().clone();
                        connection_times.insert(peer_id, Instant::now());

                        // 判断连接类型
                        let conn_type = if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                            "Relay Circuit (转发)".to_string()
                        } else if addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                            let is_lan = addr.iter().any(|p| {
                                if let Protocol::Ip4(ip) = p { ip.is_private() } else { false }
                            });
                            if is_lan {
                                "LAN Direct (局域网直连)".to_string()
                            } else {
                                "DCUtR Hole Punch (打洞直连)".to_string()
                            }
                        } else {
                            "Other".to_string()
                        };

                        // 记录连接类型，供 stream accept 时使用
                        peer_conn_type.insert(peer_id, conn_type.clone());

                        // 非 Relay Server 的连接才输出醒目信息
                        if peer_id != relay_peer_id {
                            println!("[DeviceCam] *** Viewer connected: {peer_id} via {conn_type} ***");

                            // DCUtR 尝试前预测：在 circuit 连接建立后立即输出 NAT 上下文
                            if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                                if let Some(ref diag) = nat_diagnostic {
                                    let prediction = diag.dcutr_prediction();
                                    if prediction.likely_success {
                                        tracing::info!("[DeviceCam] DCUtR prediction: likely SUCCESS - {}", prediction.reason);
                                    } else {
                                        tracing::warn!("[DeviceCam] DCUtR prediction: likely FAIL - {}", prediction.reason);
                                    }
                                } else {
                                    tracing::info!("[DeviceCam] DCUtR will be attempted (NAT diagnostic not yet available)");
                                }
                            }
                        }

                        tracing::info!("[DeviceCam] Connection established:");
                        tracing::info!("  - Peer ID: {peer_id}");
                        tracing::info!("  - Role: {role}");
                        tracing::info!("  - Remote address: {addr}");
                        tracing::info!("  - Type: {conn_type}");
                    }

                    SwarmEvent::ConnectionClosed { peer_id, endpoint, cause, num_established, .. } => {
                        let role = if endpoint.is_dialer() { "outgoing" } else { "incoming" };
                        let addr = endpoint.get_remote_address().clone();
                        let duration = connection_times.remove(&peer_id)
                            .map(|t| t.elapsed())
                            .map(|d| format!("{:.1}s", d.as_secs_f64()))
                            .unwrap_or_else(|| "unknown".to_string());
                        let conn_type = peer_conn_type.get(&peer_id).cloned().unwrap_or_else(|| "Unknown".to_string());
                        if num_established == 0 {
                            peer_conn_type.remove(&peer_id);
                        }
                        tracing::warn!("[DeviceCam] Connection closed:");
                        tracing::warn!("  - Peer ID: {peer_id}");
                        tracing::warn!("  - Role: {role}");
                        tracing::warn!("  - Remote address: {addr}");
                        tracing::warn!("  - Connection duration: {duration}");
                        tracing::warn!("  - Type: {conn_type}");
                        if let Some(cause) = cause {
                            tracing::warn!("  - Cause: {cause}");
                        }
                        tracing::warn!("  - Remaining established connections: {num_established}");

                        // 非 relay 连接断开时输出醒目信息
                        if peer_id != relay_peer_id && num_established == 0 {
                            println!("[DeviceCam] *** Viewer disconnected: {peer_id} (was {conn_type}) ***");
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
                    let conn_type = peer_conn_type.get(&peer_id).map(|s| s.as_str()).unwrap_or("Unknown");
                    println!("[DeviceCam] New video viewer: {peer_id} via {conn_type}");
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
                    let conn_type = peer_conn_type.get(&peer_id).map(|s| s.as_str()).unwrap_or("Unknown");
                    println!("[DeviceCam] New audio viewer: {peer_id} via {conn_type}");
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
                println!("[DeviceCam] Connected to relay {peer_id}");
                return Ok(());
            }
            Ok(SwarmEvent::OutgoingConnectionError { error, .. }) => {
                anyhow::bail!("Failed to connect to relay: {error}");
            }
            Ok(SwarmEvent::NewListenAddr { address, .. }) => {
                println!("[DeviceCam] Listening on: {address}");
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
                println!("[DeviceCam] Listening on: {address}");
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
        println!("[DeviceCam] Sent {} init NALs to {peer_id}", init_nals.len());
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
                    println!("[DeviceCam] First frame sent to {peer_id} ({} bytes, keyframe={})",
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
                println!("[DeviceCam] Broadcast closed for {peer_id} after {frame_count} frames");
                break;
            }
        }
    }
    let _ = stream.close().await;
    println!("[DeviceCam] Video stream to {peer_id} ended ({frame_count} frames sent)");
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
#[command(name = "p2p-camera device-cam")]
struct Opt {
    /// 配置文件路径 (不存在则自动生成默认配置)
    #[arg(long, default_value = "device-cam.toml")]
    config: PathBuf,

    /// Relay Server 地址 (覆盖配置文件)
    #[arg(long)]
    relay: Option<String>,

    /// 运行模式 (覆盖配置文件)
    #[arg(long)]
    mode: Option<String>,

    /// 身份密钥文件 (覆盖配置文件)
    #[arg(long)]
    key_file: Option<PathBuf>,

    /// 视频裸流文件 (H.265) — 代替 SDK 回调 (非 rv1106 feature)
    #[cfg(not(feature = "rv1106"))]
    #[arg(long)]
    video_file: Option<std::path::PathBuf>,

    /// 启用模拟音频 (覆盖配置文件)
    #[arg(long, default_value_t = false)]
    enable_audio: bool,

    /// [rv1106] 视频宽度 (覆盖配置文件)
    #[arg(long)]
    width: Option<u32>,

    /// [rv1106] 视频高度 (覆盖配置文件)
    #[arg(long)]
    height: Option<u32>,

    /// [rv1106] 帧率 (覆盖配置文件)
    #[arg(long)]
    fps: Option<u32>,

    /// [rv1106] 码率 (kbps) (覆盖配置文件)
    #[arg(long)]
    bitrate: Option<u32>,

    /// QUIC UDP 监听端口（若固定，便于端口映射）
    #[arg(long)]
    udp_port: Option<u16>,
}

fn validate_device_cam_config(cfg: &config::Config) {
    let relay_str = &cfg.relay;
    if relay_str.contains("/tcp/") && !relay_str.contains("/quic-v1") {
        tracing::warn!("[DeviceCam] WARNING: Using TCP relay connection - DCUtR will only produce TCP candidates, hole punching unlikely to succeed. Use /udp/<port>/quic-v1 instead");
    } else if relay_str.contains("/quic-v1") {
        tracing::info!("[DeviceCam] Relay connection protocol: QUIC - good for DCUtR hole punching");
    }

    if let Some(port) = cfg.udp_port {
        if port == 0 {
            tracing::warn!("[DeviceCam] WARNING: Using random UDP port - cannot configure port forwarding for DCUtR");
        }
    }
}

/// 检查两个 IPv4 地址是否在同一 /24 子网
fn is_same_subnet(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let a = u32::from(a);
    let b = u32::from(b);
    (a & 0xFFFFFF00) == (b & 0xFFFFFF00)
}
