//! Media Viewer — 端到端测试工具
//!
//! 用法:
//!   cargo run --example media_viewer -- \
//!     --relay /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER> \
//!     --camera <DEVICE_CAM_PEER_ID> \
//!     --output output.h265
//!
//! 实时播放 (需 --features player):
//!   cargo build --example media_viewer --features player
//!   media_viewer --relay ... --camera ... --play
//!
//! 自动重连: 连接断开时自动重新连接 Relay + DeviceCam + 打开 stream，
//!           播放器和输出文件持续运行不中断。
//!
//! 多 Relay + mDNS 支持:
//!   --relay 可多次使用: --relay /ip4/.../p2p/A --relay /ip4/.../p2p/B
//!   --enable-mdns (默认 true): 启用 mDNS 局域网发现，优先于 Relay
//!
//! 验证流程:
//!   1. 同时拨号所有 Relay Server
//!   2. 如果启用 mDNS，并行监听局域网发现事件 (5 秒超时)
//!   3. mDNS 发现目标 DeviceCam 时，优先使用 LAN 直连
//!   4. 否则通过第一个成功连接的 Relay circuit 拨号 DeviceCam
//!   5. 打开视频 stream 接收帧
//!   6. 保存到文件 (可用 ffplay 播放) 或 SDL 实时播放 (--play, 需 --features player)
//!   7. 打印接收统计

use std::net::Ipv4Addr;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use clap::Parser;
use futures::{AsyncReadExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, identify, mdns, noise,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, StreamProtocol, PeerId,
};
use mobile_core::net_diag::{NatDiagnostic, NatType};
use mobile_core::viewer::{
    get_video_protocol, is_same_subnet, ViewerBehaviour, ViewerBehaviourEvent,
    RECONNECT_DELAY, MDNS_DISCOVERY_TIMEOUT, STREAM_READ_BUF,
};
use proto::{media_packet::{MediaPacket, MediaTrack}, stream_protocols};
use serde::{Deserialize, Serialize};
use tokio::sync::{mpsc, oneshot};
use tracing_subscriber::EnvFilter;

/// 根据连接方式和 stream_type 参数决定使用哪个码流协议
///
/// "auto" 模式：转发→子码流，直连→主码流
/// 手动模式：直接使用用户指定的码流
fn resolve_stream_protocol(stream_type: &str, is_relay: bool) -> StreamProtocol {
    match stream_type {
        "auto" => {
            if is_relay {
                stream_protocols::VIDEO_SUB_PROTOCOL
            } else {
                stream_protocols::VIDEO_MAIN_PROTOCOL
            }
        }
        other => get_video_protocol(other),
    }
}

