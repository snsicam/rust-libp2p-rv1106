//! P2P Camera DeviceCam — 运行在 RV1106 上的媒体网关
//!
//! 职责:
//! 1. 连接 Relay Server 并在其上预约 (Circuit Relay v2 Reservation)
//! 2. 通过 DCUtR 与 Viewer 协商直连
//! 3. 接受 Viewer 的视频/音频 stream 请求 (三码流: main/sub/third)
//! 4. 从媒体源 (SDK/文件) 读取帧并通过 stream 发送
//!
//! 视频三码流:
//!   - /p2p-camera/video/main/1.0.0  主码流 (高清)
//!   - /p2p-camera/video/sub/1.0.0   子码流 (标清, 低码率)
//!   - /p2p-camera/video/third/1.0.0 第三码流 (中清)
//!
//! 自动重连: Relay 断开时自动重新连接 + 重新预约，媒体源和已有直连不受影响。
//! Swarm 只创建一次，重连时在同一个 Swarm 内重新 dial relay。
//!
//! 固定身份: 首次运行自动生成 Ed25519 密钥并保存到 key_file，
//!           后续启动从文件读取，保证 PeerId 不变。

mod behaviour;
mod config;
mod control_handler;
mod media_source;
mod net_diag;
#[cfg(feature = "rv1106")]
mod rk_video_source;

use std::collections::HashMap;
use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use behaviour::Behaviour;
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
use net_diag::{ConnectionStrategy, NatDiagnostic, NatType};
use proto::{
    media_packet::MediaPacket,
    stream_protocols,
};
use tokio::sync::broadcast;
use tracing_subscriber::EnvFilter;

// broadcast channel 容量: 缓冲约 2 秒的视频帧 (25fps * 2)
const BROADCAST_CAPACITY: usize = 100;

// 重连间隔 (指数退避: base → 2x → 4x → ... → max)
const RECONNECT_DELAY_BASE: Duration = Duration::from_secs(3);
const RECONNECT_DELAY_MAX: Duration = Duration::from_secs(60);

/// 单个 Relay 的连接和预约状态
struct RelayState {
    addr: Multiaddr,
    peer_id: PeerId,
    reservation_id: Option<libp2p::core::transport::ListenerId>,
    connected: bool,
    reconnect_attempt: u32,
    dial_pending: bool,
    /// 重连定时器：到期时执行 swarm.dial()
    reconnect_timer: Option<Pin<Box<tokio::time::Sleep>>>,
}