// SDL2 要求事件循环在主线程, 使用 current_thread runtime
#[cfg_attr(feature = "player", tokio::main(flavor = "current_thread"))]
#[cfg_attr(not(feature = "player"), tokio::main)]
async fn main() -> Result<()> {
    println!("[Viewer] p2p-camera media-viewer v{} ({})", env!("CARGO_PKG_VERSION"), env!("BUILD_TIME"));

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let opt = Opt::parse();

    // ---- 加载配置文件 ----
    let mut config = ViewerConfig::load(&opt.config).unwrap_or_else(|e| {
        eprintln!("[Viewer] {e}");
        std::process::exit(1);
    });

    // 命令行参数覆盖配置文件
    if !opt.relays.is_empty() { config.relays = opt.relays.clone(); }
    if let Some(ref camera) = opt.camera { config.camera = camera.clone(); }
    if let Some(ref output) = opt.output { config.output = Some(output.clone()); }
    if opt.no_audio { config.no_audio = true; }
    #[cfg(feature = "player")]
    if opt.play { config.play = true; }
    if let Some(udp_port) = opt.udp_port { config.udp_port = Some(udp_port); }
    if let Some(enable_mdns) = opt.enable_mdns { config.enable_mdns = enable_mdns; }
    // stream CLI arg always overrides config (unless it's the default "auto")
    if opt.stream != "auto" { config.stream = opt.stream.clone(); }

    // 解析 relays (兼容旧格式 relay 字段)
    config.resolve_relays();

    // ---- 参数校验 ----
    if config.relays.is_empty() && !config.enable_mdns {
        eprintln!("[Viewer] Error: no relay addresses and mDNS is disabled. Edit {} or use --relay / --enable-mdns", opt.config.display());
        std::process::exit(1);
    }
    if config.camera.is_empty() {
        eprintln!("[Viewer] Error: camera PeerId is empty. Edit {} or use --camera", opt.config.display());
        std::process::exit(1);
    }
    {
        for (i, relay_str) in config.relays.iter().enumerate() {
            let label = if config.relays.len() == 1 { "Relay".to_string() } else { format!("Relay #{}", i + 1) };
            if relay_str.contains("/tcp/") && !relay_str.contains("/quic-v1") {
                tracing::warn!("[Viewer] WARNING: {label} using TCP - DCUtR will only produce TCP candidates, hole punching unlikely to succeed. Use /udp/<port>/quic-v1 instead");
            } else if relay_str.contains("/quic-v1") {
                tracing::info!("[Viewer] {label} protocol: QUIC - good for DCUtR hole punching");
            }
        }

        if let Some(port) = config.udp_port {
            if port == 0 {
                tracing::warn!("[Viewer] WARNING: Using random UDP port - cannot configure port forwarding for DCUtR");
            }
        }
    }

    if config.enable_mdns {
        tracing::info!("[Viewer] mDNS enabled - LAN discovery active");
    } else {
        tracing::info!("[Viewer] mDNS disabled");
    }

    // ---- 初始化播放器/输出 (独立于 P2P 连接，重连期间不中断) ----
    #[cfg(feature = "player")]
    let mut player = if config.play {
        println!("[Viewer] Initializing SDL player...");
        Some(player::VideoPlayer::new()?)
    } else {
        None
    };

    #[cfg(feature = "player")]
    let mut audio_player = if config.play && !config.no_audio {
        match player::AudioPlayer::new(16000) {
            Ok(p) => Some(p),
            Err(e) => {
                println!("[Viewer] Audio player init failed (non-fatal): {e}");
                None
            }
        }
    } else {
        None
    };

    let mut output_file = if let Some(path) = &config.output {
        Some(std::fs::File::create(path).context("Failed to create output file")?)
    } else {
        None
    };

    // ---- 持久化 channel (重连时复用，不重建) ----
    let (tx, mut rx) = mpsc::channel::<MediaPacket>(60);
    let (audio_tx, mut audio_rx) = mpsc::channel::<MediaPacket>(60);

    // 用于与后台 session 通信
    let (session_tx, mut session_rx) = mpsc::channel::<SessionEvent>(1);

    let relay_addrs = config.relays.clone();
    let device_cam_str = config.camera.clone();
    let no_audio = config.no_audio;
    let udp_port = config.udp_port;
    let enable_mdns = config.enable_mdns;
    let stream_type = config.stream.clone();
    let network_type = config.network_type.clone();

    let mut frame_count: u64 = 0;
    let mut bytes_received: u64 = 0;
    let mut audio_count: u64 = 0;
    let mut direct_upgraded = false;
    let mut direct_via_lan = false;
    let mut local_nat_type: Option<NatType> = None;
    let mut remote_nat_hint: Option<String> = None;
    let mut video_start: Option<std::time::Instant> = None;
    let mut stream_disconnected = false;
    let mut window_minimized = false;
    #[allow(unused_assignments)]
    let mut session_abort_handle: Option<tokio::task::AbortHandle> = None;
    // 最小化时用于通知当前 session 优雅关闭连接的发送端
    let mut session_shutdown_tx: Option<oneshot::Sender<()>> = None;
    // DCUtR 是否启用：默认启用（锥形/EIM NAT 可打洞成功，省中继带宽）。
    // 一旦会话内 net_diag 确认为 Symmetric NAT，后续重连传入 false 以跳过无效打洞。
    let mut enable_dcutr = true;

    // 持久化密钥对：重连时复用，保证 PeerId 不变（与生产 lib 侧 MediaPlayer 行为一致）
    let keypair = libp2p::identity::Keypair::generate_ed25519();
    println!("[Viewer] Local PeerId: {}", keypair.public().to_peer_id());

    // 启动初始 session (后台任务)
    let (ah, stx) = spawn_session(
        relay_addrs.clone(),
        device_cam_str.clone(),
        no_audio,
        udp_port,
        enable_mdns,
        stream_type.clone(),
        network_type.clone(),
        enable_dcutr,
        keypair.clone(),
        tx.clone(),
        audio_tx.clone(),
        session_tx.clone(),
    );
    session_abort_handle = Some(ah);
    session_shutdown_tx = Some(stx);

    println!("[Viewer] Receiving video frames... (Ctrl+C to stop)");

    // ---- 主循环: 消费帧 + 监控 session 状态 + 触发重连 ----
    loop {
        tokio::select! {
            // Session 事件 (断开/直连升级)
            session_event = session_rx.recv() => {
                match session_event {
                    Some(SessionEvent::Disconnected { reason }) => {
                        // 忽略重复的 Disconnected 事件
                        if stream_disconnected {
                            continue;
                        }
                        eprintln!("[Viewer] Session disconnected: {reason}");
                        stream_disconnected = true;
                        #[cfg(not(feature = "player"))]
                        {
                            eprintln!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
                            tokio::time::sleep(RECONNECT_DELAY).await;
                            reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                            drain_channel(&mut rx, &mut frame_count, &mut bytes_received,
                                &mut video_start, &mut output_file,
                                #[cfg(feature = "player")]
                                player.as_mut());
                            drain_audio_channel(&mut audio_rx, &mut audio_count,
                                #[cfg(feature = "player")]
                                audio_player.as_mut());
                            let (ah, stx) = spawn_session(
                                relay_addrs.clone(),
                                device_cam_str.clone(),
                                no_audio,
                                udp_port,
                                enable_mdns,
                                stream_type.clone(),
                                network_type.clone(),
                                enable_dcutr,
                                keypair.clone(),
                                tx.clone(),
                                audio_tx.clone(),
                                session_tx.clone(),
                            );
                            session_abort_handle = Some(ah);
                            session_shutdown_tx = Some(stx);
                            stream_disconnected = false;
                        }
                        #[cfg(feature = "player")]
                        {
                            // 先轮询 SDL 事件，检查窗口是否已最小化
                            // 避免在窗口最小化时重连（浪费资源）
                            if let Some(p) = player.as_mut() {
                                let action = p.poll_events();
                                if let RenderAction::WindowMinimized = action {
                                    window_minimized = true;
                                } else if let RenderAction::Quit = action {
                                    break;
                                }
                            }
                            if !window_minimized {
                                eprintln!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
                                tokio::time::sleep(RECONNECT_DELAY).await;
                                // 先消费残留帧，再 flush 解码器
                                reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                                drain_channel(&mut rx, &mut frame_count, &mut bytes_received,
                                    &mut video_start, &mut output_file,
                                    player.as_mut());
                                drain_audio_channel(&mut audio_rx, &mut audio_count,
                                    audio_player.as_mut());
                                // drain 完成后再 flush 解码器，设置 waiting_for_keyframe
                                if let Some(p) = player.as_mut() {
                                    p.reset_decoder();
                                }
                                let (ah, stx) = spawn_session(
                                    relay_addrs.clone(),
                                    device_cam_str.clone(),
                                    no_audio,
                                    udp_port,
                                    enable_mdns,
                                    stream_type.clone(),
                                    network_type.clone(),
                                    enable_dcutr,
                                    keypair.clone(),
                                    tx.clone(),
                                    audio_tx.clone(),
                                    session_tx.clone(),
                                );
                                session_abort_handle = Some(ah);
                                session_shutdown_tx = Some(stx);
                                stream_disconnected = false;
                            } else {
                                eprintln!("[Viewer] Window is minimized, will reconnect on restore");
                            }
                        }
                    }
                    Some(SessionEvent::StreamEOF { reason }) => {
                        // 忽略重复的 StreamEOF（视频和音频 stream 各发一个）
                        if stream_disconnected {
                            continue;
                        }
                        tracing::warn!("[Viewer] Stream EOF: {reason}");
                        stream_disconnected = true;
                        #[cfg(not(feature = "player"))]
                        {
                            eprintln!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
                            tokio::time::sleep(RECONNECT_DELAY).await;
                            reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                            drain_channel(&mut rx, &mut frame_count, &mut bytes_received,
                                &mut video_start, &mut output_file,
                                #[cfg(feature = "player")]
                                player.as_mut());
                            drain_audio_channel(&mut audio_rx, &mut audio_count,
                                #[cfg(feature = "player")]
                                audio_player.as_mut());
                            let (ah, stx) = spawn_session(
                                relay_addrs.clone(),
                                device_cam_str.clone(),
                                no_audio,
                                udp_port,
                                enable_mdns,
                                stream_type.clone(),
                                network_type.clone(),
                                enable_dcutr,
                                keypair.clone(),
                                tx.clone(),
                                audio_tx.clone(),
                                session_tx.clone(),
                            );
                            session_abort_handle = Some(ah);
                            session_shutdown_tx = Some(stx);
                            stream_disconnected = false;
                        }
                        #[cfg(feature = "player")]
                        {
                            // 先轮询 SDL 事件，检查窗口是否已最小化
                            if let Some(p) = player.as_mut() {
                                let action = p.poll_events();
                                if let RenderAction::WindowMinimized = action {
                                    window_minimized = true;
                                } else if let RenderAction::Quit = action {
                                    break;
                                }
                            }
                            if !window_minimized {
                                eprintln!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
                                tokio::time::sleep(RECONNECT_DELAY).await;
                                // 先消费残留帧，再 flush 解码器
                                reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                                drain_channel(&mut rx, &mut frame_count, &mut bytes_received,
                                    &mut video_start, &mut output_file,
                                    player.as_mut());
                                drain_audio_channel(&mut audio_rx, &mut audio_count,
                                    audio_player.as_mut());
                                // drain 完成后再 flush 解码器，设置 waiting_for_keyframe
                                if let Some(p) = player.as_mut() {
                                    p.reset_decoder();
                                }
                                let (ah, stx) = spawn_session(
                                    relay_addrs.clone(),
                                    device_cam_str.clone(),
                                    no_audio,
                                    udp_port,
                                    enable_mdns,
                                    stream_type.clone(),
                                    network_type.clone(),
                                    enable_dcutr,
                                    keypair.clone(),
                                    tx.clone(),
                                    audio_tx.clone(),
                                    session_tx.clone(),
                                );
                                session_abort_handle = Some(ah);
                                session_shutdown_tx = Some(stx);
                                stream_disconnected = false;
                            } else {
                                eprintln!("[Viewer] Window is minimized, will reconnect on restore");
                            }
                        }
                    }
                    Some(SessionEvent::DirectUpgraded { via_lan }) => {
                        direct_upgraded = true;
                        direct_via_lan = via_lan;
                        let conn_type = if via_lan { "LAN direct (same subnet)" } else { "DCUtR hole punch" };
                        println!("[Viewer] *** Direct connection established: {conn_type} ***");
                    }
                    Some(SessionEvent::NatDiagnosis { local_nat, remote_nat }) => {
                        local_nat_type = Some(local_nat);
                        if remote_nat.is_some() {
                            remote_nat_hint = remote_nat;
                        }
                        // 对称型 NAT → 后续重连禁用 DCUtR（跳过无效打洞 ~17s）
                        enable_dcutr = local_nat != NatType::Symmetric;
                        tracing::info!(
                            "[Viewer] NAT type updated: {} → DCUtR {} for next reconnect",
                            local_nat.short_name(),
                            if enable_dcutr { "ENABLED" } else { "DISABLED" }
                        );
                    }
                    None => break, // channel 关闭 → 退出
                }
            }

            // 接收视频帧
            packet = rx.recv() => {
                let Some(packet) = packet else { continue; };
                match process_video_frame(
                    packet,
                    &mut frame_count,
                    &mut bytes_received,
                    &mut video_start,
                    &mut output_file,
                    #[cfg(feature = "player")]
                    player.as_mut(),
                ) {
                    RenderAction::Quit => break,
                    RenderAction::WindowMinimized => {
                        #[cfg(feature = "player")]
                        {
                            window_minimized = true;
                            if !stream_disconnected {
                                // 最小化时主动断开 stream，节约网络和 CPU 资源
                                // 恢复最大化时会重新连接，从关键帧开始，避免马赛克
                                eprintln!("[Viewer] Window minimized, aborting session to save resources");
                                stream_disconnected = true;
                                // 关键修复：仅 abort 会话任务(swarm)不够 —— receive_frames
                                // 任务仍持有子流句柄，会导致 QUIC 连接处理任务不退出、
                                // 连接不关闭，设备侧便持续推流（两路流打架）。
                                // 故通过 shutdown 通道让 run_viewer_session 显式
                                // abort receive_frames(释放子流) + disconnect_peer_id，
                                // 真正关闭连接，使设备侧 stream_video_to_viewer 的
                                // write_all 失败并停止推流。
                                if let Some(tx) = session_shutdown_tx.take() {
                                    let _ = tx.send(());
                                }
                                drain_channel(&mut rx, &mut frame_count, &mut bytes_received,
                                    &mut video_start, &mut output_file,
                                    player.as_mut());
                                drain_audio_channel(&mut audio_rx, &mut audio_count,
                                    audio_player.as_mut());
                            }
                        }
                    }
                    RenderAction::WindowRestored => {
                        #[cfg(feature = "player")]
                        {
                            window_minimized = false;
                            if stream_disconnected {
                                eprintln!("[Viewer] Window restored, reconnecting...");
                                // 先消费残留帧，再 flush 解码器
                                // 顺序很重要：如果先 flush 再 drain，残留帧中的 VPS/SPS/PPS
                                // 会清除 waiting_for_keyframe，导致新 session 的 P/B 帧被错误解码
                                reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                                drain_channel(&mut rx, &mut frame_count, &mut bytes_received,
                                    &mut video_start, &mut output_file,
                                    player.as_mut());
                                drain_audio_channel(&mut audio_rx, &mut audio_count,
                                    audio_player.as_mut());
                                // drain 完成后再 flush 解码器，设置 waiting_for_keyframe
                                if let Some(p) = player.as_mut() {
                                    p.reset_decoder();
                                }
                                let (ah, stx) = spawn_session(
                                    relay_addrs.clone(),
                                    device_cam_str.clone(),
                                    no_audio,
                                    udp_port,
                                    enable_mdns,
                                    stream_type.clone(),
                                    network_type.clone(),
                                    enable_dcutr,
                                    keypair.clone(),
                                    tx.clone(),
                                    audio_tx.clone(),
                                    session_tx.clone(),
                                );
                                session_abort_handle = Some(ah);
                                session_shutdown_tx = Some(stx);
                                stream_disconnected = false;
                            }
                        }
                    }
                    RenderAction::Continue => {}
                }
            }

            // 接收音频帧
            audio_packet = audio_rx.recv() => {
                if let Some(packet) = audio_packet {
                    audio_count += 1;
                    if audio_count == 1 {
                        println!("[Viewer] First audio frame: {} bytes, ts={}",
                            packet.data.len(), packet.timestamp_ms);
                    }
                    #[cfg(feature = "player")]
                    if let Some(ap) = &mut audio_player {
                        ap.write(&packet.data);
                    }
                    if audio_count % 250 == 0 {
                        println!("[Viewer] Audio: {} frames, last {} bytes, ts={}",
                            audio_count, packet.data.len(), packet.timestamp_ms);
                    }
                }
            }

            // 最小化时定期轮询 SDL 事件，检测窗口恢复
            // 因为最小化后 session 被 abort，没有新帧进来，
            // rx.recv() 会阻塞，导致 SDL 事件无法被轮询
            _ = tokio::time::sleep(std::time::Duration::from_millis(100)), if window_minimized => {
                #[cfg(feature = "player")]
                {
                    let action = player.as_mut().map(|p| p.poll_events());
                    match action {
                        Some(RenderAction::WindowRestored) => {
                            window_minimized = false;
                            if stream_disconnected {
                                eprintln!("[Viewer] Window restored (poll), reconnecting...");
                                // 先消费残留帧，再 flush 解码器
                                reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                                drain_channel(&mut rx, &mut frame_count, &mut bytes_received,
                                    &mut video_start, &mut output_file,
                                    player.as_mut());
                                drain_audio_channel(&mut audio_rx, &mut audio_count,
                                    audio_player.as_mut());
                                // drain 完成后再 flush 解码器，设置 waiting_for_keyframe
                                if let Some(p) = player.as_mut() {
                                    p.reset_decoder();
                                }
                                let (ah, stx) = spawn_session(
                                    relay_addrs.clone(),
                                    device_cam_str.clone(),
                                    no_audio,
                                    udp_port,
                                    enable_mdns,
                                    stream_type.clone(),
                                    network_type.clone(),
                                    enable_dcutr,
                                    keypair.clone(),
                                    tx.clone(),
                                    audio_tx.clone(),
                                    session_tx.clone(),
                                );
                                session_abort_handle = Some(ah);
                                session_shutdown_tx = Some(stx);
                                stream_disconnected = false;
                            }
                        }
                        Some(RenderAction::Quit) => break,
                        _ => {}
                    }
                }
            }
        }
    }

    // ---- Summary ----
    let video_elapsed = video_start.map(|s| s.elapsed().as_secs_f64()).unwrap_or(0.0);
    println!("\n[Viewer] === Summary ===");
    println!("[Viewer] Local NAT: {}", local_nat_type.map(|t| t.short_name()).unwrap_or("Unknown"));
    println!("[Viewer] Remote NAT: {}", remote_nat_hint.as_deref().unwrap_or("Unknown"));
    println!("[Viewer] Direct connection: {}", if direct_upgraded {
        if direct_via_lan { "YES (LAN direct, same subnet)" } else { "YES (DCUtR hole punched, no relay bandwidth)" }
    } else { "NO (relay circuit)" });
    println!("[Viewer] Total frames: {frame_count}");
    println!("[Viewer] Total bytes: {bytes_received}");
    if video_elapsed > 0.0 {
        println!("[Viewer] Duration: {video_elapsed:.1}s");
        println!("[Viewer] Avg fps: {:.1}", frame_count as f64 / video_elapsed);
        println!("[Viewer] Avg bitrate: {:.0} kbps", (bytes_received * 8) as f64 / video_elapsed / 1000.0);
    }

    if let Some(path) = &opt.output {
        println!("[Viewer] Output saved to: {}", path.display());
        println!("[Viewer] Play with: ffplay -f hevc {}", path.display());
    }

    Ok(())
}