impl RelayState {
    fn reconnect_delay(&self) -> Duration {
        let delay_secs = (RECONNECT_DELAY_BASE.as_secs() as u64)
            .saturating_mul(1u64 << self.reconnect_attempt.saturating_sub(1).min(5));
        Duration::from_secs(delay_secs).min(RECONNECT_DELAY_MAX)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    println!("[DeviceCam] p2p-camera device-cam v{} ({})",
        env!("CARGO_PKG_VERSION"), env!("BUILD_TIME"));

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let opt = Opt::parse();

    // ---- 加载配置文件 ----
    let mut config = config::Config::load(&opt.config).unwrap_or_else(|e| {
        eprintln!("[DeviceCam] {e}");
        std::process::exit(1);
    });

    // 命令行参数覆盖配置文件
    let cli_overrides = config::CliOverrides {
        relays: opt.relays,
        enable_mdns: opt.enable_mdns,
        mode: opt.mode,
        key_file: opt.key_file,
        enable_audio: opt.enable_audio,
        udp_port: opt.udp_port,
        video_file: None,
    };
    config.apply_cli_overrides(&cli_overrides);

    // video_file 只在非 rv1106 模式下有效
    #[cfg(not(feature = "rv1106"))]
    if let Some(ref vf) = opt.video_file {
        config.video_file = Some(vf.clone());
    }

    if config.relays.is_empty() && !config.enable_mdns {
        eprintln!("[DeviceCam] Error: no relay addresses and mDNS is disabled. \
                   Edit {} or use --relay / --enable-mdns", opt.config.display());
        std::process::exit(1);
    }

    validate_device_cam_config(&config);

    // ---- 初始化媒体源 ----
    // 三路视频 broadcast channel
    let (main_tx, _main_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);
    let (sub_tx, _sub_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);
    let (third_tx, _third_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);
    let (audio_tx, _audio_rx) = broadcast::channel::<MediaPacket>(BROADCAST_CAPACITY);

    // 标记每个码流是否启用 (仅在 rv1106 特性下使用)
    #[allow(unused_variables)]
    let main_enabled = config.video.main.enabled;
    #[allow(unused_variables)]
    let sub_enabled = config.video.sub.enabled;
    #[allow(unused_variables)]
    let third_enabled = config.video.third.enabled;

    // RkVideoSource 必须在整个 main() 生命周期内保持存活，因为：
    //   spawn(&self) 后 self 仍被借用 → Drop 不清空 GLOBAL_SENDERS。
    //   若在 cfg 块内 drop, GLOBAL_SENDERS → None → 线程探测到后立即退出
    //   → rk_camera_deinit 段错误。
    #[cfg(feature = "rv1106")]
    let mut _cam_source: Option<rk_video_source::RkVideoSource> = None;

    #[cfg(feature = "rv1106")]
    {
        if main_enabled {
            let main_params = stream_config_to_params(&config.video.main);
            let sub_params = if sub_enabled {
                Some(stream_config_to_params(&config.video.sub))
            } else {
                None
            };
            let third_params = if third_enabled {
                Some(stream_config_to_params(&config.video.third))
            } else {
                None
            };

            println!("[DeviceCam] Video source: RV1106 camera");
            println!("[DeviceCam]   Main:  {}x{} @{}/{}fps {}kbps {}",
                     main_params.width, main_params.height,
                     main_params.dst_fps_num, main_params.dst_fps_den,
                     main_params.bitrate_kbps, main_params.codec);
            if let Some(ref p) = sub_params {
                println!("[DeviceCam]   Sub:   {}x{} @{}/{}fps {}kbps {}",
                         p.width, p.height,
                         p.dst_fps_num, p.dst_fps_den,
                         p.bitrate_kbps, p.codec);
            }
            if let Some(ref p) = third_params {
                println!("[DeviceCam]   Third: {}x{} @{}/{}fps {}kbps {}",
                         p.width, p.height,
                         p.dst_fps_num, p.dst_fps_den,
                         p.bitrate_kbps, p.codec);
            }

            let mut source = rk_video_source::RkVideoSource::new(
                main_params, sub_params, third_params,
                config.sensor_frame_rate,
            );
            if config.lcd.enabled {
                source = source.with_lcd(config.lcd.width, config.lcd.height);
            }
            let (_, start_tx) = source.spawn(
                broadcast_sender_to_crossbeam(main_tx.clone()),
                if sub_enabled { Some(broadcast_sender_to_crossbeam(sub_tx.clone())) } else { None },
                if third_enabled { Some(broadcast_sender_to_crossbeam(third_tx.clone())) } else { None },
            );
            // 立即启动摄像头：RkVideoSource 内部线程在 start_rx.recv() 处阻塞，
            // 必须发送开始信号才会初始化摄像头并开始取流 (rk_camera_init)。
            // 否则 on_frame 回调永不触发，main_tx 永远为空，viewer 收不到任何视频帧。
            let _ = start_tx.send(());

            // 将 source 移出块作用域: 它必须在整个 main() 生命周期保持存活
            _cam_source = Some(source);
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        if let Some(video_path) = &config.video_file {
            let data = std::fs::read(video_path)
                .context("Failed to read video file")?;
            println!("[DeviceCam] Video file: {:?} ({} bytes)", video_path, data.len());
            let source = media_source::FileVideoSource::from_file(data);
            let (_stop_tx, _start_tx) = source.spawn(broadcast_sender_to_crossbeam(main_tx.clone()));
            let _ = _start_tx.send(());
            println!("[DeviceCam] Video source: file ({:?}) — started", video_path);
        } else {
            println!("[DeviceCam] Video source: NONE (waiting for stream requests)");
        }
    }

    // 音频源
    #[cfg(feature = "rv1106")]
    {
        if config.audio.enabled {
            let source = rk_video_source::RkAudioSource::new(
                config.audio.sample_rate,
                config.audio.card_name.clone(),
                config.audio.channels,
                config.audio.frame_size,
                config.audio.volume,
                config.audio.format.clone(),
                config.audio.encode_type.clone(),
                config.audio.bit_rate,
                config.audio.enable_vqe,
                config.audio.vqe_cfg.clone(),
            );
            source.spawn(broadcast_sender_to_crossbeam(audio_tx.clone()));
            println!("[DeviceCam] Audio source: RV1106 AI ({}Hz mono, frame={}, encode={})",
                config.audio.sample_rate, config.audio.frame_size, config.audio.encode_type);
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        if config.audio.enabled {
            let source = media_source::SilenceAudioSource::new(config.audio.sample_rate, config.audio.channels as u8);
            source.spawn(broadcast_sender_to_crossbeam(audio_tx.clone()));
            println!("[DeviceCam] Audio source: silence (16kHz mono)");
        }
    }

    // ---- 加载/生成固定身份密钥 ----
    let keypair = load_or_create_keypair(&config.key_file)?;
    let peer_id = keypair.public().to_peer_id();
    println!("[DeviceCam] PeerId: {peer_id}");

    // ---- 解析 Relay 地址列表 ----
    let relay_states: Vec<RelayState> = config.relays.iter().map(|addr_str| {
        let addr: Multiaddr = addr_str.parse()
            .context(format!("Invalid relay address: {addr_str}"))
            .unwrap();
        let peer_id = addr.iter()
            .find_map(|p| match p {
                Protocol::P2p(peer_id) => Some(peer_id),
                _ => None,
            })
            .expect("Relay address must contain PeerId");
        RelayState {
            addr,
            peer_id,
            reservation_id: None,
            connected: false,
            reconnect_attempt: 0,
            dial_pending: false,
            reconnect_timer: None,
        }
    }).collect();

    if !relay_states.is_empty() {
        println!("[DeviceCam] Configured {} relay(s)", relay_states.len());
        for (i, state) in relay_states.iter().enumerate() {
            println!("[DeviceCam]   Relay #{}: {}", i + 1, state.peer_id);
        }
    }
    if config.enable_mdns {
        println!("[DeviceCam] mDNS enabled - LAN discovery active");
    }

    // ---- 构建 Swarm ----
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
            Behaviour::new_with_identify_config(
                key.public().clone(),
                relay_client,
                identify_config,
                config.enable_dcutr,
            )
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();

    tracing::info!("[DeviceCam] push_listen_addr_updates enabled for DCUtR");

    // ---- 监听本地 QUIC / TCP ----
    let udp_port = config.udp_port.unwrap_or(0);
    let udp_addr = format!("/ip4/0.0.0.0/udp/{}/quic-v1", udp_port).parse()
        .context("Invalid local QUIC listen addr")?;
    swarm.listen_on(udp_addr)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()
        .context("Invalid local TCP listen addr")?)?;
    println!("[DeviceCam] Listening on QUIC (port {}) and TCP",
        if udp_port != 0 { udp_port.to_string() } else { "random".to_string() });

    // ---- Stream 控制 (三路视频 + 音频 + 向后兼容旧协议) ----
    let mut stream_control = swarm.behaviour().new_stream_control();

    // 主码流
    let mut incoming_main = stream_control
        .accept(stream_protocols::VIDEO_MAIN_PROTOCOL)
        .context("Failed to accept main video protocol")?;
    // 子码流
    let mut incoming_sub = stream_control
        .accept(stream_protocols::VIDEO_SUB_PROTOCOL)
        .context("Failed to accept sub video protocol")?;
    // 第三码流
    let mut incoming_third = stream_control
        .accept(stream_protocols::VIDEO_THIRD_PROTOCOL)
        .context("Failed to accept third video protocol")?;
    // 音频
    let mut incoming_audio = stream_control
        .accept(stream_protocols::AUDIO_PROTOCOL)
        .context("Failed to accept audio protocol")?;
    // 控制通道
    let mut incoming_control = stream_control
        .accept(stream_protocols::CONTROL_PROTOCOL)
        .context("Failed to accept control protocol")?;

    // ---- 状态 ----
    let mut connection_times: HashMap<PeerId, Instant> = HashMap::new();
    let mut peer_conn_type: HashMap<PeerId, String> = HashMap::new();
    let mut nat_diagnostic: Option<NatDiagnostic> = None;
    let mut local_nat_type: Option<NatType> = None;
    let mut local_ips: Vec<Ipv4Addr> = Vec::new();
    let mut local_quic_port: u16 = 0;
    let mut relay_states: Vec<RelayState> = relay_states;
    // 每个 viewer 的 DCUtR 失败次数。4G/CGNAT 等入站 UDP 被屏蔽的网络下打洞必然失败，
    // 反复失败说明该 peer 无法直连，应停用 DCUtR 以免其握手挤占中继视频流带宽。
    let mut dcutr_fail_count: HashMap<PeerId, u32> = HashMap::new();

    // 初始连接所有 Relay
    for state in &relay_states {
        println!("[DeviceCam] Dialing relay: {}", state.addr);
        if let Err(e) = swarm.dial(state.addr.clone()) {
            tracing::error!("[DeviceCam] Failed to dial relay {}: {e}", state.peer_id);
        }
    }

    // ---- 事件循环 ----
    // 活跃视频/音频发送任务注册表: (PeerId, 流类型) -> 任务句柄
    // 用于同 peer 同路流去重: viewer 最小化/重连可能重复打开同一路流,
    // 若不及时 abort 旧任务, 会出现多路流同时推送 (两路视频流打架)。
    let mut active_streams: HashMap<(PeerId, &str), tokio::task::JoinHandle<()>> = HashMap::new();

    loop {
        tokio::select! {
            // Swarm 事件 (与之前相同)
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(behaviour::BehaviourEvent::RelayClient(
                        relay::client::Event::ReservationReqAccepted { relay_peer_id, .. },
                    )) => {
                        if let Some(state) = relay_states.iter_mut().find(|s| s.peer_id == relay_peer_id) {
                            if !state.connected {
                                println!("[DeviceCam] Relay {} reservation confirmed!", relay_peer_id);
                                state.connected = true;
                                state.reconnect_attempt = 0;
                                state.dial_pending = false;
                                state.reconnect_timer = None;
                            }
                        }
                        let reserved_count = relay_states.iter().filter(|s| s.connected).count();
                        if reserved_count > 0 {
                            println!("[DeviceCam] {reserved_count}/{} relay(s) have active reservations", relay_states.len());
                        }
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
                            // 按 peer 累计打洞失败次数；反复失败说明该 viewer 无法直连
                            // (典型为 4G/CGNAT 入站 UDP 被屏蔽)，应停用 DCUtR 以免其握手
                            // 挤占中继视频流写入带宽，导致 SLOW write / 卡顿。
                            let fails = dcutr_fail_count.entry(remote_peer_id).or_insert(0);
                            *fails += 1;
                            if *fails >= 2 && config.enable_dcutr {
                                tracing::warn!(
                                    "[DeviceCam] DCUtR 对该 viewer 已失败 {} 次，打洞对本端无效。\
                                     建议在该 device-cam.toml 设置 enable_dcutr = false 仅走中继，\
                                     可消除打洞握手对视频流的干扰与卡顿。",
                                    *fails
                                );
                            }
                            tracing::warn!("[DeviceCam] NAT context: local={}", local_nat);
                            if let Some(ref diag) = nat_diagnostic {
                                let result = diag.diagnose();
                                tracing::warn!("[DeviceCam] Suggestion: {}", result.dcutr_suggestion);
                                // 连接策略结果日志
                                let (strategy, _) = diag.connection_strategy();
                                if matches!(strategy, ConnectionStrategy::SkipDcutr) {
                                    tracing::info!("[DeviceCam] Connection strategy result: DCUtR failed as predicted (Symmetric NAT/4G), fallback to Relay Circuit");
                                }
                            } else {
                                let err_str = err.to_string();
                                if err_str.contains("timeout") {
                                    tracing::warn!("[DeviceCam] DCUtR failure cause: NAT type incompatibility or firewall blocking UDP");
                                } else if err_str.contains("IO error") || err_str.contains("connection refused") || err_str.contains("network unreachable") {
                                    tracing::warn!("[DeviceCam] DCUtR failure cause: network unreachable or connection refused");
                                }
                                tracing::warn!("[DeviceCam] If both peers are behind symmetric NAT, DCUtR cannot succeed.");
                            }
                            let has_any_relay = relay_states.iter().any(|s| s.connected);
                            if has_any_relay {
                                tracing::info!("[DeviceCam] Fallback: Relay circuit is still active");
                            } else {
                                tracing::warn!("[DeviceCam] Fallback: No relay connection available");
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

                        // 局域网直连检测
                        {
                            let lan_addrs: Vec<Multiaddr> = info.listen_addrs.iter()
                                .filter(|a| a.iter().any(|p| matches!(p, Protocol::QuicV1)))
                                .filter(|a| !a.iter().any(|p| matches!(p, Protocol::P2pCircuit)))
                                .filter(|a| {
                                    if let Some(Protocol::Ip4(ip)) = a.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                                        ip.is_private() && !ip.is_loopback()
                                    } else { false }
                                })
                                .filter(|a| {
                                    if let Some(Protocol::Ip4(remote_ip)) = a.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                                        local_ips.iter().any(|local_ip| is_same_subnet(*local_ip, remote_ip))
                                    } else { false }
                                })
                                .cloned().collect();

                            if !lan_addrs.is_empty() {
                                for addr in &lan_addrs {
                                    tracing::info!("[DeviceCam] LAN direct: detected same-subnet peer address {addr}");
                                }
                                tracing::info!("[DeviceCam] LAN direct: peer is on the same subnet");
                            }
                        }

                        if let Some(Protocol::Ip4(ip)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                            if ip.is_private() {
                                tracing::warn!("[DeviceCam] WARNING: Observed address is private IP ({}) - DCUtR may fail!", ip);
                            } else {
                                tracing::info!("[DeviceCam] Observed address is public IP ({}) - good for DCUtR", ip);
                            }
                        }
                    }

                    SwarmEvent::ListenerClosed { listener_id, reason: Err(e), .. } => {
                        if let Some(state) = relay_states.iter_mut().find(|s| s.reservation_id == Some(listener_id)) {
                            println!("[DeviceCam] *** Relay {} reservation lost! ***", state.peer_id);
                            tracing::warn!("[DeviceCam] Relay reservation lost: {e}");
                            state.reservation_id = None;
                            state.connected = false;
                            state.dial_pending = false;
                            if swarm.is_connected(&state.peer_id) {
                                tracing::info!("[DeviceCam] Relay {} still connected, re-requesting...", state.peer_id);
                                match swarm.listen_on(state.addr.clone().with(Protocol::P2pCircuit)) {
                                    Ok(new_id) => {
                                        state.reservation_id = Some(new_id);
                                        println!("[DeviceCam] Re-requesting relay {} reservation...", state.peer_id);
                                    }
                                    Err(e) => {
                                        tracing::error!("[DeviceCam] Failed to re-request reservation: {e}");
                                    }
                                }
                            }
                        }
                    }

                    SwarmEvent::NewListenAddr { address, .. } => {
                        println!("[DeviceCam] Listening on: {address}");
                        let is_quic = address.iter().any(|p| matches!(p, Protocol::QuicV1));
                        let is_relayed = address.iter().any(|p| matches!(p, Protocol::P2pCircuit));
                        if is_quic && !is_relayed {
                            if let Some(Protocol::Ip4(ip)) = address.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                                if !ip.is_loopback() && !ip.is_unspecified() {
                                    swarm.add_external_address(address.clone());
                                    tracing::info!("[DeviceCam] Added local address as DCUtR candidate: {address}");
                                    if !local_ips.contains(&ip) {
                                        local_ips.push(ip);
                                    }
                                    if let Some(Protocol::Udp(port)) = address.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                                        local_quic_port = port;
                                    }
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
                        peer_conn_type.insert(peer_id, conn_type.clone());

                        let is_relay = relay_states.iter().any(|s| s.peer_id == peer_id);
                        if is_relay {
                            if let Some(state) = relay_states.iter_mut().find(|s| s.peer_id == peer_id) {
                                if state.reconnect_attempt > 0 {
                                    println!("[DeviceCam] *** Relay {} reconnected! (was attempt #{}) ***", peer_id, state.reconnect_attempt);
                                } else {
                                    println!("[DeviceCam] Connected to relay {peer_id}");
                                }
                                state.reconnect_attempt = 0;
                                state.dial_pending = false;
                                state.reconnect_timer = None;
                                if state.reservation_id.is_none() {
                                    match swarm.listen_on(state.addr.clone().with(Protocol::P2pCircuit)) {
                                        Ok(new_id) => {
                                            state.reservation_id = Some(new_id);
                                            println!("[DeviceCam] Requesting relay {} reservation...", peer_id);
                                        }
                                        Err(e) => {
                                            tracing::error!("[DeviceCam] Failed to request reservation: {e}");
                                        }
                                    }
                                }
                            }
                        } else {
                            println!("[DeviceCam] *** Viewer connected: {peer_id} via {conn_type} ***");
                            if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                                if let Some(ref diag) = nat_diagnostic {
                                    let prediction = diag.dcutr_prediction();
                                    if prediction.likely_success {
                                        tracing::info!("[DeviceCam] DCUtR prediction: likely SUCCESS - {}", prediction.reason);
                                    } else {
                                        tracing::warn!("[DeviceCam] DCUtR prediction: likely FAIL - {}", prediction.reason);
                                    }

                                    // 连接策略日志：输出本端 NAT 类型和 DCUtR 策略
                                    let (strategy, reason) = diag.connection_strategy();
                                    tracing::info!("[DeviceCam] Connection strategy: {} - {}", strategy.name(), reason);
                                }
                            }
                        }
                        tracing::info!("[DeviceCam] Connection established: peer={peer_id} role={role} addr={addr} type={conn_type}");
                    }

                    SwarmEvent::ConnectionClosed { peer_id, endpoint: _, cause, num_established, .. } => {
                        let duration = connection_times.remove(&peer_id)
                            .map(|t| t.elapsed())
                            .map(|d| format!("{:.1}s", d.as_secs_f64()))
                            .unwrap_or_else(|| "unknown".to_string());
                        let conn_type = peer_conn_type.get(&peer_id).cloned().unwrap_or_else(|| "Unknown".to_string());
                        if num_established == 0 {
                            peer_conn_type.remove(&peer_id);
                        }
                        tracing::warn!("[DeviceCam] Connection closed: peer={peer_id} duration={duration} type={conn_type} remaining={num_established}");
                        if let Some(cause) = cause {
                            tracing::warn!("  - Cause: {cause}");
                        }

                        let is_relay = relay_states.iter().any(|s| s.peer_id == peer_id);
                        if is_relay && num_established == 0 {
                            if let Some(state) = relay_states.iter_mut().find(|s| s.peer_id == peer_id) {
                                state.connected = false;
                                state.reservation_id = None;
                                state.dial_pending = false;
                                state.reconnect_attempt += 1;
                                let delay = state.reconnect_delay();
                                println!("[DeviceCam] *** Relay {} connection lost! Reconnecting in {}s (attempt {}) ***",
                                    peer_id, delay.as_secs(), state.reconnect_attempt);
                                state.reconnect_timer = Some(Box::pin(tokio::time::sleep(delay)));
                            }
                        }
                        if !is_relay && num_established == 0 {
                            println!("[DeviceCam] *** Viewer disconnected: {peer_id} (was {conn_type}) ***");
                        }
                    }

                    SwarmEvent::OutgoingConnectionError { peer_id: Some(peer_id), error, .. } => {
                        let is_relay = relay_states.iter().any(|s| s.peer_id == peer_id);
                        if is_relay {
                            if let Some(state) = relay_states.iter_mut().find(|s| s.peer_id == peer_id) {
                                state.dial_pending = false;
                                state.reconnect_attempt += 1;
                                let delay = state.reconnect_delay();
                                println!("[DeviceCam] *** Failed to connect to relay {}! Retrying in {}s (attempt {}) ***",
                                    peer_id, delay.as_secs(), state.reconnect_attempt);
                                tracing::warn!("[DeviceCam] Relay connection error: {error}");
                                state.reconnect_timer = Some(Box::pin(tokio::time::sleep(delay)));
                            }
                        } else {
                            tracing::warn!("[DeviceCam] Outgoing connection error: {error}");
                        }
                    }

                    SwarmEvent::OutgoingConnectionError { error, .. } => {
                        tracing::warn!("[DeviceCam] Outgoing connection error: {error}");
                    }

                    _ => {
                        tracing::debug!("Event: {:?}", event);
                    }
                }
            }

            // 主码流请求
            main_video = incoming_main.next() => {
                if let Some((peer_id, stream)) = main_video {
                    let conn_type = peer_conn_type.get(&peer_id).map(|s| s.as_str()).unwrap_or("Unknown");
                    println!("[DeviceCam] New MAIN video viewer: {peer_id} via {conn_type}");
                    // 同 peer 同路流去重: 新请求到达时先 abort 旧任务, 避免多路流打架
                    let key = (peer_id, "main");
                    match active_streams.get(&key).map(|h| h.is_finished()) {
                        Some(true) => { active_streams.remove(&key); }
                        Some(false) => { active_streams.get(&key).unwrap().abort(); }
                        None => {}
                    }
                    let rx = main_tx.subscribe();
                    let handle = tokio::spawn(stream_video_to_viewer(peer_id, stream, rx, "main"));
                    active_streams.insert(key, handle);
                }
            }

            // 子码流请求
            sub_video = incoming_sub.next() => {
                if let Some((peer_id, stream)) = sub_video {
                    let conn_type = peer_conn_type.get(&peer_id).map(|s| s.as_str()).unwrap_or("Unknown");
                    println!("[DeviceCam] New SUB video viewer: {peer_id} via {conn_type}");
                    let key = (peer_id, "sub");
                    match active_streams.get(&key).map(|h| h.is_finished()) {
                        Some(true) => { active_streams.remove(&key); }
                        Some(false) => { active_streams.get(&key).unwrap().abort(); }
                        None => {}
                    }
                    let rx = sub_tx.subscribe();
                    let handle = tokio::spawn(stream_video_to_viewer(peer_id, stream, rx, "sub"));
                    active_streams.insert(key, handle);
                }
            }

            // 第三码流请求
            third_video = incoming_third.next() => {
                if let Some((peer_id, stream)) = third_video {
                    let conn_type = peer_conn_type.get(&peer_id).map(|s| s.as_str()).unwrap_or("Unknown");
                    println!("[DeviceCam] New THIRD video viewer: {peer_id} via {conn_type}");
                    let key = (peer_id, "third");
                    match active_streams.get(&key).map(|h| h.is_finished()) {
                        Some(true) => { active_streams.remove(&key); }
                        Some(false) => { active_streams.get(&key).unwrap().abort(); }
                        None => {}
                    }
                    let rx = third_tx.subscribe();
                    let handle = tokio::spawn(stream_video_to_viewer(peer_id, stream, rx, "third"));
                    active_streams.insert(key, handle);
                }
            }

            // 音频
            audio = incoming_audio.next() => {
                if let Some((peer_id, stream)) = audio {
                    let conn_type = peer_conn_type.get(&peer_id).map(|s| s.as_str()).unwrap_or("Unknown");
                    println!("[DeviceCam] New audio viewer: {peer_id} via {conn_type}");
                    let key = (peer_id, "audio");
                    match active_streams.get(&key).map(|h| h.is_finished()) {
                        Some(true) => { active_streams.remove(&key); }
                        Some(false) => { active_streams.get(&key).unwrap().abort(); }
                        None => {}
                    }
                    let rx = audio_tx.subscribe();
                    let handle = tokio::spawn(stream_audio_to_viewer(peer_id, stream, rx));
                    active_streams.insert(key, handle);
                } else {
                    tracing::error!("[DeviceCam] Audio stream accept channel closed");
                }
            }

            // 控制通道
            control = incoming_control.next() => {
                if let Some((peer_id, stream)) = control {
                    let conn_type = peer_conn_type.get(&peer_id).map(|s| s.as_str()).unwrap_or("Unknown");
                    println!("[DeviceCam] New control connection: {peer_id} via {conn_type}");
                    tokio::spawn(control_handler::handle_control_stream(peer_id, stream));
                } else {
                    tracing::error!("[DeviceCam] Control stream accept channel closed");
                }
            }

            // Relay 重连定时器（非阻塞：timer 到期时执行 dial，未到期时其他分支正常轮询）
            _ = async {
                // 找到最早到期的 relay 重连 timer
                let timer = relay_states.iter_mut()
                    .filter_map(|s| s.reconnect_timer.as_mut())
                    .min_by_key(|t| t.deadline());
                if let Some(t) = timer {
                    t.await;
                } else {
                    // 没有活跃的 timer，用无限期 future 让此分支永远不被选中
                    std::future::pending::<()>().await;
                }
            } => {
                // 找到到期的 relay 并执行 dial
                for state in &mut relay_states {
                    if let Some(ref mut timer) = state.reconnect_timer {
                        if timer.is_elapsed() {
                            state.reconnect_timer = None;
                            match swarm.dial(state.addr.clone()) {
                                Ok(()) => {
                                    println!("[DeviceCam] Dialing relay {} (attempt {})...", state.peer_id, state.reconnect_attempt);
                                    state.dial_pending = true;
                                }
                                Err(e) => {
                                    tracing::error!("[DeviceCam] Failed to dial relay {}: {e}", state.peer_id);
                                    state.reconnect_attempt += 1;
                                    let delay = state.reconnect_delay();
                                    state.reconnect_timer = Some(Box::pin(tokio::time::sleep(delay)));
                                }
                            }
                            break;
                        }
                    }
                }
            }
        }
    }
}