// ---- Session 管理 ----

/// 后台 session 事件
#[derive(Debug)]
enum SessionEvent {
    /// 连接断开，需要重连
    Disconnected { reason: String },
    /// 流断开（EOF 或读错误），连接可能仍存活，窗口恢复时触发重连
    StreamEOF { reason: String },
    /// 直连建立 (DCUtR 或局域网)
    DirectUpgraded { via_lan: bool },
    /// NAT 类型诊断更新
    NatDiagnosis { local_nat: NatType, remote_nat: Option<String> },
}

/// 在后台启动一个 viewer session，返回 AbortHandle 用于取消 session
fn spawn_session(
    relay_addrs: Vec<String>,
    device_cam_str: String,
    no_audio: bool,
    udp_port: Option<u16>,
    enable_mdns: bool,
    stream_type: String,
    network_type: String,
    enable_dcutr: bool,
    keypair: libp2p::identity::Keypair,
    video_tx: mpsc::Sender<MediaPacket>,
    audio_tx: mpsc::Sender<MediaPacket>,
    event_tx: mpsc::Sender<SessionEvent>,
) -> (tokio::task::AbortHandle, oneshot::Sender<()>) {
    // 创建关闭信号通道：最小化时主循环发送 ()，run_viewer_session 据此优雅关流
    let (shutdown_tx, shutdown_rx) = oneshot::channel::<()>();
    let handle = tokio::spawn(async move {
        let result = run_viewer_session(
            relay_addrs,
            &device_cam_str,
            no_audio,
            udp_port,
            enable_mdns,
            stream_type,
            network_type,
            enable_dcutr,
            keypair,
            video_tx,
            audio_tx,
            event_tx.clone(),
            shutdown_rx,
        ).await;

        match result {
            Ok(()) => {} // 正常退出（含最小化主动关闭），event_tx drop 通知主循环
            Err(e) => {
                let _ = event_tx.send(SessionEvent::Disconnected {
                    reason: e.to_string(),
                }).await;
            }
        }
    }).abort_handle();
    (handle, shutdown_tx)
}

/// 重置帧率统计，在重连时调用
fn reset_stats(frame_count: &mut u64, bytes_received: &mut u64, video_start: &mut Option<std::time::Instant>) {
    *frame_count = 0;
    *bytes_received = 0;
    *video_start = None;
}

/// 消费 channel 中所有残留视频帧
#[allow(unused_variables)]
fn drain_channel(
    rx: &mut mpsc::Receiver<MediaPacket>,
    frame_count: &mut u64,
    bytes_received: &mut u64,
    video_start: &mut Option<std::time::Instant>,
    output_file: &mut Option<std::fs::File>,
    #[cfg(feature = "player")] mut player: Option<&mut player::VideoPlayer>,
) {
    while let Ok(packet) = rx.try_recv() {
        match process_video_frame(
            packet, frame_count, bytes_received, video_start, output_file,
            #[cfg(feature = "player")]
            player.as_mut().map(|p| &mut **p),
        ) {
            RenderAction::Quit => break,
            _ => {}
        }
    }
}

/// 消费音频 channel 残留帧
fn drain_audio_channel(
    audio_rx: &mut mpsc::Receiver<MediaPacket>,
    audio_count: &mut u64,
    #[cfg(feature = "player")] mut audio_player: Option<&mut player::AudioPlayer>,
) {
    while let Ok(packet) = audio_rx.try_recv() {
        *audio_count += 1;
        #[cfg(feature = "player")]
        if let Some(ap) = audio_player.as_mut().map(|p| &mut **p) {
            ap.write(&packet.data);
        }
    }
}