/// 将 config::StreamConfig 转换为 rk_video_source::StreamParams
#[cfg(feature = "rv1106")]
fn stream_config_to_params(s: &config::StreamConfig) -> rk_video_source::StreamParams {
    rk_video_source::StreamParams {
        codec: s.codec.clone(),
        width: s.width,
        height: s.height,
        src_fps_num: s.src_frame_rate_num,
        src_fps_den: s.src_frame_rate_den,
        dst_fps_num: s.dst_frame_rate_num,
        dst_fps_den: s.dst_frame_rate_den,
        bitrate_kbps: s.bitrate_kbps,
        rc_mode: s.rc_mode.clone(),
        rc_quality: s.rc_quality.clone(),
        gop: s.gop,
        gop_mode: s.gop_mode.clone(),
        h264_profile: s.h264_profile.clone(),
        smartp_viridrlen: s.smartp_viridrlen,
        stream_buf_cnt: s.stream_buf_cnt,
        mirror: s.mirror.clone(),
    }
}

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

async fn stream_video_to_viewer(
    peer_id: PeerId,
    mut stream: libp2p::swarm::Stream,
    mut source: broadcast::Receiver<MediaPacket>,
    stream_name: &str,
) {
    let mut frame_count: u64 = 0;
    let mut bytes_sent: u64 = 0;
    let mut video_start: Option<std::time::Instant> = None;
    let mut lagged_count: u64 = 0;
    let mut dropped_count: u64 = 0;
    // peer_id 短标识，用于日志区分不同 viewer
    let peer_short = peer_id.to_string();
    let peer_short = &peer_short[peer_short.len().saturating_sub(7)..];
    // write_all+flush 耗时超过此阈值则输出警告
    const WRITE_SLOW_THRESHOLD_MS: u64 = 100;
    // write_all+flush 超过此阈值则视为 DCUtR 握手干扰（yamux 竞争导致写堵塞 500ms+）
    const WRITE_DCUTR_STALL_THRESHOLD_MS: u64 = 500;
    // write_all+flush 超过此时间视为写入受阻。注意：relay 电路在跨网(4G/公网)场景下
    // 偶发 3-5s 的慢写是正常的，阈值必须留足余量，否则会误判为断连。
    // 旧值 5s 偏小，relay 拥塞时单帧写超 5s 就触发断连→viewer 重连风暴，反而更卡。
    const WRITE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

    // 新 Viewer 接入时主动请求一帧 IDR：
    // 若连接时编码器正处于 GOP 中段，broadcast 里先到的会是 P 帧，viewer
    // 缺少参考帧无法解码，需等到下一个 GOP（约 2s@gop=50）才起播。
    // 主动请求 IDR 让编码器立即产出关键帧，显著缩短起播延迟。
    #[cfg(feature = "rv1106")]
    {
        let chn = match stream_name {
            "main" | "main(legacy)" => 0u8,
            "sub" => 1,
            "third" => 2,
            _ => 0,
        };
        rk_video_source::request_idr(chn);
        println!("[DeviceCam] Requested IDR for new viewer ..{peer_short} ({stream_name}, chn={chn})");
    }

    loop {
        match source.recv().await {
            Ok(packet) => {
                let encoded = packet.encode();
                let write_start = std::time::Instant::now();

                // 带超时的写入：防止慢 viewer 阻塞帧发送
                let write_result = tokio::time::timeout(WRITE_TIMEOUT, async {
                    stream.write_all(&encoded).await?;
                    stream.flush().await?;
                    Ok::<(), std::io::Error>(())
                }).await;

                match write_result {
                    Ok(Ok(())) => {
                        let write_ms = write_start.elapsed().as_millis() as u64;
                        if write_ms > WRITE_SLOW_THRESHOLD_MS {
                            let cause = if write_ms > WRITE_DCUTR_STALL_THRESHOLD_MS {
                                " (likely relay congestion / peer DCUtR handshake interference)"
                            } else {
                                " (relay congestion or backpressure)"
                            };
                            println!("[DeviceCam] SLOW write ..{peer_short}: {write_ms}ms frame #{frame_count} ({stream_name}, {} bytes){cause}",
                                encoded.len());
                        }

                        frame_count += 1;
                        bytes_sent += encoded.len() as u64;

                        if video_start.is_none() {
                            video_start = Some(std::time::Instant::now());
                        }

                        if frame_count % 100 == 0 {
                            if let Some(start) = video_start {
                                let elapsed = start.elapsed().as_secs_f64();
                                let fps = frame_count as f64 / elapsed;
                                let kbps = (bytes_sent * 8) as f64 / elapsed / 1000.0;
                                // 注意: cam 端已不再计算 keyframe 标志(见 rk_video_source.rs),
                                // 关键帧判定改由 viewer 侧字节扫描完成, 故此处不再显示 [I] 标记。
                                println!(
                                    "[DeviceCam] frame #{} | {:.1} fps | {:.0} kbps | ts={} | ..{peer_short} {stream_name}",
                                    frame_count, fps, kbps, packet.timestamp_ms
                                );
                            }
                        }
                    }
                    Ok(Err(e)) => {
                        println!("[DeviceCam] Write to ..{peer_short} failed: {e} ({stream_name}, frame #{frame_count})");
                        break;
                    }
                    Err(_) => {
                        // 写入超时：relay 拥塞或 viewer 暂忙（如 DCUtR 打洞占用 4G 上行）。
                        // 关键修复：不再断开连接！断开会触发 viewer 重连风暴(每 16-25s 重连一次，
                        // 每次只传几帧)，viewer 平均仅 2-3fps。改为丢弃本帧并继续，连接保持，
                        // relay 通畅后写入自动恢复。真正的断连由 Ok(Err(e)) 分支(流已死)处理。
                        dropped_count += 1;
                        println!("[DeviceCam] WRITE TIMEOUT ..{peer_short}: write blocked >{}s, DROPPING frame (keep connection) ({stream_name}, frame #{frame_count}, dropped: {dropped_count})",
                            WRITE_TIMEOUT.as_secs());
                        continue;
                    }
                }
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                lagged_count += n;
                println!("[DeviceCam] LAGGED ..{peer_short} {stream_name}: {n} frames dropped (total lagged: {lagged_count}, sent: {frame_count})");
                #[cfg(feature = "rv1106")]
                {
                    // Request IDR for appropriate channel based on stream name
                    let chn = match stream_name {
                        "main" | "main(legacy)" => 0u8,
                        "sub" => 1,
                        "third" => 2,
                        _ => 0,
                    };
                    rk_video_source::request_idr(chn);
                }
            }
            Err(broadcast::error::RecvError::Closed) => {
                println!("[DeviceCam] Broadcast closed for ..{peer_short} after {frame_count} frames ({stream_name})");
                break;
            }
        }
    }
    let _ = stream.close().await;
    println!("[DeviceCam] Video stream ({stream_name}) to ..{peer_short} ended ({frame_count} frames sent, {lagged_count} lagged, {dropped_count} dropped)");

    // Summary
    println!("[DeviceCam] === Summary ({stream_name} -> ..{peer_short}) ===");
    println!("[DeviceCam] Total frames: {frame_count}");
    println!("[DeviceCam] Total bytes: {bytes_sent}");
    println!("[DeviceCam] Total lagged: {lagged_count}");
    println!("[DeviceCam] Total dropped: {dropped_count}");
    if let Some(start) = video_start {
        let elapsed = start.elapsed().as_secs_f64();
        println!("[DeviceCam] Duration: {elapsed:.1}s");
        println!("[DeviceCam] Avg fps: {:.1}", frame_count as f64 / elapsed);
        println!("[DeviceCam] Avg bitrate: {:.0} kbps", (bytes_sent * 8) as f64 / elapsed / 1000.0);
    } else {
        println!("[DeviceCam] Duration: N/A");
        println!("[DeviceCam] Avg fps: N/A");
        println!("[DeviceCam] Avg bitrate: N/A");
    }
}

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