/// 一次 Viewer 会话: 连接 Relay → Circuit 拨号 DeviceCam → 打开 stream → 驱动 swarm
///
/// 帧接收通过 spawn 的 receive_frames task → channel → 主循环消费。
/// 本函数只负责 swarm 事件循环，连接断开时返回 Err 通知主循环重连。
///
/// 支持多 Relay 并发拨号和 mDNS 局域网发现:
/// - 同时拨号所有 Relay，第一个成功即用
/// - 如果 enable_mdns，并行监听 mDNS 发现事件 (5 秒超时)
/// - mDNS 发现目标 DeviceCam 时，优先使用 LAN 直连
/// - 否则通过第一个成功连接的 Relay circuit 拨号 DeviceCam
async fn run_viewer_session(
    relay_addrs: Vec<String>,
    device_cam_str: &str,
    no_audio: bool,
    udp_port: Option<u16>,
    enable_mdns: bool,
    stream_type: String,
    network_type: String,
    enable_dcutr: bool,
    keypair: libp2p::identity::Keypair,
    video_tx: mpsc::Sender<MediaPacket>,
    audio_tx: mpsc::Sender<MediaPacket>,
    event_tx: mpsc::Sender<SessionEvent>,
    // 最小化时主循环通过此通道通知 session 优雅关闭：
    // 仅 abort 会话任务(swarm)不够 —— receive_frames 任务仍持有子流句柄，
    // 会令 QUIC 连接处理任务不退出、连接不关闭，设备侧便持续推流。
    // 必须显式 abort receive_frames(释放子流) 并 disconnect_peer_id 才能真正关流。
    mut shutdown_rx: oneshot::Receiver<()>,
) -> Result<()> {
    let device_cam: PeerId = device_cam_str.parse()
        .context("Invalid camera PeerId")?;

    // 解析所有 Relay 地址
    let relay_multiaddrs: Vec<Multiaddr> = relay_addrs.iter()
        .map(|s| s.parse::<Multiaddr>())
        .collect::<std::result::Result<Vec<_>, _>>()
        .context("Invalid relay address")?;

    let local_peer_id = keypair.public().to_peer_id();
    println!("[Viewer] PeerId: {local_peer_id}");

    // ---- 构建 Swarm ----
    // DCUtR 默认启用（锥形/EIM NAT 含多数 4G 可打洞成功，省中继带宽）。
    // 仅当本端确认为 Symmetric NAT 时（由上层在重连时传入 enable_dcutr=false）
    // 才禁用 dcutr 行为，跳过无效打洞 ~17s。
    if !enable_dcutr {
        tracing::info!(
            "[Viewer] DCUtR disabled (Symmetric NAT) - using Relay Circuit only"
        );
    }
    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            libp2p::yamux::Config::default,
        )?
        .with_quic()
        .with_relay_client(noise::Config::new, libp2p::yamux::Config::default)?
        .with_behaviour(|key, relay_client| {
            // 启用 push_listen_addr_updates
            let identify_config = identify::Config::new(
                "/p2p-camera-viewer/1.0.0".to_string(),
                key.public().clone(),
            )
            .with_push_listen_addr_updates(true);
            ViewerBehaviour::new_with_identify_config(
                key.public().clone(),
                relay_client,
                identify_config,
                enable_dcutr,
            )
        })?
        // idle timeout 120s: DCUtR handler 在重试期间需要 keep-alive，0 会导致连接被意外关闭
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(120)))
        .build();

    // ---- 监听本地 QUIC (固定端口，若指定) ----
    let udp_port = udp_port.unwrap_or(0);
    let udp_addr = format!("/ip4/0.0.0.0/udp/{}/quic-v1", udp_port).parse()
        .context("Invalid local QUIC listen addr")?;
    swarm.listen_on(udp_addr)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()
        .context("Invalid local TCP listen addr")?)?;
    println!("[Viewer] Listening on QUIC (port {}) and TCP",
        if udp_port != 0 { udp_port.to_string() } else { "random".to_string() });

    // ---- 外部地址由 identify 协议自动发现（公网 IP）和 NewListenAddr 事件自动注入（本地 IP） ----

    // ---- NAT 诊断状态 ----
    let mut nat_diagnostic: Option<NatDiagnostic> = None;
    let mut local_nat_type: Option<NatType> = None;
    let mut remote_nat_hint: Option<String> = None;
    let mut local_ips: Vec<Ipv4Addr> = Vec::new();
    let mut local_quic_port: u16 = 0;

    // 缓存 wait_for_event 期间被吞掉的事件，供主循环处理
    let mut pending_events: Vec<SwarmEvent<ViewerBehaviourEvent>> = Vec::new();

    // ---- 1. 多 Relay 并发拨号 + mDNS 发现 ----
    // 同时拨号所有 Relay
    for addr in &relay_multiaddrs {
        println!("[Viewer] Dialing relay: {addr}");
        if let Err(e) = swarm.dial(addr.clone()) {
            tracing::warn!("[Viewer] Failed to dial relay {addr}: {e}");
        }
    }

    // 并行等待: mDNS 发现 或 Relay 连接
    let mdns_deadline = tokio::time::Instant::now() + MDNS_DISCOVERY_TIMEOUT;
    let mut connected_relay: Option<(PeerId, Multiaddr)> = None;
    let mut mdns_discovered_addr: Option<Multiaddr> = None;
    let mut relay_error_count: usize = 0;
    let total_relays = relay_multiaddrs.len();

    // 提取 relay PeerIds 用于判断连接类型
    let relay_peer_ids: Vec<PeerId> = relay_multiaddrs.iter()
        .filter_map(|a| a.iter().find_map(|p| match p {
            Protocol::P2p(pid) => Some(pid),
            _ => None,
        }))
        .collect();

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

        let remaining = Duration::from_secs(30);

        let event_result = tokio::time::timeout(remaining, swarm.select_next_some()).await;

        match event_result {
            Ok(event) => {
                // 收集 NewListenAddr 事件中的本地 IP (在 match 之前用引用读取)
                if let SwarmEvent::NewListenAddr { address, .. } = &event {
                    let is_quic = address.iter().any(|p| matches!(p, Protocol::QuicV1));
                    let is_relayed = address.iter().any(|p| matches!(p, Protocol::P2pCircuit));
                    if is_quic && !is_relayed {
                        if let Some(Protocol::Ip4(ip)) = address.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                            if !ip.is_loopback() && !ip.is_unspecified() {
                                swarm.add_external_address(address.clone());
                                if !local_ips.contains(&ip) {
                                    local_ips.push(ip);
                                }
                                if let Some(Protocol::Udp(port)) = address.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                                    local_quic_port = port;
                                }
                            }
                        }
                    }
                }

                // match 移动 event，Identify 事件缓存到 pending_events
                match event {
                    SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                        let addr = endpoint.get_remote_address().clone();
                        // 检查是否为 Relay 连接
                        if relay_peer_ids.contains(&peer_id) && connected_relay.is_none() {
                            println!("[Viewer] Connected to relay {peer_id}");
                            connected_relay = Some((peer_id, addr));
                        }
                    }
                    SwarmEvent::Behaviour(ViewerBehaviourEvent::Identify(_)) => {
                        // 缓存 Identify 事件供主循环处理
                        pending_events.push(event);
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
                        if relay_peer_ids.contains(&peer_id) {
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
                        tracing::debug!("[Viewer] Event during connection phase");
                    }
                }
            }
            Err(_) => {
                // timeout
            }
        }
    }

    // ---- 2. 使用 mDNS LAN 直连 或 Relay circuit 拨号 DeviceCam ----
    let mut initial_is_relay = false;
    if let Some(lan_addr) = mdns_discovered_addr {
        // mDNS 发现了目标 DeviceCam，优先使用 LAN 直连
        tracing::info!("[Viewer] Using mDNS-discovered LAN address: {lan_addr}");
        swarm.dial(lan_addr)?;
        wait_for_event_collecting(&mut swarm, |e| matches!(
            e,
            SwarmEvent::ConnectionEstablished { peer_id, .. } if *peer_id == device_cam
        ), "device-cam LAN direct connection", &mut local_ips, &mut local_quic_port, &mut pending_events).await?;
        println!("[Viewer] Connected to device-cam {device_cam} via LAN direct (mDNS)");

        // LAN 直连已建立，关闭 relay 连接以阻止 DCUtR 继续尝试打洞
        // DCUtR 打洞会干扰已正常工作的 LAN 直连，导致 stream 断开
        if let Some((relay_peer_id, _)) = connected_relay {
            tracing::info!("[Viewer] Closing relay connection to {relay_peer_id} after LAN direct established (prevents DCUtR interference)");
            let _ = swarm.disconnect_peer_id(relay_peer_id);
        }
    } else if let Some((relay_peer_id, relay_addr)) = connected_relay {
        // 通过 Relay circuit 拨号 DeviceCam
        let circuit_addr = relay_addr
            .with(Protocol::P2pCircuit)
            .with(Protocol::P2p(device_cam));
        println!("[Viewer] Dialing device-cam via circuit: {circuit_addr}");
        swarm.dial(circuit_addr)?;
        wait_for_event_collecting(&mut swarm, |e| matches!(
            e,
            SwarmEvent::ConnectionEstablished { peer_id, .. } if *peer_id == device_cam
        ), "device-cam circuit connection", &mut local_ips, &mut local_quic_port, &mut pending_events).await?;
        println!("[Viewer] Connected to device-cam {device_cam} via relay circuit");
        initial_is_relay = true;

        // DCUtR 尝试前预测：在 circuit 连接建立后立即输出 NAT 上下文
        if let Some(ref diag) = nat_diagnostic {
            let prediction = diag.dcutr_prediction();
            if prediction.likely_success {
                tracing::info!("[Viewer] DCUtR prediction: likely SUCCESS - {}", prediction.reason);
            } else {
                tracing::warn!("[Viewer] DCUtR prediction: likely FAIL - {}", prediction.reason);
            }
            // 连接策略日志
            let (strategy, reason) = diag.connection_strategy();
            tracing::info!("[Viewer] Connection strategy: {} - {}", strategy.name(), reason);
        } else {
            tracing::info!("[Viewer] DCUtR will be attempted (NAT diagnostic not yet available)");
        }
    } else {
        anyhow::bail!("Failed to connect: no relay connection established and no mDNS discovery");
    }

    // ---- 3. 打开 video stream ----
    let mut stream_control = swarm.behaviour().stream.new_control();
    let video_protocol = resolve_stream_protocol(&stream_type, initial_is_relay);
    let resolved_name = if video_protocol == stream_protocols::VIDEO_MAIN_PROTOCOL { "main" }
        else if video_protocol == stream_protocols::VIDEO_SUB_PROTOCOL { "sub" }
        else { "third" };
    let video_stream = stream_control
        .open_stream(device_cam, video_protocol)
        .await
        .context("Failed to open video stream")?;
    println!("[Viewer] Video stream opened (requested={}, resolved={}, relay={})", stream_type, resolved_name, initial_is_relay);

    // ---- 3b. 打开 audio stream (可选) ----
    let mut audio_abort_handle: Option<tokio::task::AbortHandle> = None;
    if !no_audio {
        match stream_control.open_stream(device_cam, stream_protocols::AUDIO_PROTOCOL).await {
            Ok(audio_stream) => {
                println!("[Viewer] Audio stream opened");
                let h = tokio::spawn(receive_frames(device_cam, audio_stream, audio_tx.clone(), event_tx.clone()))
                    .abort_handle();
                audio_abort_handle = Some(h);
            }
            Err(e) => {
                println!("[Viewer] Audio stream open failed (non-fatal): {e}");
            }
        }
    }

    // ---- 4. 启动视频接收任务 ----
    let mut video_abort_handle: Option<tokio::task::AbortHandle> =
        Some(tokio::spawn(receive_frames(device_cam, video_stream, video_tx.clone(), event_tx.clone())).abort_handle());

    let mut direct_upgraded = !initial_is_relay; // 初始直连时无需再升级
    let mut lan_direct_attempted = false;
    let mut relay_connection_id: Option<libp2p::swarm::ConnectionId> = None;

    // 初始化 NAT 诊断（如果 wait_for_event 期间已收集到 local_ips）
    if nat_diagnostic.is_none() && local_quic_port != 0 && !local_ips.is_empty() {
        let mut diag = NatDiagnostic::new(local_quic_port, local_ips.clone());
        if network_type == "4g" {
            diag.set_force_4g(true);
            tracing::info!("[Viewer] Network type: 4G (forced by config)");
        }
        nat_diagnostic = Some(diag);
        tracing::info!("[Viewer] NAT diagnostic initialized: port={}, ips={:?}", local_quic_port, local_ips);
    }

    if !local_ips.is_empty() {
        tracing::info!("[Viewer] Local IPs collected: {:?}", local_ips);
    }

    // ---- 5. Swarm 事件循环 (帧消费在主循环中，不在此处) ----
    // 将 pending_events (wait_for_event 期间缓存的事件) 放入队列
    let mut event_queue: std::collections::VecDeque<SwarmEvent<ViewerBehaviourEvent>> =
        pending_events.into_iter().collect();

    loop {
        // 优先处理缓存事件，再从 swarm 取新事件
        let event = if let Some(e) = event_queue.pop_front() {
            e
        } else {
            tokio::select! {
                // 最小化时主循环发来关闭信号：主动释放子流并断开连接，
                // 让设备侧 stream_video_to_viewer 的 write_all 失败、停止推流。
                _ = &mut shutdown_rx => {
                    if let Some(h) = video_abort_handle.take() { h.abort(); }
                    if let Some(h) = audio_abort_handle.take() { h.abort(); }
                    let _ = swarm.disconnect_peer_id(device_cam);
                    println!("[Viewer] Session shutdown (window minimized), closing connection to device");
                    return Ok(());
                }
                ev = swarm.select_next_some() => ev,
            }
        };

        match event {
            // DCUtR 或局域网直连建立后，升级 stream
            SwarmEvent::ConnectionEstablished { peer_id, endpoint, connection_id, .. } if peer_id == device_cam && !direct_upgraded => {
                let addr = endpoint.get_remote_address().clone();
                let is_relay = addr.iter().any(|p| matches!(p, Protocol::P2pCircuit));
                if is_relay {
                    // 记录中继连接 ID，直连升级后关闭
                    relay_connection_id = Some(connection_id);
                    continue;
                }
                // 判断直连类型：如果远程地址是私有 IP，则为局域网直连
                let is_lan = addr.iter().any(|p| {
                    if let Protocol::Ip4(ip) = p { ip.is_private() } else { false }
                });
                let via = if is_lan { "LAN direct" } else { "DCUtR hole punch" };
                println!("[Viewer] Direct connection established with {device_cam} via {via}, upgrading streams...");
                // 直连升级：如果 stream_type 为 "auto"，在直连上打开主码流
                let upgrade_protocol = if stream_type == "auto" {
                    stream_protocols::VIDEO_MAIN_PROTOCOL
                } else {
                    get_video_protocol(&stream_type)
                };
                match stream_control.open_stream(device_cam, upgrade_protocol).await {
                    Ok(new_stream) => {
                        if let Some(h) = video_abort_handle.take() { h.abort(); }
                        let handle = tokio::spawn(receive_frames(device_cam, new_stream, video_tx.clone(), event_tx.clone())).abort_handle();
                        video_abort_handle = Some(handle);
                        direct_upgraded = true;
                        let _ = event_tx.send(SessionEvent::DirectUpgraded { via_lan: is_lan }).await;
                        if stream_type == "auto" {
                            println!("[Viewer] Stream upgraded: sub → main (direct connection)");
                        } else {
                            println!("[Viewer] Video stream upgraded to direct connection (stream={})", stream_type);
                        }

                        // 直连升级成功后，关闭中继连接
                        // 防止 DCUtR 基于中继连接继续尝试打洞，导致视频卡顿
                        if let Some(relay_conn_id) = relay_connection_id.take() {
                            tracing::info!("[Viewer] Closing relay circuit connection after direct upgrade (prevents DCUtR interference)");
                            swarm.close_connection(relay_conn_id);
                        }
                    }
                    Err(e) => {
                        println!("[Viewer] Failed to open direct video stream (staying on current): {e}");
                    }
                }
                if direct_upgraded && !no_audio {
                    match stream_control.open_stream(device_cam, stream_protocols::AUDIO_PROTOCOL).await {
                        Ok(new_stream) => {
                            if let Some(h) = audio_abort_handle.take() { h.abort(); }
                            let handle = tokio::spawn(receive_frames(device_cam, new_stream, audio_tx.clone(), event_tx.clone())).abort_handle();
                            audio_abort_handle = Some(handle);
                            println!("[Viewer] Audio stream upgraded to direct connection");
                        }
                        Err(e) => {
                            println!("[Viewer] Failed to open direct audio stream: {e}");
                        }
                    }
                }
            }
            SwarmEvent::ConnectionEstablished { .. } => {}
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                mdns::Event::Discovered(peers),
            )) => {
                for (peer_id, addr) in peers {
                    tracing::info!("[Viewer] mDNS discovered peer {peer_id} at {addr}");
                }
            }
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                mdns::Event::Expired(peers),
            )) => {
                for (peer_id, addr) in peers {
                    tracing::debug!("[Viewer] mDNS peer expired: {peer_id} at {addr}");
                }
            }
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                dcutr::Event { result: Ok(_), .. },
            )) => {
                // DCUtR 成功会触发 ConnectionEstablished，在那里处理升级
            }
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                dcutr::Event { result: Err(e), remote_peer_id, .. },
            )) => {
                let err_str = e.to_string();
                let local_nat = local_nat_type.map(|t| t.short_name()).unwrap_or("Unknown");
                let remote_nat = remote_nat_hint.as_deref().unwrap_or("Unknown");
                tracing::warn!("[Viewer] DCUtR hole punch FAILED with {remote_peer_id}: {e}");
                tracing::warn!("[Viewer] NAT context: local={}, remote={}", local_nat, remote_nat);
                if let Some(ref diag) = nat_diagnostic {
                    let result = diag.diagnose();
                    tracing::warn!("[Viewer] Suggestion: {}", result.dcutr_suggestion);
                } else {
                    if err_str.contains("timeout") {
                        tracing::warn!("[Viewer] DCUtR failure cause: NAT type incompatibility or firewall blocking UDP");
                    } else if err_str.contains("IO error") || err_str.contains("connection refused") || err_str.contains("network unreachable") {
                        tracing::warn!("[Viewer] DCUtR failure cause: network unreachable or connection refused");
                    }
                    tracing::warn!("[Viewer] If both peers are behind symmetric NAT, DCUtR cannot succeed. Consider:");
                    tracing::warn!("[Viewer]   1. Configure port forwarding on router (map external UDP port → device internal UDP port)");
                    tracing::warn!("[Viewer]   2. Use --external-ip and --udp-port on device-cam to advertise correct external address");
                }
                // 快速降级确认：DCUtR 失败后确认 Relay Circuit 仍在工作
                let has_circuit = swarm.is_connected(&device_cam);
                if has_circuit {
                    tracing::info!("[Viewer] Fallback: Relay circuit is still active, video/audio will continue via relay");
                } else {
                    tracing::warn!("[Viewer] Fallback: Relay circuit may be lost, connection may drop soon");
                }
            }
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Identify(
                identify::Event::Received { info, peer_id: identify_peer_id, .. },
            )) => {
                tracing::info!("[Viewer] Identify: observed_addr={}, listen_addrs={}",
                    info.observed_addr,
                    info.listen_addrs.len());

                // NAT 诊断：记录观测地址
                if let Some(ref mut diag) = nat_diagnostic {
                    diag.record_observed(&info.observed_addr);
                    let result = diag.diagnose();
                    tracing::info!("[Viewer] NAT diagnosis: {}", result.nat_type.description());
                    if result.is_4g {
                        tracing::info!("[Viewer] 4G/CGNAT network detected");
                    }
                    tracing::info!("[Viewer] DCUtR suggestion: {}", result.dcutr_suggestion);
                    let dcutr_currently_enabled = swarm.behaviour().dcutr.is_enabled();
                    tracing::info!(
                        "[Viewer] === NAT type: {} ({}) | DCUtR currently {} | will skip on reconnect: {} ===",
                        result.nat_type.short_name(),
                        if result.is_4g { "4G/CGNAT" } else { "broadband" },
                        if dcutr_currently_enabled { "enabled" } else { "DISABLED" },
                        result.nat_type == NatType::Symmetric
                    );
                    local_nat_type = Some(result.nat_type);
                    let _ = event_tx.send(SessionEvent::NatDiagnosis {
                        local_nat: result.nat_type,
                        remote_nat: remote_nat_hint.clone(),
                    }).await;
                }

                // 对端 NAT 类型推断 + 局域网直连检测（仅对 device-cam 的 Identify 事件）
                if identify_peer_id == device_cam {
                    if let Some(Protocol::Udp(observed_port)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                        // 从对端 listen_addrs 中找 QUIC 端口
                        let remote_quic_port = info.listen_addrs.iter()
                            .filter(|a| a.iter().any(|p| matches!(p, Protocol::QuicV1)))
                            .filter_map(|a| a.iter().find(|p| matches!(p, Protocol::Udp(_))))
                            .find_map(|p| if let Protocol::Udp(port) = p { Some(port) } else { None });

                        if let Some(remote_port) = remote_quic_port {
                            if observed_port == remote_port {
                                remote_nat_hint = Some("Cone".to_string());
                                tracing::info!("[Viewer] Remote peer NAT hint: Cone (observed port {} matches listen port {})", observed_port, remote_port);
                            } else {
                                remote_nat_hint = Some("Symmetric?".to_string());
                                tracing::warn!("[Viewer] Remote peer NAT hint: possibly Symmetric (observed port {} != listen port {})", observed_port, remote_port);
                            }
                        }
                    }

                    // 局域网直连检测：检查对端 listen_addrs 中是否有与本地 IP 同子网的 QUIC 地址
                    if !direct_upgraded && !lan_direct_attempted {
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
                                // 检查是否与本地某个 IP 在同一 /24 子网
                                if let Some(Protocol::Ip4(remote_ip)) = a.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                                    local_ips.iter().any(|local_ip| {
                                        is_same_subnet(*local_ip, remote_ip)
                                    })
                                } else {
                                    false
                                }
                            })
                            .cloned()
                            .collect();

                        if !lan_addrs.is_empty() {
                            lan_direct_attempted = true;
                            for addr in &lan_addrs {
                                tracing::info!("[Viewer] LAN direct: detected same-subnet address {addr}, dialing...");
                            }
                            // 拨号第一个同子网 QUIC 地址
                            if let Err(e) = swarm.dial(lan_addrs[0].clone()) {
                                tracing::warn!("[Viewer] LAN direct dial failed: {e}");
                            }
                        }
                    }
                }

                if let Some(Protocol::Ip4(ip)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                    if ip.is_private() {
                        tracing::warn!("[Viewer] WARNING: Observed address is private IP ({}) - DCUtR may fail!", ip);
                    } else {
                        tracing::info!("[Viewer] Observed address is public IP ({}) - good for DCUtR", ip);
                    }
                }
                if info.observed_addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                    tracing::info!("[Viewer] Observed address protocol: QUIC - good for DCUtR hole punching");
                    // NAT 端口映射检测: observed_addr 的 UDP 端口与本地监听端口不一致时，可能是对称型 NAT
                    if let Some(Protocol::Udp(observed_port)) = info.observed_addr.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                        let local_port = udp_port;
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
            }
            SwarmEvent::NewListenAddr { address, .. } => {
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
                            tracing::info!("[Viewer] Added local address as DCUtR candidate: {address}");

                            // 收集本地 IP 和端口用于 NAT 诊断
                            if !local_ips.contains(&ip) {
                                local_ips.push(ip);
                            }
                            if let Some(Protocol::Udp(port)) = address.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                                local_quic_port = port;
                            }
                            // 延迟初始化 NatDiagnostic（需要端口和 IP）
                            if nat_diagnostic.is_none() && local_quic_port != 0 {
                                let mut diag = NatDiagnostic::new(local_quic_port, local_ips.clone());
                                if network_type == "4g" {
                                    diag.set_force_4g(true);
                                }
                                nat_diagnostic = Some(diag);
                                tracing::info!("[Viewer] NAT diagnostic initialized: port={}, ips={:?}", local_quic_port, local_ips);
                            }
                        }
                    }
                }
            }
            SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
                if peer_id == device_cam {
                    if num_established == 0 {
                        // 所有到 DeviceCam 的连接都已关闭 (circuit + direct 都没了)
                        println!("[Viewer] DeviceCam connection closed (no remaining connections)");
                        return Err(anyhow::anyhow!("DeviceCam connection closed"));
                    } else {
                        // DCUtR 直连建立后, 旧的 circuit 连接会被关闭, 但直连仍在
                        println!("[Viewer] Circuit connection to device-cam closed, {num_established} direct remaining");
                    }
                } else {
                    tracing::warn!("[Viewer] Connection closed: {peer_id} ({num_established} remaining)");
                }
            }
            e => {
                tracing::debug!("[Viewer] Event: {:?}", e);
            }
        }
    }
}

/// 处理单个视频帧，返回 RenderAction 表示渲染结果和窗口状态
#[allow(unused_variables)]
fn process_video_frame(
    packet: MediaPacket,
    frame_count: &mut u64,
    bytes_received: &mut u64,
    video_start: &mut Option<std::time::Instant>,
    output_file: &mut Option<std::fs::File>,
    #[cfg(feature = "player")] player: Option<&mut player::VideoPlayer>,
) -> RenderAction {
    *frame_count += 1;
    *bytes_received += packet.data.len() as u64;

    // 第一帧到达时记录起始时间，用于 fps 计算
    if video_start.is_none() {
        *video_start = Some(std::time::Instant::now());
    }

    // 帧率统计（在渲染之前输出，确保不受渲染返回值影响）
    if *frame_count % 100 == 0 {
        if let Some(start) = video_start {
            let elapsed = start.elapsed().as_secs_f64();
            let fps = *frame_count as f64 / elapsed;
            let kbps = (*bytes_received * 8) as f64 / elapsed / 1000.0;
            let keyframe = if packet.is_keyframe() { "[I]" } else { "   " };
            println!(
                "[Viewer] {keyframe} frame #{} | {:.1} fps | {:.0} kbps | ts={}",
                frame_count, fps, kbps, packet.timestamp_ms
            );
        }
    }

    if let Some(file) = output_file {
        use std::io::Write;
        if file.write_all(&packet.data).is_err() {
            return RenderAction::Quit;
        }
        let _ = file.flush();
    }

    #[cfg(feature = "player")]
    if let Some(p) = player {
        match p.render(
            &packet.data,
            packet.is_keyframe() || is_nal_keyframe(&packet.data),
        ) {
            Ok(action) => return action,
            Err(e) => {
                tracing::error!("[Viewer] Player error: {e}");
            }
        }
    }

    RenderAction::Continue
}