fn broadcast_sender_to_crossbeam(tx: broadcast::Sender<MediaPacket>) -> Sender<MediaPacket> {
    let (c_tx, c_rx) = crossbeam_channel::bounded::<MediaPacket>(BROADCAST_CAPACITY);
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
    #[arg(long, default_value = "device-cam.toml")]
    config: PathBuf,

    #[arg(long = "relay")]
    relays: Vec<String>,

    #[arg(long)]
    enable_mdns: Option<bool>,

    #[arg(long)]
    mode: Option<String>,

    #[arg(long)]
    key_file: Option<PathBuf>,

    #[cfg(not(feature = "rv1106"))]
    #[arg(long)]
    video_file: Option<std::path::PathBuf>,

    #[arg(long, default_value_t = false)]
    enable_audio: bool,

    #[arg(long)]
    udp_port: Option<u16>,
}

fn validate_device_cam_config(config: &config::Config) {
    for (i, relay_str) in config.relays.iter().enumerate() {
        let label = if config.relays.len() == 1 {
            "Relay".to_string()
        } else {
            format!("Relay #{}", i + 1)
        };
        if relay_str.contains("/tcp/") && !relay_str.contains("/quic-v1") {
            tracing::warn!("[DeviceCam] WARNING: {label} using TCP - DCUtR will only produce TCP candidates");
        } else if relay_str.contains("/quic-v1") {
            tracing::info!("[DeviceCam] {label} protocol: QUIC - good for DCUtR hole punching");
        }
    }

    if let Some(port) = config.udp_port {
        if port == 0 {
            tracing::warn!("[DeviceCam] WARNING: Using random UDP port - cannot configure port forwarding for DCUtR");
        }
    }

    if config.enable_mdns {
        tracing::info!("[DeviceCam] mDNS enabled - LAN discovery active");
    } else {
        tracing::info!("[DeviceCam] mDNS disabled");
    }

    // 打印三码流状态
    println!("[DeviceCam] Stream config:");
    println!("  Main:  {} ({}x{} @{}/{}fps {}kbps {})",
        if config.video.main.enabled { "ON" } else { "OFF" },
        config.video.main.width, config.video.main.height,
        config.video.main.dst_frame_rate_num, config.video.main.dst_frame_rate_den,
        config.video.main.bitrate_kbps, config.video.main.codec);
    println!("  Sub:   {} ({}x{} @{}/{}fps {}kbps {})",
        if config.video.sub.enabled { "ON" } else { "OFF" },
        config.video.sub.width, config.video.sub.height,
        config.video.sub.dst_frame_rate_num, config.video.sub.dst_frame_rate_den,
        config.video.sub.bitrate_kbps, config.video.sub.codec);
    println!("  Third: {} ({}x{} @{}/{}fps {}kbps {})",
        if config.video.third.enabled { "ON" } else { "OFF" },
        config.video.third.width, config.video.third.height,
        config.video.third.dst_frame_rate_num, config.video.third.dst_frame_rate_den,
        config.video.third.bitrate_kbps, config.video.third.codec);

    // 打印音频状态
    println!("[DeviceCam] Audio: {} ({}Hz, {}ch, {}samples/frame, card={}, fmt={}, encode={}, bitrate={}, vqe={})",
        if config.audio.enabled { "ON" } else { "OFF" },
        config.audio.sample_rate,
        config.audio.channels,
        config.audio.frame_size,
        config.audio.card_name,
        config.audio.format,
        config.audio.encode_type,
        config.audio.bit_rate,
        config.audio.enable_vqe);
}

fn is_same_subnet(a: Ipv4Addr, b: Ipv4Addr) -> bool {
    let a = u32::from(a);
    let b = u32::from(b);
    (a & 0xFFFFFF00) == (b & 0xFFFFFF00)
}