/// 等待首个关键帧的最长时间（最后安全网）。正常情况下门控由 `is_nal_keyframe` 扫描
/// 字节流里的真实 IDR NAL 立即开门（cam 在 `request_idr` 后很快产出真 IDR），不会等到这里。
/// 仅当字节扫描也异常时才兜底强制开门，避免 `!got_first_idr` 门控永久不开 → 黑屏。
/// 3s 足够覆盖一个 GOP 周期（gop=50@25fps≈2s），正常绝不触发。
const IDR_WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3);

/// 从 Annex B 裸流扫描 NAL，判断是否为关键帧（IRAP）。
/// H.265: IRAP NAL type 16..=21 (BLA/IDR/CRA)；H.264: IDR NAL type 5。
/// 与 C 侧 `rk_camera.c` 的 `is_keyframe_h265/h264` 判定范围一致，但**在 viewer 侧独立扫描
/// 字节**，不依赖对端 `is_keyframe()` 标志位。日志实证：cam 发的 IDR 数据字节里确有 IRAP
/// NAL（ffmpeg 能据此恢复解码），但 `is_keyframe()` 标志常不可靠（8s 内检测不到），故必须用
/// 本函数扫字节兜底——这是之前版本快速起播的关键，删掉它改用 `is_keyframe()` 正是本次变慢的根因。
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

async fn receive_frames(
    peer_id: PeerId,
    mut stream: libp2p::swarm::Stream,
    sender: mpsc::Sender<MediaPacket>,
    event_tx: mpsc::Sender<SessionEvent>,
) {
    let mut buf = BytesMut::with_capacity(STREAM_READ_BUF);
    let mut read_buf = vec![0u8; STREAM_READ_BUF];
    // HEVC 初始帧容错：丢弃首个 IDR 前的非关键帧，避免解码器缺少参考帧导致
    // "Player error" (ffmpeg 无法从无参考的 P/B 帧开始解码)。
    // 收到第一个关键帧后恢复正常转发。
    let mut got_first_idr = false;
    // 收到首个视频包的时间，用于 IDR 等待超时兜底
    let mut first_video_at: Option<std::time::Instant> = None;

    loop {
        match stream.read(&mut read_buf).await {
            Ok(0) => {
                println!("[Viewer] Stream EOF from {peer_id}");
                let _ = event_tx.send(SessionEvent::StreamEOF {
                    reason: format!("Stream EOF from {peer_id}"),
                }).await;
                break;
            }
            Ok(n) => {
                buf.extend_from_slice(&read_buf[..n]);
                while let Some(packet) = MediaPacket::try_decode(&mut buf) {
                    // 仅对视频流做 IDR 容错；音频始终透传。
                    // 关键帧判定: `is_keyframe()`(对端标志, 实测常不可靠) || `is_nal_keyframe`(扫字节, 可靠)。
                    // 之前版本用字节扫描起播很快; 后来误删字节扫描、改信 `is_keyframe()` 标志,
                    // 导致门控等不到关键帧、卡满整个超时(8s)才强制开门 → 起播极慢。
                    // 收到首个 IDR 前的非关键帧直接丢弃(解码器无参考帧会报错),
                    // 收到首个 IDR 后恢复正常转发。
                    if packet.track == MediaTrack::Video && !got_first_idr {
                        if first_video_at.is_none() {
                            first_video_at = Some(std::time::Instant::now());
                        }
                        let is_kf =
                            packet.is_keyframe() || is_nal_keyframe(&packet.data);
                        let waited = first_video_at.map(|t| t.elapsed());
                        if is_kf || waited.map_or(false, |w| w >= IDR_WAIT_TIMEOUT) {
                            got_first_idr = true;
                            if is_kf {
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
                    if sender.send(packet).await.is_err() {
                        return;
                    }
                }
            }
            Err(e) => {
                tracing::warn!("[Viewer] Stream read error: {e}");
                let _ = event_tx.send(SessionEvent::StreamEOF {
                    reason: format!("Stream read error: {e}"),
                }).await;
                break;
            }
        }
    }
}

/// 等待特定事件 (带超时)，同时收集 local_ips 和缓存 Identify/NewListenAddr 事件
async fn wait_for_event_collecting(
    swarm: &mut libp2p::Swarm<ViewerBehaviour>,
    predicate: impl Fn(&SwarmEvent<ViewerBehaviourEvent>) -> bool,
    label: &str,
    local_ips: &mut Vec<Ipv4Addr>,
    local_quic_port: &mut u16,
    pending_events: &mut Vec<SwarmEvent<ViewerBehaviourEvent>>,
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

        if let SwarmEvent::OutgoingConnectionError { error, .. } = &event {
            tracing::warn!("[Viewer] Connection error ({label}): {error}");
        }

        // 收集 NewListenAddr 事件中的本地 IP
        if let SwarmEvent::NewListenAddr { address, .. } = &event {
            let is_quic = address.iter().any(|p| matches!(p, Protocol::QuicV1));
            let is_relayed = address.iter().any(|p| matches!(p, Protocol::P2pCircuit));
            if is_quic && !is_relayed {
                if let Some(Protocol::Ip4(ip)) = address.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                    if !ip.is_loopback() && !ip.is_unspecified() {
                        swarm.add_external_address(address.clone());
                        if !local_ips.contains(&ip) {
                            local_ips.push(ip);
                        }
                        if let Some(Protocol::Udp(port)) = address.iter().find(|p| matches!(p, Protocol::Udp(_))) {
                            *local_quic_port = port;
                        }
                    }
                }
            }
        }

        // 缓存 Identify 事件，供主循环处理
        // 注意: SwarmEvent 不实现 Clone，只能 move，不能 clone
        match event {
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Identify(_)) => {
                pending_events.push(event);
            }
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                mdns::Event::Discovered(peers),
            )) => {
                // mDNS 发现事件在连接阶段已处理，主循环中仅记录日志
                for (peer_id, addr) in peers {
                    tracing::info!("[Viewer] mDNS discovered peer {peer_id} at {addr}");
                }
            }
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
                mdns::Event::Expired(peers),
            )) => {
                for (peer_id, addr) in peers {
                    tracing::debug!("[Viewer] mDNS peer expired: {peer_id} at {addr}");
                }
            }
            _ => {}
        }
    }
}

// ---- SDL Player (player feature) ----

/// VideoPlayer::render() / process_video_frame() 的返回动作
enum RenderAction {
    /// 正常渲染，继续主循环
    Continue,
    /// 窗口已最小化
    WindowMinimized,
    /// 窗口已恢复（从最小化）
    WindowRestored,
    /// 用户关闭窗口，退出主循环
    Quit,
}

#[cfg(feature = "player")]
mod player {
    use super::RenderAction;
    use anyhow::{Context, Result};
    use ffmpeg_next as ffmpeg;
    use sdl2::event::Event;
    use sdl2::event::WindowEvent;

    use sdl2::keyboard::Keycode;
    use sdl2::pixels::PixelFormatEnum;
    use sdl2::rect::Rect;

    /// 将 sdl2 的各种错误类型统一转为 anyhow::Error
    fn map_sdl<T, E: std::string::ToString>(r: std::result::Result<T, E>, ctx: &str) -> Result<T> {
        r.map_err(|e| anyhow::anyhow!("SDL {ctx}: {}", e.to_string()))
    }

    /// H.265 解码 + SDL2 渲染的实时播放器
    ///
    /// SAFETY: `texture` 字段使用 `Texture<'static>`，实际生命周期绑定到 `canvas`。
    /// Rust 保证 struct 字段按声明顺序 drop，因此 texture (在前) 先于 canvas drop。
    pub struct VideoPlayer {
        texture: Option<sdl2::render::Texture<'static>>,
        canvas: sdl2::render::Canvas<sdl2::video::Window>,
        decoder: ffmpeg::decoder::Video,
        event_pump: sdl2::EventPump,
        scaler: Option<ffmpeg::software::scaling::Context>,
        yuv_frame: ffmpeg::frame::Video,
        width: u32,
        height: u32,
        frame_count: u64,
        /// 窗口是否最小化：最小化时跳过解码和渲染，避免 SDL 阻塞导致 stream EOF 延迟检测
        minimized: bool,
        /// 解码器 flush 后等待关键帧：跳过非关键帧，避免 POC 错误导致花屏
        waiting_for_keyframe: bool,
        /// 进入 waiting_for_keyframe 的时间，用于超时兜底（避免 cam 关键帧标记
        /// 异常时播放器门控永久不开 → 黑屏）
        waiting_since: Option<std::time::Instant>,
    }

    impl VideoPlayer {
        /// 重置解码器状态，在重连后调用以清除旧的参考帧缓冲区
        /// 避免旧参考帧导致马赛克
        pub fn reset_decoder(&mut self) {
            // 使用 avcodec_flush_buffers 清除解码器内部缓冲区
            // 这会重置解码器状态，使下一帧必须从关键帧开始
            self.decoder.flush();
            self.waiting_for_keyframe = true;
            self.waiting_since = Some(std::time::Instant::now());
            eprintln!("[Player] Decoder flushed, waiting for keyframe");
        }

        /// 仅轮询 SDL 事件，不做解码/渲染
        /// 用于最小化期间检测窗口恢复事件
        pub fn poll_events(&mut self) -> RenderAction {
            for event in self.event_pump.poll_iter() {
                match event {
                    Event::Quit { .. }
                    | Event::KeyDown {
                        keycode: Some(Keycode::Escape),
                        ..
                    } => return RenderAction::Quit,
                    Event::Window { win_event, .. } => {
                        match win_event {
                            WindowEvent::Restored => {
                                eprintln!("[Player] SDL window event (poll): Restored");
                                self.minimized = false;
                                return RenderAction::WindowRestored;
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }
            RenderAction::Continue
        }

        pub fn new() -> Result<Self> {
            ffmpeg::init()?;

            let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::HEVC)
                .context("HEVC decoder not found (install libavcodec-dev / libavcodec-extra)")?;
            let decoder = ffmpeg::codec::Context::new()
                .decoder()
                .open_as(codec)
                .context("Failed to open HEVC decoder")?
                .video()?;

            let sdl_context = map_sdl(sdl2::init(), "init")?;
            let video_subsystem = map_sdl(sdl_context.video(), "video")?;

            let window = video_subsystem
                .window("P2P Camera Viewer", 1280, 720)
                .position_centered()
                .build()
                .map_err(|e| anyhow::anyhow!("SDL window: {e}"))?;
            let canvas = window
                .into_canvas()
                .accelerated()
                .present_vsync()
                .build()
                .map_err(|e| anyhow::anyhow!("SDL canvas: {e}"))?;

            let event_pump = map_sdl(sdl_context.event_pump(), "event_pump")?;

            Ok(Self {
                texture: None,
                canvas,
                decoder,
                event_pump,
                scaler: None,
                yuv_frame: ffmpeg::frame::Video::empty(),
                width: 0,
                height: 0,
                frame_count: 0,
                minimized: false,
                // 首帧同样先等真正的 IDR 再解码：设备新流建立时会先发标了关键帧的
                // VPS/SPS/PPS 独立包，并在 request_idr 后才产出真 IDR，其间 broadcast
                // 里排队的 GOP 中段 P 帧会被转发过来。若此处不门控，这些无参考 P 帧
                // 会直接进空解码器，触发 "Could not find ref with POC" 花屏（马赛克一下）。
                // 置 true 与重连后 reset_decoder() 行为一致：丢弃一切直到真 IDR。
                waiting_for_keyframe: true,
                waiting_since: Some(std::time::Instant::now()),
            })
        }

        /// 渲染一个 H.265 access unit, 返回 RenderAction 表示渲染结果和窗口状态。
        /// `is_keyframe` 由调用方从 MediaPacket 标志传入(RK VENC 硬编码, 可靠),
        /// 无需播放器再扫描 NAL。
        pub fn render(&mut self, au: &[u8], is_keyframe: bool) -> Result<RenderAction> {
            for event in self.event_pump.poll_iter() {
                match event {
                    Event::Quit { .. }
                    | Event::KeyDown {
                        keycode: Some(Keycode::Escape),
                        ..
                    } => return Ok(RenderAction::Quit),
                    Event::Window { win_event, .. } => {
                        match win_event {
                            WindowEvent::Minimized => {
                                eprintln!("[Player] SDL window event: Minimized");
                                self.minimized = true;
                                return Ok(RenderAction::WindowMinimized);
                            }
                            WindowEvent::Restored => {
                                eprintln!("[Player] SDL window event: Restored");
                                self.minimized = false;
                                return Ok(RenderAction::WindowRestored);
                            }
                            _ => {}
                        }
                    }
                    _ => {}
                }
            }

            // 窗口最小化时跳过 SDL 渲染
            // 最小化时 session 已被 abort，不会有新帧进来，此处仅作防御性处理
            let skip_render = self.minimized;

            // 解码器 flush 后等待关键帧：跳过非关键帧避免花屏
            if self.waiting_for_keyframe {
                let waited = self.waiting_since.map(|t| t.elapsed());
                // 收到关键帧，或等待超时（cam 关键帧标记异常时强制开门，避免永久黑屏）
                if !is_keyframe && !waited.map_or(false, |w| w >= crate::IDR_WAIT_TIMEOUT) {
                    return Ok(RenderAction::Continue);
                }
                self.waiting_for_keyframe = false;
                if is_keyframe {
                    eprintln!("[Player] Keyframe received, resuming decode");
                } else {
                    eprintln!(
                        "[Player] No IDR within {:?}, resuming decode anyway (cam keyframe flag may be unreliable)",
                        crate::IDR_WAIT_TIMEOUT
                    );
                }
            }

            let mut packet = ffmpeg::Packet::new(au.len());
            if let Some(data) = packet.data_mut() {
                data.copy_from_slice(au);
            }
            self.decoder.send_packet(&packet)?;

            let mut frame = ffmpeg::frame::Video::empty();
            loop {
                match self.decoder.receive_frame(&mut frame) {
                    Ok(()) => {
                        if !skip_render {
                            self.render_frame(&frame)?;
                        }
                    }
                    Err(_) => break,
                }
            }

            Ok(RenderAction::Continue)
        }

        fn render_frame(&mut self, frame: &ffmpeg::frame::Video) -> Result<()> {
            use ffmpeg::format::pixel::Pixel;

            let w = frame.width();
            let h = frame.height();

            if w != self.width || h != self.height || self.texture.is_none() {
                self.width = w;
                self.height = h;
                let tc = self.canvas.texture_creator();
                let tex = map_sdl(
                    tc.create_texture_streaming(PixelFormatEnum::IYUV, w, h),
                    "create_texture",
                )?;
                let tex: sdl2::render::Texture<'static> =
                    unsafe { std::mem::transmute::<sdl2::render::Texture<'_>, sdl2::render::Texture<'static>>(tex) };
                self.texture = Some(tex);
                map_sdl(self.canvas.window_mut().set_size(w, h), "set_size")?;
                println!("[Player] Video: {w}x{h} ({:?})", frame.format());
            }

            // YUVJ420P 与 YUV420P 数据布局相同（仅色彩范围不同），可直接使用
            // 避免 scaler 转换和 to_vec() 内存拷贝
            if frame.format() == Pixel::YUV420P || frame.format() == Pixel::YUVJ420P {
                let y = frame.data(0);
                let u = frame.data(1);
                let v = frame.data(2);
                let ys = frame.stride(0) as usize;
                let us = frame.stride(1) as usize;
                let vs = frame.stride(2) as usize;
                if let Some(tex) = &mut self.texture {
                    map_sdl(tex.update_yuv(None, y, ys, u, us, v, vs), "update_yuv")?;
                }
            } else {
                // 其他格式需要 scaler 转换
                if self.scaler.is_none() {
                    self.scaler = Some(
                        ffmpeg::software::scaling::context::Context::get(
                            frame.format(), w, h,
                            Pixel::YUV420P, w, h,
                            ffmpeg::software::scaling::Flags::BILINEAR,
                        )
                        .context("Failed to create scaler")?,
                    );
                }
                self.yuv_frame = ffmpeg::frame::Video::new(Pixel::YUV420P, w, h);
                {
                    let scaler = self.scaler.as_mut().unwrap();
                    scaler.run(frame, &mut self.yuv_frame)?;
                }
                let y = self.yuv_frame.data(0);
                let u = self.yuv_frame.data(1);
                let v = self.yuv_frame.data(2);
                let ys = self.yuv_frame.stride(0) as usize;
                let us = self.yuv_frame.stride(1) as usize;
                let vs = self.yuv_frame.stride(2) as usize;
                if let Some(tex) = &mut self.texture {
                    map_sdl(tex.update_yuv(None, y, ys, u, us, v, vs), "update_yuv")?;
                }
            }

            self.canvas.clear();
            map_sdl(
                self.canvas.copy(
                    self.texture.as_ref().unwrap(),
                    None,
                    Some(Rect::new(0, 0, self.width, self.height)),
                ),
                "copy",
            )?;
            self.canvas.present();

            self.frame_count += 1;
            if self.frame_count % 100 == 0 {
                println!("[Player] Rendered {} frames", self.frame_count);
            }

            Ok(())
        }
    }

    /// SDL2 音频播放器 — 播放 PCM 16LE 数据
    pub struct AudioPlayer {
        device: sdl2::audio::AudioDevice<AudioQueue>,
        sample_rate: i32,
    }

    struct AudioQueue {
        buffer: std::collections::VecDeque<u8>,
    }

    impl sdl2::audio::AudioCallback for AudioQueue {
        type Channel = i16;

        fn callback(&mut self, out: &mut [i16]) {
            for sample in out.iter_mut() {
                if self.buffer.len() >= 2 {
                    let lo = self.buffer.pop_front().unwrap();
                    let hi = self.buffer.pop_front().unwrap();
                    *sample = i16::from_le_bytes([lo, hi]);
                } else {
                    *sample = 0;
                }
            }
        }
    }

    impl AudioPlayer {
        pub fn new(sample_rate: u32) -> Result<Self> {
            let sdl_context = sdl2::init()
                .map_err(|e| anyhow::anyhow!("SDL init: {e}"))?;
            let audio_subsystem = sdl_context.audio()
                .map_err(|e| anyhow::anyhow!("SDL audio: {e}"))?;

            let desired_spec = sdl2::audio::AudioSpecDesired {
                freq: Some(sample_rate as i32),
                channels: Some(1),
                samples: Some(1024),
            };

            let device = audio_subsystem.open_playback(None, &desired_spec, |spec| {
                println!("[AudioPlayer] Opened: {}Hz {}ch {}samples",
                    spec.freq, spec.channels, spec.samples);
                AudioQueue {
                    buffer: std::collections::VecDeque::with_capacity(65536),
                }
            }).map_err(|e| anyhow::anyhow!("SDL audio device: {e}"))?;

            device.resume();
            println!("[AudioPlayer] Started ({}Hz)", sample_rate);

            Ok(Self {
                device,
                sample_rate: sample_rate as i32,
            })
        }

        pub fn write(&mut self, data: &[u8]) {
            let mut queue = self.device.lock();
            queue.buffer.extend(data);
            let max_bytes = (self.sample_rate as usize) * 2 / 2;
            while queue.buffer.len() > max_bytes {
                queue.buffer.pop_front();
            }
        }
    }
}

// ---- CLI ----

#[derive(Debug, Parser)]
#[command(name = "media-viewer")]
struct Opt {
    /// 配置文件路径 (不存在则自动生成默认配置)
    #[arg(long, default_value = "viewer.toml")]
    config: PathBuf,

    /// 视频流类型 (覆盖配置文件): auto, main, sub, third (默认 auto)
    #[arg(long, default_value = "auto")]
    stream: String,

    /// Relay Server 地址 (可多次使用, 覆盖配置文件)
    #[arg(long = "relay")]
    relays: Vec<String>,

    /// 摄像头 (DeviceCam) PeerId (覆盖配置文件)
    #[arg(long)]
    camera: Option<String>,

    /// 输出文件路径 (H.265 裸流, 可选, 覆盖配置文件)
    #[arg(long)]
    output: Option<PathBuf>,

    /// 禁用音频流接收 (覆盖配置文件)
    #[arg(long, default_value_t = false)]
    no_audio: bool,

    /// SDL 实时播放 (需 --features player 编译, 覆盖配置文件)
    #[cfg(feature = "player")]
    #[arg(long)]
    play: bool,

    /// QUIC UDP 监听端口 (覆盖配置文件)
    #[arg(long)]
    udp_port: Option<u16>,

    /// 是否启用 mDNS 局域网发现 (覆盖配置文件)
    #[arg(long)]
    enable_mdns: Option<bool>,
}

// ---- 配置文件 ----

fn default_enable_mdns() -> bool { true }

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ViewerConfig {
    /// 多 Relay 地址列表 (新格式优先)
    #[serde(default)]
    relays: Vec<String>,
    /// 单 Relay 地址 (旧格式, 向后兼容, 解析时合并到 relays)
    #[serde(default)]
    relay: String,
    /// 是否启用 mDNS 局域网发现 (默认 true)
    #[serde(default = "default_enable_mdns")]
    enable_mdns: bool,
    camera: String,
    /// 视频流类型: "main" | "sub" | "third" (默认 main)
    #[serde(default = "default_stream")]
    stream: String,
    #[serde(default)]
    output: Option<PathBuf>,
    #[serde(default)]
    no_audio: bool,
    #[serde(default)]
    play: bool,
    #[serde(default)]
    udp_port: Option<u16>,
    /// 网络类型: "auto" | "4g" (默认 auto)
    /// 4G 模块的 IP 可能是 RFC1918 私有地址（如 10.x.x.x），无法通过 IP 启发式检测
    /// 设置为 "4g" 后，NAT 诊断会将 4G CGNAT 的端口映射视为不可预测，DCUtR 预测更准确
    #[serde(default = "default_network_type")]
    network_type: String,
}

fn default_stream() -> String { "auto".to_string() }
fn default_network_type() -> String { "auto".to_string() }

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            relay: String::new(),
            enable_mdns: default_enable_mdns(),
            camera: String::new(),
            stream: default_stream(),
            output: None,
            no_audio: false,
            play: false,
            udp_port: None,
            network_type: default_network_type(),
        }
    }
}

impl ViewerConfig {
    fn load(path: &PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {e}", path.display()))?;
            let mut config: ViewerConfig = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config file {}: {e}", path.display()))?;
            config.resolve_relays();
            println!("[Viewer] Loaded config from {}", path.display());
            Ok(config)
        } else {
            let config = ViewerConfig::default();
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            let content = toml::to_string_pretty(&config)
                .map_err(|e| anyhow::anyhow!("Failed to serialize config: {e}"))?;
            std::fs::write(path, content)?;
            println!("[Viewer] Generated default config file: {}", path.display());
            Ok(config)
        }
    }

    /// 解析 Relay 地址列表: 处理旧格式 relay 与新格式 relays 的兼容
    ///
    /// 规则:
    /// - 如果 relays 非空, 忽略 relay (新格式优先)
    /// - 如果 relays 为空且 relay 非空, 将 relay 加入 relays (旧格式兼容)
    fn resolve_relays(&mut self) {
        if self.relays.is_empty() && !self.relay.is_empty() {
            self.relays.push(self.relay.clone());
        }
    }
}
