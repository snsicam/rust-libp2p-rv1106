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
//! GUI 模式 (play=true):
//!   窗口左侧为设备管理面板，显示 viewer.toml 中 camera_serials 设备列表；
//!   - 单击选中设备，右键菜单可「连接/断开/配置」并在右侧播放视频
//!   - [+ Add] 添加设备 (键入或 Ctrl+V 粘贴 16位序列号 SN，Enter 确认，Esc 取消)
//!   - [- Del] 删除选中设备
//!   - 增删自动保存回 viewer.toml (注意: 保存会丢失配置文件中的注释)
//!   启动时不自动连接，右键设备选择「连接」才连接。
//!
//! 自动重连: 连接断开时自动重新连接 Relay + DeviceCam + 打开 stream，
//!           播放器和输出文件持续运行不中断。
//!
//! 多 Relay + mDNS 支持:
//!   --relay 可多次使用: --relay /ip4/.../p2p/A --relay /ip4/.../p2p/B
//!   --enable-mdns (默认 true): 启用 mDNS 局域网发现，优先于 Relay

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use clap::Parser;
use mobile_core::viewer::{
    MediaPlayer, MediaPlayerEvent, ConnectOptions,
    RECONNECT_DELAY,
};
#[cfg(feature = "player")]
use mobile_core::viewer::IDR_WAIT_TIMEOUT;
use proto::media_packet::MediaPacket;
use serde::{Deserialize, Serialize};
use tracing_subscriber::EnvFilter;

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
    if let Some(ref camera) = opt.camera { config.camera_serials.insert(0, camera.clone()); }
    if let Some(ref output) = opt.output { config.output = Some(output.clone()); }
    if opt.no_audio { config.no_audio = true; }
    #[cfg(feature = "player")]
    if opt.play { config.play = true; }
    if let Some(udp_port) = opt.udp_port { config.udp_port = Some(udp_port); }
    if let Some(enable_mdns) = opt.enable_mdns { config.enable_mdns = enable_mdns; }
    // stream CLI arg always overrides config (unless it's the default "auto")
    if opt.stream != "auto" { config.stream = opt.stream.clone(); }

    // ---- 参数校验 ----
    if config.relay_multiaddrs().is_empty() && !config.enable_mdns {
        eprintln!("[Viewer] Error: no relay addresses and mDNS is disabled. Edit {} or use --relay / --enable-mdns", opt.config.display());
        std::process::exit(1);
    }
    {
        let relay_addrs = config.relay_multiaddrs();
        for (i, relay_str) in relay_addrs.iter().enumerate() {
            let label = if relay_addrs.len() == 1 { "Relay".to_string() } else { format!("Relay #{}", i + 1) };
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

    // GUI 模式: 左侧设备管理面板 + 右侧视频, 右键设备「连接」才连接
    #[cfg(feature = "player")]
    if config.play {
        return run_gui(opt, config).await;
    }

    // headless 模式: 直连配置中的 camera (旧行为)
    run_headless(opt, config).await
}

// ---- headless 模式 (无窗口: 保存文件/统计) ----

async fn run_headless(opt: Opt, config: ViewerConfig) -> Result<()> {
    let device_cam_str = match config.primary_camera() {
        Some(c) => c.to_string(),
        None => {
            eprintln!("[Viewer] Error: camera PeerId is empty. Edit {} or use --camera", opt.config.display());
            std::process::exit(1);
        }
    };

    let mut output_file = if let Some(path) = &config.output {
        Some(std::fs::File::create(path).context("Failed to create output file")?)
    } else {
        None
    };

    let relay_addrs = config.relay_multiaddrs();
    let no_audio = config.no_audio;
    let udp_port = config.udp_port.unwrap_or(0);
    let enable_mdns = config.enable_mdns;
    let stream_type = config.stream.clone();
    let network_type = config.network_type.clone();

    let mut frame_count: u64 = 0;
    let mut bytes_received: u64 = 0;
    let mut _audio_count: u64 = 0;
    let mut video_start: Option<std::time::Instant> = None;
    let mut stream_disconnected = false;
    // DCUtR 是否启用：默认启用（锥形/EIM NAT 可打洞成功，省中继带宽）。
    let enable_dcutr = network_type != "4g";

    // ---- 创建 MediaPlayer ----
    let mut viewer = MediaPlayer::new_with_options(
        enable_dcutr,
        ConnectOptions {
            udp_port,
            network_type: network_type.clone(),
            no_audio,
        },
    ).await?;

    println!("[Viewer] Receiving video frames... (Ctrl+C to stop)");

    // ---- 连接 (带重试) ----
    // 设备可能尚未在 relay 上完成 reservation, 或 relay 连接尚在重连中,
    // 初次拨号会失败 ("Relay has no reservation for destination")。
    // 这里重试而不是直接退出, 否则 viewer 会"异常退出"。
    let mut connect_attempt: u32 = 0;
    const MAX_CONNECT_ATTEMPTS: u32 = 60; // 最多重试 60 次 (约 60 * RECONNECT_DELAY)
    loop {
        connect_attempt += 1;
        match viewer
            .connect_with_options(
                &relay_addrs,
                &device_cam_str,
                enable_mdns,
                &config.serial_map,
                &stream_type,
                ConnectOptions {
                    udp_port,
                    network_type: network_type.clone(),
                    no_audio,
                },
            )
            .await
        {
            Ok(()) => {
                println!("[Viewer] Connected to DeviceCam (attempt #{})", connect_attempt);
                break;
            }
            Err(e) => {
                eprintln!("[Viewer] Connect attempt #{} failed: {}", connect_attempt, e);
                if connect_attempt >= MAX_CONNECT_ATTEMPTS {
                    anyhow::bail!(
                        "Failed to connect to device-cam after {} retries: {}",
                        connect_attempt,
                        e
                    );
                }
                eprintln!("[Viewer] Retrying in {}s...", RECONNECT_DELAY.as_secs());
                tokio::time::sleep(RECONNECT_DELAY).await;
            }
        }
    }

    // ---- 主循环: 驱动 Swarm + 消费帧 + 监控事件 ----
    loop {
        // 驱动 Swarm 事件循环（必须定期调用）
        viewer.poll_swarm().await;

        // 轮询事件
        while let Some(event) = viewer.poll_event() {
            match event {
                MediaPlayerEvent::Disconnected { reason }
                | MediaPlayerEvent::StreamEOF { reason } => {
                    if stream_disconnected {
                        continue;
                    }
                    eprintln!("[Viewer] Session lost: {reason}");
                    stream_disconnected = true;
                    eprintln!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
                    reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                    match viewer.reconnect().await {
                        Ok(()) => {
                            stream_disconnected = false;
                        }
                        Err(e) => {
                            eprintln!("[Viewer] Reconnect failed: {e}");
                        }
                    }
                }
                MediaPlayerEvent::DirectUpgraded { via_lan } => {
                    let via = if via_lan { "LAN direct" } else { "DCUtR hole punch" };
                    println!("[Viewer] Direct connection established via {via}, streams upgraded");
                }
                MediaPlayerEvent::NatDiagnosis { local_nat, remote_nat } => {
                    println!("[Viewer] NAT diagnosis: local={}, remote={}",
                        local_nat.short_name(),
                        remote_nat.as_deref().unwrap_or("Unknown"));
                }
                MediaPlayerEvent::DcutrBackoff => {
                    tracing::warn!("[Viewer] DCUtR backoff detected, disabling DCUtR for next reconnect");
                    viewer.set_enable_dcutr(false);
                }
            }
        }

        // 轮询视频帧
        while let Some(packet) = viewer.poll_video_frame() {
            if let Err(e) = handle_video_packet(
                &packet, &mut frame_count, &mut bytes_received, &mut video_start, &mut output_file,
            ) {
                eprintln!("[Viewer] Output write failed: {e}");
                if let Some(path) = &config.output {
                    println!("[Viewer] Output saved to: {}", path.display());
                    println!("[Viewer] Play with: ffplay -f hevc {}", path.display());
                }
                return Ok(());
            }
        }

        // 轮询音频帧
        while let Some(_packet) = viewer.poll_audio_frame() {
            _audio_count += 1;
        }

        // 短暂让出 CPU，避免 busy loop
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

// ---- GUI 模式 (设备管理面板 + SDL 播放) ----

#[cfg(feature = "player")]
async fn run_gui(opt: Opt, mut config: ViewerConfig) -> Result<()> {
    use player::UiAction;
    use std::time::Instant;

    println!("[Viewer] Initializing SDL player...");
    let mut player = player::VideoPlayer::new(config.device_list())?;

    let mut audio_player = if !config.no_audio {
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

    let relay_addrs = config.relay_multiaddrs();
    let no_audio = config.no_audio;
    let udp_port = config.udp_port.unwrap_or(0);
    let enable_mdns = config.enable_mdns;
    let stream_type = config.stream.clone();
    let network_type = config.network_type.clone();
    // DCUtR 是否启用：默认启用（锥形/EIM NAT 可打洞成功，省中继带宽）。
    let enable_dcutr = network_type != "4g";

    // 会话状态: 右键「连接」后才创建 MediaPlayer 并连接
    let mut session: Option<MediaPlayer> = None;
    let mut current_cam: Option<String> = None;   // 最近一次选择连接的设备
    let mut pending_cam: Option<String> = None;   // 待连接设备 (重试机制)
    let mut next_attempt = Instant::now();
    let mut attempt: u32 = 0;
    let mut stream_disconnected = false;
    let mut window_minimized = false;

    let mut frame_count: u64 = 0;
    let mut bytes_received: u64 = 0;
    let mut _audio_count: u64 = 0;
    let mut video_start: Option<std::time::Instant> = None;

    // 播放时钟 (PTS gating): 防止"收到即渲染"导致的忽快忽慢。
    // 把首帧 PTS 锚定到系统时钟, 之后每帧只在到期时渲染, 未到期帧暂存在 pending 池。
    let mut pending: std::collections::VecDeque<MediaPacket> = std::collections::VecDeque::new();
    let mut play_clock_anchor_ms: Option<u64> = None;   // 首帧 PTS
    let mut play_clock_anchor_inst: Option<std::time::Instant> = None; // 锚定时刻
    const TARGET_BUFFER_MS: u64 = 60;     // 目标缓冲, 吸收网络突发
    const MAX_BEHIND_MS: u64 = 300;       // 落后超此值加速追赶(不丢帧, 避免 P 帧花屏)

    player.set_status("Double-click a device to connect");
    println!("[Viewer] Window ready. Double-click a device in the left panel to connect.");

    loop {
        // ---- 1. UI 事件 ----
        for action in player.pump_events() {
            match action {
                UiAction::Quit => {
                    if let Some(path) = &config.output {
                        println!("[Viewer] Output saved to: {}", path.display());
                        println!("[Viewer] Play with: ffplay -f hevc {}", path.display());
                    }
                    return Ok(());
                }
                UiAction::ConnectDevice(cam) => {
                    println!("[Viewer] Device selected: {cam}");
                    if let Some(s) = session.as_mut() {
                        s.shutdown();
                    }
                    session = None;
                    stream_disconnected = false;
                    current_cam = Some(cam.clone());
                    pending_cam = Some(cam.clone());
                    attempt = 0;
                    next_attempt = Instant::now();
                    reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                    player.reset_decoder();
                    player.set_connected(None);
                    player.set_status(format!("Connecting {} ...", short_id(&cam)));
                }
                UiAction::DevicesChanged => {
                    // 设备列表统一存回 camera_serials (serial 与 PeerId 混用均可，连接时自动判定)
                    config.camera_serials = player.devices().to_vec();
                    match config.save(&opt.config) {
                        Ok(()) => println!("[Viewer] Config saved: {}", opt.config.display()),
                        Err(e) => eprintln!("[Viewer] Failed to save config: {e}"),
                    }
                }
                UiAction::DisconnectDevice => {
                    // 右键菜单"断开连接": 真正关闭底层 media 流
                    if let Some(s) = session.as_mut() {
                        s.shutdown();
                    }
                    session = None;
                    pending_cam = None;
                    current_cam = None;
                    stream_disconnected = true;
                    player.set_connected(None);
                    player.set_status("Disconnected");
                    println!("[Viewer] Disconnected by user");
                }
                UiAction::QuerySnapshots => {
                    if let Some(viewer) = session.as_mut() {
                        match viewer.list_snapshots().await {
                            Ok(files) => {
                                player.snapshot_files = files.clone();
                                player.selected_snapshot = None;
                                if files.is_empty() {
                                    player.set_status("无已合成的 AVI 文件".to_string());
                                } else {
                                    let msg = format!("已合成 {} 个 AVI: {}", files.len(), files.join(", "));
                                    player.set_status(msg);
                                    println!("[Viewer] Snapshots: {files:?}");
                                }
                            }
                            Err(e) => {
                                player.set_status(format!("查询失败: {e}"));
                                eprintln!("[Viewer] QuerySnapshots failed: {e}");
                            }
                        }
                    } else {
                        player.set_status("未连接，无法查询".to_string());
                    }
                }
                UiAction::DownloadSnapshot => {
                    if let Some(viewer) = session.as_mut() {
                        let idx = player.selected_snapshot;
                        if idx.is_none() || idx.unwrap() >= player.snapshot_files.len() {
                            player.set_status("请先在「系统」页选择要下载的文件".to_string());
                        } else {
                            let name = player.snapshot_files[idx.unwrap()].clone();
                            // 默认保存到当前工作目录下的 ./tmp 子目录
                            let save_dir = std::path::Path::new("./tmp");
                            let _ = std::fs::create_dir_all(save_dir);
                            let save = save_dir.join(&name);
                            // 先给出「下载中」反馈并立即刷新, 避免网络等待期间界面看似卡死
                            player.set_status(format!("下载中: {name} ..."));
                            player.draw_now();
                            match viewer.download_file(&name, &save).await {
                                Ok(n) => {
                                    player.set_status(format!("已下载 {} ({n} bytes) -> {}", name, save.display()));
                                    player.set_toast(format!("下载完成\n{name}  ({n} bytes)"));
                                    println!("[Viewer] Downloaded {name}: {n} bytes -> {}", save.display());
                                }
                                Err(e) => {
                                    player.set_status(format!("下载失败: {e}"));
                                    player.set_toast(format!("下载失败\n{e}"));
                                    eprintln!("[Viewer] DownloadFile failed: {e}");
                                }
                            }
                        }
                    } else {
                        player.set_status("未连接，无法下载".to_string());
                    }
                }
                UiAction::Minimized => {
                    window_minimized = true;
                    if let Some(s) = session.as_mut() {
                        // 最小化时关闭连接节约资源
                        s.shutdown();
                        stream_disconnected = true;
                    }
                    player.set_status("Minimized: connection closed");
                }
                UiAction::Restored => {
                    if !window_minimized {
                        continue;
                    }
                    window_minimized = false;
                    if session.is_some() {
                        eprintln!("[Viewer] Window restored, reconnecting...");
                        player.set_status("Reconnecting...");
                        player.draw_now();
                        reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                        player.reset_decoder();
                        let ok = match session.as_mut().unwrap().reconnect().await {
                            Ok(()) => {
                                stream_disconnected = false;
                                player.set_status("Connected");
                                true
                            }
                            Err(e) => {
                                eprintln!("[Viewer] Reconnect failed: {e}");
                                false
                            }
                        };
                        if !ok {
                            // 转入 pending 重试机制 (新建 MediaPlayer)
                            session = None;
                            pending_cam = current_cam.clone();
                            attempt = 0;
                            next_attempt = Instant::now() + RECONNECT_DELAY;
                        }
                    }
                }
            }
        }

        // ---- 1.5 配置窗体: 通过控制通道真正读取/下发编码参数 ----
        // 不依赖 is_config_open() 守卫: 保存按钮会立即关闭窗体(config_peer=None),
        // 若在此守卫内处理, apply/fetch 请求会在关闭后被丢弃, 导致下发不生效。
        {
            if let Some(_peer) = player.take_config_fetch_request() {
                if let Some(viewer) = session.as_mut() {
                    let stream = player.config_stream();
                    match viewer.get_encoder_config(&stream).await {
                        Ok(ec) => {
                            player.apply_encoder_config(ec);
                            player.set_status(format!("已读取 {} 编码参数", stream));
                        }
                        Err(e) => {
                            player.set_status(format!("读取编码参数失败: {e}"));
                            eprintln!("[Viewer] GetEncoderConfig failed: {e}");
                        }
                    }
                } else {
                    player.set_status("未连接，无法读取参数".to_string());
                }
            }
            if let Some((_peer, stream, ec)) = player.take_config_apply_request() {
                if let Some(viewer) = session.as_mut() {
                    match viewer.set_encoder_config(&stream, &ec).await {
                        Ok(()) => {
                            player.set_status(format!("编码参数已下发: {}", stream));
                            println!("[Viewer] Encoder config applied to '{stream}' stream");
                        }
                        Err(e) => {
                            player.set_status(format!("下发失败: {e}"));
                            eprintln!("[Viewer] SetEncoderConfig failed: {e}");
                        }
                    }
                } else {
                    player.set_status("未连接，无法下发参数".to_string());
                }
            }

            // ---- 1.6 配置窗体: 图像参数读取/下发 (控制通道) ----
            if let Some(_peer) = player.take_config_image_fetch_request() {
                if let Some(viewer) = session.as_mut() {
                    match viewer.get_image_config(0).await {
                        Ok(ic) => {
                            player.apply_image_config(ic);
                            player.set_status("已读取图像参数".to_string());
                        }
                        Err(e) => {
                            player.set_status(format!("读取图像参数失败: {e}"));
                            eprintln!("[Viewer] GetImageConfig failed: {e}");
                        }
                    }
                } else {
                    player.set_status("未连接，无法读取图像参数".to_string());
                }
            }
            if let Some((_peer, ic)) = player.take_config_image_apply_request() {
                if let Some(viewer) = session.as_mut() {
                    match viewer.set_image_config(0, &ic).await {
                        Ok(()) => player.set_status("图像参数已下发".to_string()),
                        Err(e) => player.set_status(format!("图像下发失败: {e}")),
                    }
                } else {
                    player.set_status("未连接，无法下发图像参数".to_string());
                }
            }
        }

        // ---- 2. 待连接设备 (带重试, 不阻塞 UI 退出) ----
        if !window_minimized && session.is_none() {
            if let Some(cam) = pending_cam.clone() {
                if Instant::now() >= next_attempt {
                    attempt += 1;
                    player.set_status(format!("Connecting {} (attempt {})", short_id(&cam), attempt));
                    player.draw_now(); // 拨号会阻塞事件循环, 先刷新 UI 显示状态
                    let result = async {
                        let mut v = MediaPlayer::new_with_options(
                            enable_dcutr,
                            ConnectOptions {
                                udp_port,
                                network_type: network_type.clone(),
                                no_audio,
                            },
                        ).await?;
                        v.connect_with_options(
                            &relay_addrs,
                            &cam,
                            enable_mdns,
                            &config.serial_map,
                            &stream_type,
                            ConnectOptions {
                                udp_port,
                                network_type: network_type.clone(),
                                no_audio,
                            },
                        ).await?;
                        Ok::<MediaPlayer, anyhow::Error>(v)
                    }.await;
                    match result {
                        Ok(v) => {
                            println!("[Viewer] Connected to DeviceCam (attempt #{attempt})");
                            session = Some(v);
                            pending_cam = None;
                            stream_disconnected = false;
                            player.set_connected(Some(&cam));
                            player.set_status("Connected");
                        }
                        Err(e) => {
                            eprintln!("[Viewer] Connect attempt #{attempt} failed: {e}");
                            player.set_status(format!("Connect failed ({attempt}), retry in {}s", RECONNECT_DELAY.as_secs()));
                            next_attempt = Instant::now() + RECONNECT_DELAY;
                        }
                    }
                }
            }
        }

        // ---- 3. 会话轮询: Swarm + 事件 + 帧 ----
        let mut drop_session = false;
        if let Some(viewer) = session.as_mut() {
            viewer.poll_swarm().await;

            let mut need_reconnect = false;
            while let Some(event) = viewer.poll_event() {
                match event {
                    MediaPlayerEvent::Disconnected { reason }
                    | MediaPlayerEvent::StreamEOF { reason } => {
                        if !stream_disconnected {
                            eprintln!("[Viewer] Session lost: {reason}");
                            stream_disconnected = true;
                            need_reconnect = true;
                        }
                    }
                    MediaPlayerEvent::DirectUpgraded { via_lan } => {
                        let via = if via_lan { "LAN direct" } else { "DCUtR hole punch" };
                        println!("[Viewer] Direct connection established via {via}, streams upgraded");
                        // 流从 sub 升级到 main 后，cam 侧会对新 main 流 request_idr 产出
                        // 新 GOP 的 IDR，但新流开头仍是 GOP 中段帧。重置解码器门控，等待
                        // 新 IDR 再解码，避免把无参考的 P 帧喂给 ffmpeg 导致
                        // "PPS id out of range" / 花屏 / 卡死（见 viewer.log 升级后的报错）。
                        player.reset_decoder();
                    }
                    MediaPlayerEvent::NatDiagnosis { local_nat, remote_nat } => {
                        println!("[Viewer] NAT diagnosis: local={}, remote={}",
                            local_nat.short_name(),
                            remote_nat.as_deref().unwrap_or("Unknown"));
                    }
                    MediaPlayerEvent::DcutrBackoff => {
                        tracing::warn!("[Viewer] DCUtR backoff detected, disabling DCUtR for next reconnect");
                        viewer.set_enable_dcutr(false);
                    }
                }
            }

            if need_reconnect && !window_minimized {
                eprintln!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
                player.set_status("Disconnected, reconnecting...");
                player.draw_now();
                reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                player.reset_decoder();
                match viewer.reconnect().await {
                    Ok(()) => {
                        stream_disconnected = false;
                        player.set_status("Connected");
                        // 重连后首帧重新锚定播放时钟
                        pending.clear();
                        play_clock_anchor_ms = None;
                        play_clock_anchor_inst = None;
                    }
                    Err(e) => {
                        eprintln!("[Viewer] Reconnect failed: {e}");
                        drop_session = true;
                    }
                }
            }

            // 轮询视频帧 —— PTS gating: 抽干 channel 进暂存池, 每轮只渲染已到期的队首帧
            while let Some(packet) = viewer.poll_video_frame() {
                pending.push_back(packet);
                // 暂存池容量保护: 超过 60 帧时优先丢弃非关键帧, 保关键帧
                if pending.len() > 60 {
                    if let Some(pos) = pending.iter().position(|p| !p.is_keyframe()) {
                        pending.remove(pos);
                    } else {
                        pending.pop_front();
                    }
                }
            }

            // 锚定播放时钟 (首帧)
            if play_clock_anchor_ms.is_none() {
                if let Some(front) = pending.front() {
                    play_clock_anchor_ms = Some(front.timestamp_ms);
                    play_clock_anchor_inst = Some(std::time::Instant::now());
                }
            }

            // 渲染所有已到期的帧 (按 PTS 顺序)
            while let Some(front) = pending.front() {
                let anchor_ms = match play_clock_anchor_ms {
                    Some(v) => v,
                    None => break,
                };
                let anchor_inst = match play_clock_anchor_inst {
                    Some(v) => v,
                    None => break,
                };
                let pts = front.timestamp_ms;
                // 目标呈现时刻 = now + (pts - anchor_ms) + 目标缓冲
                let elapsed_ms = anchor_inst.elapsed().as_millis() as u64;
                let target_at_ms = (pts.saturating_sub(anchor_ms)) + TARGET_BUFFER_MS;
                let behind = target_at_ms.saturating_sub(elapsed_ms);
                // 落后太多: 加速追赶, 不等满缓冲直接渲染(不丢帧)
                if behind > MAX_BEHIND_MS {
                    break; // 还没到期, 本轮先不渲染
                }

                let packet = pending.pop_front().unwrap();
                if let Err(e) = handle_video_packet(
                    &packet, &mut frame_count, &mut bytes_received, &mut video_start, &mut output_file,
                ) {
                    eprintln!("[Viewer] Output write failed: {e}");
                    return Ok(());
                }
                if let Err(e) = player.render(&packet.data, packet.is_keyframe()) {
                    tracing::error!("[Viewer] Player error: {e}");
                }
            }

            // 轮询音频帧
            while let Some(packet) = viewer.poll_audio_frame() {
                _audio_count += 1;
                if let Some(ap) = audio_player.as_mut() {
                    ap.write(&packet.data);
                }
            }
        }
        if drop_session {
            // 会话内重连失败, 转入 pending 机制新建 MediaPlayer 重试
            session = None;
            player.set_connected(None);
            pending_cam = current_cam.clone();
            attempt = 0;
            next_attempt = Instant::now() + RECONNECT_DELAY;
            // 重置播放时钟, 下次连接首帧重新锚定
            pending.clear();
            play_clock_anchor_ms = None;
            play_clock_anchor_inst = None;
        }

        // ---- 4. UI 重绘 (面板变化或空闲刷新) ----
        player.maybe_draw();

        // 短暂让出 CPU，避免 busy loop
        tokio::time::sleep(Duration::from_millis(1)).await;
    }
}

/// 重置帧率统计，在重连时调用
fn reset_stats(frame_count: &mut u64, bytes_received: &mut u64, video_start: &mut Option<std::time::Instant>) {
    *frame_count = 0;
    *bytes_received = 0;
    *video_start = None;
}

/// PeerId 截断显示: 前 8 + ".." + 后 6
#[cfg(feature = "player")]
fn short_id(s: &str) -> String {
    if s.len() > 20 && s.is_ascii() {
        format!("{}..{}", &s[..8], &s[s.len() - 6..])
    } else {
        s.to_string()
    }
}

/// 处理单个视频帧: 统计 + 可选写文件 (GUI 与 headless 共用)
fn handle_video_packet(
    packet: &MediaPacket,
    frame_count: &mut u64,
    bytes_received: &mut u64,
    video_start: &mut Option<std::time::Instant>,
    output_file: &mut Option<std::fs::File>,
) -> Result<()> {
    *frame_count += 1;
    *bytes_received += packet.data.len() as u64;

    // 第一帧到达时记录起始时间，用于 fps 计算
    if video_start.is_none() {
        *video_start = Some(std::time::Instant::now());
    }

    // 帧率统计
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
        file.write_all(&packet.data).context("write output file")?;
        let _ = file.flush();
    }

    Ok(())
}

// ---- SDL Player (player feature) ----

#[cfg(feature = "player")]
mod player {
    use std::collections::HashMap;
    use std::time::Instant;

    use anyhow::{Context, Result};
    use ffmpeg_next as ffmpeg;
    use sdl2::event::{Event, WindowEvent};
    use sdl2::keyboard::{Keycode, Mod};
    use sdl2::mouse::MouseButton;
    use sdl2::pixels::{Color, PixelFormatEnum};
    use sdl2::rect::Rect;
    use proto::control::{EncoderConfig, ImageConfig, ImageAdjustment};

    /// 左侧设备管理面板宽度 (px)
    pub const PANEL_W: u32 = 260;
    /// 设备列表首行 y
    const ROWS_Y0: i32 = 44;
    /// 设备行高
    const ROW_H: i32 = 24;

    // 面板配色 (RGBA) — 现代深色主题 (蓝强调, 与 Android 端对齐)
    const COL_BG: [u8; 4] = [22, 23, 29, 255];
    const COL_BORDER: [u8; 4] = [54, 57, 68, 255];
    const COL_ROW_SEL: [u8; 4] = [38, 64, 110, 255];
    const COL_BTN: [u8; 4] = [44, 47, 58, 255];
    const COL_BTN_HI: [u8; 4] = [62, 66, 80, 255];
    const COL_INPUT_BG: [u8; 4] = [14, 15, 20, 255];
    const COL_ACCENT_BG: [u8; 4] = [63, 110, 255, 255]; // #3F6EFF
    // 文字配色 (RGB)
    const COL_TEXT: [u8; 3] = [226, 228, 235];
    const COL_TEXT_DIM: [u8; 3] = [140, 145, 158];
    const COL_GREEN: [u8; 3] = [76, 208, 125]; // #4CD07D
    const COL_TITLE: [u8; 3] = [255, 255, 255];
    const COL_ACCENT: [u8; 3] = [120, 160, 255];

    /// UI 事件动作 (pump_events 返回给主循环)
    pub enum UiAction {
        /// 用户关闭窗口 / Esc
        Quit,
        /// 窗口最小化
        Minimized,
        /// 窗口从最小化恢复
        Restored,
        /// 右键菜单「连接」设备, 开始连接该 PeerId
        ConnectDevice(String),
        /// 设备列表已增删, 需要保存配置
        DevicesChanged,
        /// 右键菜单"断开连接": 真正关闭底层 media 流
        DisconnectDevice,
        /// 配置窗体"系统"页「查询」按钮: 列出已合成 AVI 文件
        QuerySnapshots,
        /// 配置窗体"系统"页「下载」按钮: 下载选中的 AVI 文件
        DownloadSnapshot,
    }

    // ============ 设备配置 (仅 viewer 端本地保存, 不真正下发到摄像头) ============
    // 字段按 RV1106 摄像头能力设计: 编码 / 图像 / 系统 三类。

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum CodecType { H265, H264 }
    impl CodecType {
        fn cycle(self) -> Self {
            match self { CodecType::H265 => CodecType::H264, CodecType::H264 => CodecType::H265 }
        }
        fn label(self) -> &'static str {
            match self { CodecType::H265 => "H.265", CodecType::H264 => "H.264" }
        }
        fn from_proto(s: &str) -> Self {
            if s == "H.264" { CodecType::H264 } else { CodecType::H265 }
        }
        fn to_proto(self) -> String {
            match self { CodecType::H265 => "H.265".to_string(), CodecType::H264 => "H.264".to_string() }
        }
    }

    #[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
    pub enum Resolution { R1080p, R720p, R540p, R360p }
    #[allow(dead_code)]
    impl Resolution {
        fn cycle(self) -> Self {
            match self {
                Resolution::R1080p => Resolution::R720p,
                Resolution::R720p => Resolution::R540p,
                Resolution::R540p => Resolution::R360p,
                Resolution::R360p => Resolution::R1080p,
            }
        }
        fn label(self) -> &'static str {
            match self {
                Resolution::R1080p => "1920x1080",
                Resolution::R720p => "1280x720",
                Resolution::R540p => "960x540",
                Resolution::R360p => "640x360",
            }
        }
        fn from_wh(w: u32, h: u32) -> Self {
            match (w, h) {
                (1920, 1080) => Resolution::R1080p,
                (1280, 720) => Resolution::R720p,
                (960, 540) => Resolution::R540p,
                (640, 360) => Resolution::R360p,
                _ => Resolution::R720p,
            }
        }
        fn to_wh(self) -> (u32, u32) {
            match self {
                Resolution::R1080p => (1920, 1080),
                Resolution::R720p => (1280, 720),
                Resolution::R540p => (960, 540),
                Resolution::R360p => (640, 360),
            }
        }
    }

    /// 右键菜单动作
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum CtxMenuAct { Connect, Disconnect, Configure }

    /// 可编辑配置字段标识 (用于布局/点击命中)
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    enum ConfigField {
        Stream, Codec, Resolution, RcMode, Fps, Bitrate, Gop,
        Brightness, Contrast, Saturation, Sharpness,
        DeviceName,
    }

    /// 配置窗体几何布局 (draw 与 click 共用, 保证命中一致)
    struct CfgGeo {
        win: (i32, i32, i32, i32),
        tabs: [(i32, i32, i32, i32); 3],
        /// (字段, 减号按钮, 值区域, 加号按钮)
        rows: Vec<(ConfigField, (i32, i32, i32, i32), (i32, i32, i32, i32), (i32, i32, i32, i32))>,
        /// (保存, 取消, 默认)
        buttons: [(i32, i32, i32, i32); 3],
        /// 系统页专属: (查询, 下载) 两个按钮
        snap_buttons: [(i32, i32, i32, i32); 2],
        /// 系统页专属: 文件列表区域 (命中检测用)
        snap_list: (i32, i32, i32, i32),
    }

    const CFG_W: i32 = 480;
    const CFG_H: i32 = 460;

    #[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
    pub struct DeviceConfig {
        // ---- 编码参数 ----
        pub codec: CodecType,
        pub resolution: Resolution,
        #[serde(default = "default_rc_mode")]
        pub rc_mode: String,
        pub fps: u32,
        pub bitrate_kbps: u32,
        pub gop: u32,
        // ---- 图像参数 ----
        pub brightness: i32,
        pub contrast: i32,
        pub saturation: i32,
        #[serde(default)]
        pub sharpness: i32,
        // ---- 系统参数 ----
        pub device_name: String,
    }

    fn default_rc_mode() -> String { "CBR".to_string() }

    impl Default for DeviceConfig {
        fn default() -> Self {
            DeviceConfig {
                codec: CodecType::H265,
                resolution: Resolution::R720p,
                rc_mode: "CBR".to_string(),
                fps: 25,
                bitrate_kbps: 2000,
                gop: 50,
                brightness: 0,
                contrast: 0,
                saturation: 0,
                sharpness: 0,
                device_name: "Camera".to_string(),
            }
        }
    }

    impl DeviceConfig {
        fn path(peer: &str) -> std::path::PathBuf {
            let mut dir = std::env::current_dir().unwrap_or_default();
            dir.push("device_configs");
            let _ = std::fs::create_dir_all(&dir);
            dir.push(format!("{}.json", peer));
            dir
        }
        fn load(peer: &str) -> DeviceConfig {
            let p = DeviceConfig::path(peer);
            std::fs::read_to_string(&p)
                .ok()
                .and_then(|s| serde_json::from_str(&s).ok())
                .unwrap_or_default()
        }
        fn save(&self, peer: &str) {
            let p = DeviceConfig::path(peer);
            if let Ok(s) = serde_json::to_string_pretty(self) {
                let _ = std::fs::write(&p, s);
            }
        }
    }

    /// 编码参数默认值 (当摄像头未返回或读取失败时, 用于构造完整 EncoderConfig)
    fn default_encoder() -> EncoderConfig {
        EncoderConfig {
            output_data_type: "H.265".to_string(),
            width: 1280,
            height: 720,
            rc_mode: "CBR".to_string(),
            rc_quality: "high".to_string(),
            gop: 50,
            gop_mode: "normalP".to_string(),
            max_rate: 2000,
            dst_frame_rate_num: 25,
            dst_frame_rate_den: 1,
            h264_profile: "high".to_string(),
            smart: "close".to_string(),
            rotation: 0,
        }
    }

    /// 将摄像头返回的 EncoderConfig 转换为 UI 用的 DeviceConfig (保留图像/系统字段)
    fn device_config_from_encoder(ec: &EncoderConfig, base: DeviceConfig) -> DeviceConfig {
        DeviceConfig {
            codec: CodecType::from_proto(&ec.output_data_type),
            resolution: Resolution::from_wh(ec.width, ec.height),
            rc_mode: ec.rc_mode.clone(),
            fps: ec.dst_frame_rate_num,
            bitrate_kbps: ec.max_rate,
            gop: ec.gop,
            ..base
        }
    }

    /// 将 sdl2 的各种错误类型统一转为 anyhow::Error
    fn map_sdl<T, E: std::string::ToString>(r: std::result::Result<T, E>, ctx: &str) -> Result<T> {
        r.map_err(|e| anyhow::anyhow!("SDL {ctx}: {}", e.to_string()))
    }

    fn btn_add_rect(h: u32) -> (i32, i32, i32, i32) { (8, h as i32 - 44, 118, 30) }
    fn btn_del_rect(h: u32) -> (i32, i32, i32, i32) { (134, h as i32 - 44, 118, 30) }
    fn rows_end(h: u32) -> i32 { (h as i32 - 130).max(ROWS_Y0) }
    fn in_rect(px: i32, py: i32, r: (i32, i32, i32, i32)) -> bool {
        px >= r.0 && px < r.0 + r.2 && py >= r.1 && py < r.1 + r.3
    }

    /// 在 RGBA 缓冲上填充矩形 (带裁剪)
    fn fill_rect(buf: &mut [u8], buf_w: u32, buf_h: u32, x: i32, y: i32, w: u32, h: u32, c: [u8; 4]) {
        let x0 = x.max(0) as u32;
        let y0 = y.max(0) as u32;
        let x1 = ((x + w as i32).max(0) as u32).min(buf_w);
        let y1 = ((y + h as i32).max(0) as u32).min(buf_h);
        for py in y0..y1 {
            for px in x0..x1 {
                let idx = ((py * buf_w + px) * 4) as usize;
                buf[idx..idx + 4].copy_from_slice(&c);
            }
        }
    }

    /// 判断点是否落在以 (cx,cy) 为圆心、半径 r 的圆内 (含边界)
    fn in_circle(px: i32, py: i32, cx: i32, cy: i32, r: i32) -> bool {
        let dx = px - cx;
        let dy = py - cy;
        dx * dx + dy * dy <= r * r
    }

    /// 圆角矩形填充 (带裁剪): 四角 r×r 区域按内切圆外剔除, 形成圆角。
    /// r<=0 退化为普通矩形。
    fn fill_rect_r(buf: &mut [u8], buf_w: u32, buf_h: u32, x: i32, y: i32, w: u32, h: u32, r: u32, c: [u8; 4]) {
        let r = (r as i32).min(w as i32 / 2).min(h as i32 / 2).max(0);
        let x0 = (x.max(0)) as u32;
        let y0 = (y.max(0)) as u32;
        let x1 = ((x + w as i32).max(0) as u32).min(buf_w);
        let y1 = ((y + h as i32).max(0) as u32).min(buf_h);
        if r == 0 {
            for py in y0..y1 {
                for px in x0..x1 {
                    let idx = ((py * buf_w + px) * 4) as usize;
                    buf[idx..idx + 4].copy_from_slice(&c);
                }
            }
            return;
        }
        let (xr, yr, wr, hr) = (x + r, y + r, x + w as i32 - r, y + h as i32 - r);
        for py in y0..y1 {
            for px in x0..x1 {
                let skip = (px < xr as u32 && py < yr as u32 && !in_circle(px as i32, py as i32, xr, yr, r))
                    || (px >= wr as u32 && py < yr as u32 && !in_circle(px as i32, py as i32, wr, yr, r))
                    || (px < xr as u32 && py >= hr as u32 && !in_circle(px as i32, py as i32, xr, hr, r))
                    || (px >= wr as u32 && py >= hr as u32 && !in_circle(px as i32, py as i32, wr, hr, r));
                if skip {
                    continue;
                }
                let idx = ((py * buf_w + px) * 4) as usize;
                buf[idx..idx + 4].copy_from_slice(&c);
            }
        }
    }

    /// 加载系统 TTF 字体 (fontdue 纯 Rust 光栅化, 无需 SDL2_ttf)
    fn load_font() -> Result<fontdue::Font> {
        const CANDIDATES: &[&str] = &[
            // Windows
            "C:\\Windows\\Fonts\\consola.ttf",
            "C:\\Windows\\Fonts\\cour.ttf",
            "C:\\Windows\\Fonts\\arial.ttf",
            "C:\\Windows\\Fonts\\segoeui.ttf",
            // Linux
            "/usr/share/fonts/truetype/dejavu/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/dejavu/DejaVuSans.ttf",
            "/usr/share/fonts/TTF/DejaVuSansMono.ttf",
            "/usr/share/fonts/truetype/liberation/LiberationMono-Regular.ttf",
            "/usr/share/fonts/truetype/freefont/FreeMono.ttf",
        ];
        for p in CANDIDATES {
            if let Ok(bytes) = std::fs::read(p) {
                if let Ok(f) = fontdue::Font::from_bytes(bytes, fontdue::FontSettings::default()) {
                    println!("[Player] UI font: {p}");
                    return Ok(f);
                }
            }
        }
        anyhow::bail!("No usable TTF font found for UI panel (tried: {CANDIDATES:?})")
    }

    /// 加载中文 fallback 字体 (主等宽字体不含 CJK 字形, 中文需回退到此字体)
    fn load_cjk_font() -> Option<fontdue::Font> {
        const CANDIDATES: &[&str] = &[
            // Windows (微软雅黑 / 宋体)
            "C:\\Windows\\Fonts\\msyh.ttc",
            "C:\\Windows\\Fonts\\msyh.ttf",
            "C:\\Windows\\Fonts\\simsun.ttc",
            "C:\\Windows\\Fonts\\simhei.ttf",
            // Linux
            "/usr/share/fonts/truetype/wqy/wqy-microhei.ttc",
            "/usr/share/fonts/truetype/droid/DroidSansFallbackFull.ttf",
            "/usr/share/fonts/opentype/noto/NotoSansCJK-Regular.ttc",
            "/usr/share/fonts/truetype/arphic/uming.ttc",
        ];
        for p in CANDIDATES {
            if let Ok(bytes) = std::fs::read(p) {
                // .ttc 字体集合: 尝试 index 0 (多数中文 TTC 的 index 0 即中文)
                for idx in 0..=2u32 {
                    if let Ok(f) = fontdue::Font::from_bytes(
                        bytes.clone(),
                        fontdue::FontSettings { collection_index: idx, ..fontdue::FontSettings::default() },
                    ) {
                        // 抽样「中」字能光栅化即认为可用
                        let (m, _) = f.rasterize('中', 14.0);
                        if m.width > 0 && m.height > 0 {
                            println!("[Player] CJK font: {p} (index {idx})");
                            return Some(f);
                        }
                    }
                }
            }
        }
        eprintln!("[Player] WARNING: no CJK font found, Chinese text will be blank");
        None
    }

    /// H.265 解码 + SDL2 渲染的实时播放器 (左侧设备管理面板 + 右侧视频)
    ///
    /// SAFETY: `texture`/`panel_texture` 字段使用 `Texture<'static>`，实际生命周期
    /// 绑定到 `canvas`。Rust 保证 struct 字段按声明顺序 drop，因此 texture (在前)
    /// 先于 canvas drop。
    pub struct VideoPlayer {
        texture: Option<sdl2::render::Texture<'static>>,
        panel_texture: Option<sdl2::render::Texture<'static>>,
        canvas: sdl2::render::Canvas<sdl2::video::Window>,
        video_subsystem: sdl2::VideoSubsystem,
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
        /// 显示屏可用区域最大尺寸，用于限制窗口初始尺寸不超过显示屏
        display_max: (u32, u32),
        /// 是否已根据视频分辨率设置过初始窗口尺寸（只在首帧设置一次，之后尊重用户拖动/最大化）
        window_sized: bool,
        /// 是否已执行过运行时 maximize()：在首次 present 后调用一次，确保窗口启动即最大化
        maximize_applied: bool,

        // ---- 右键上下文菜单 / 配置窗体状态 ----
        /// 右键弹出的上下文菜单: (x, y, 选中设备索引)
        context_menu: Option<(i32, i32, usize)>,
        /// 配置窗体正在编辑的设备 PeerId
        config_peer: Option<String>,
        /// 配置窗体当前编辑的配置 (None = 未打开)
        config: Option<DeviceConfig>,
        /// 配置窗体当前 tab: 0 编码 / 1 图像 / 2 系统
        config_tab: usize,
        /// 系统 tab 中设备名是否处于文本编辑态
        config_editing_name: bool,
        /// 设备名文本编辑缓冲
        config_text: String,

        // ---- 编码参数读取/下发 (控制通道) 状态 ----
        /// 当前编辑的码流: main / sub / third
        config_stream: String,
        /// 从摄像头读取的完整编码配置 (保留未编辑字段, 下发时回写)
        encoder_raw: Option<EncoderConfig>,
        /// 待从摄像头读取编码参数的设备 PeerId (主循环异步处理)
        config_fetch_pending: Option<String>,
        /// 待下发到摄像头的编码参数 (peer, stream, config)
        config_apply_pending: Option<(String, String, EncoderConfig)>,

        // ---- 图像参数读取/下发 (控制通道) 状态 ----
        /// 从摄像头读取的完整图像配置 (保留未编辑字段, 下发时回写)
        image_raw: Option<ImageConfig>,
        /// 待从摄像头读取图像参数的设备 PeerId (主循环异步处理)
        config_image_fetch_pending: Option<String>,
        /// 待下发到摄像头的图像参数 (peer, config)
        config_image_apply_pending: Option<(String, ImageConfig)>,

        // ---- 抓拍 / 延时摄影 (控制通道) 状态 ----
        /// 最近一次查询返回的 AVI 文件列表
        pub snapshot_files: Vec<String>,
        /// 系统页文件列表中当前选中的索引 (供「下载」按钮使用)
        pub selected_snapshot: Option<usize>,

        /// 右键菜单 / 配置窗体共用的 overlay 纹理 (ABGR8888)
        overlay_texture: Option<sdl2::render::Texture<'static>>,

        // ---- 设备管理面板状态 ----
        font: fontdue::Font,
        /// 中文 fallback 字体 (主字体不含 CJK 字形时使用)
        font_cjk: Option<fontdue::Font>,
        glyph_cache: HashMap<(char, u32), (fontdue::Metrics, Vec<u8>)>,
        devices: Vec<String>,
        selected: Option<usize>,
        connected_cam: Option<String>,
        adding: bool,
        input: String,
        status: String,
        /// 瞬时提示: 文字 + 过期时刻 (过期后不再绘制). 用于下载完成等一次性反馈.
        toast: Option<(String, Instant)>,
        panel_dirty: bool,
        panel_h: u32,
        last_present: Instant,
    }

    impl VideoPlayer {
        pub fn new(devices: Vec<String>) -> Result<Self> {
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

            // 显示屏可用区域(排除任务栏), 用于限制窗口初始及最大尺寸不超过屏幕
            let display_max = video_subsystem
                .display_usable_bounds(0)
                .map(|r| (r.width(), r.height()))
                .unwrap_or((1920, 1080));

            // 初始窗口尺寸: 面板 + 1280x720 视频区, 不超过屏幕可用区域
            let init_w = (1280 + PANEL_W).min(display_max.0).max(PANEL_W + 320);
            let init_h = 720u32.min(display_max.1);

            let window = video_subsystem
                .window("P2P Camera Viewer", init_w, init_h)
                .position_centered()
                .resizable()
                .maximized()
                .build()
                .map_err(|e| anyhow::anyhow!("SDL window: {e}"))?;
            let mut canvas = window
                .into_canvas()
                .accelerated()
                .present_vsync()
                .build()
                .map_err(|e| anyhow::anyhow!("SDL canvas: {e}"))?;
            // 确保窗口在软件启动时即最大化。WindowBuilder::maximized() 标志在部分
            // 窗口管理器(尤其 Linux/X11)上不会让窗口一出现就真正最大化，必须在
            // build 后通过运行时 maximize() 强制生效。
            canvas.window_mut().maximize();

            let event_pump = map_sdl(sdl_context.event_pump(), "event_pump")?;

            let font = load_font()?;

            Ok(Self {
                texture: None,
                panel_texture: None,
                canvas,
                video_subsystem,
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
                display_max,
                window_sized: false,
                maximize_applied: false,
                context_menu: None,
                config_peer: None,
                config: None,
                config_tab: 0,
                config_editing_name: false,
                config_text: String::new(),
                config_stream: "main".to_string(),
                encoder_raw: None,
                config_fetch_pending: None,
                config_apply_pending: None,
                image_raw: None,
                config_image_fetch_pending: None,
                config_image_apply_pending: None,
                snapshot_files: Vec::new(),
                selected_snapshot: None,
                overlay_texture: None,
                font,
                font_cjk: load_cjk_font(),
                glyph_cache: HashMap::new(),
                devices,
                selected: None,
                connected_cam: None,
                adding: false,
                input: String::new(),
                status: String::new(),
                toast: None,
                panel_dirty: true,
                panel_h: 0,
                last_present: Instant::now(),
            })
        }

        // ---- 面板状态访问 ----

        pub fn devices(&self) -> &[String] {
            &self.devices
        }

        pub fn set_connected(&mut self, cam: Option<&str>) {
            self.connected_cam = cam.map(|s| s.to_string());
            self.panel_dirty = true;
        }

        pub fn set_status(&mut self, s: impl Into<String>) {
            self.status = s.into();
            self.panel_dirty = true;
        }

        /// 设置瞬时提示, 默认 3 秒后自动消失
        pub fn set_toast(&mut self, s: impl Into<String>) {
            self.toast = Some((s.into(), Instant::now() + std::time::Duration::from_secs(3)));
            self.panel_dirty = true;
        }

        // ---- 编码参数控制通道辅助 ----

        /// 当前编辑的码流
        pub fn config_stream(&self) -> String {
            self.config_stream.clone()
        }

        /// 取出待读取请求 (取出后清空)
        pub fn take_config_fetch_request(&mut self) -> Option<String> {
            self.config_fetch_pending.take()
        }

        /// 取出待下发请求 (取出后清空)
        pub fn take_config_apply_request(&mut self) -> Option<(String, String, EncoderConfig)> {
            self.config_apply_pending.take()
        }

        /// 用从摄像头读取到的编码配置刷新 UI 显示 (保留图像/系统字段)
        pub fn apply_encoder_config(&mut self, ec: EncoderConfig) {
            self.encoder_raw = Some(ec.clone());
            let base = self.config.clone().unwrap_or_default();
            self.config = Some(device_config_from_encoder(&ec, base));
            self.panel_dirty = true;
        }

        /// 根据 UI 编辑结果构造待下发的完整 EncoderConfig
        /// (分辨率当前只读, 保留摄像头原始值; 其余未暴露字段沿用摄像头返回值)
        fn build_encoder_config(&self) -> Option<EncoderConfig> {
            let cfg = self.config.as_ref()?;
            let mut ec = self.encoder_raw.clone().unwrap_or_else(default_encoder);
            ec.output_data_type = cfg.codec.to_proto();
            ec.rc_mode = cfg.rc_mode.clone();
            let (w, h) = cfg.resolution.to_wh();
            ec.width = w;
            ec.height = h;
            ec.dst_frame_rate_num = cfg.fps;
            ec.dst_frame_rate_den = 1;
            ec.max_rate = cfg.bitrate_kbps;
            ec.gop = cfg.gop;
            Some(ec)
        }

        /// 用从摄像头读取到的图像配置刷新 UI 显示 (亮度/对比度/饱和度/锐度)
        pub fn apply_image_config(&mut self, ic: ImageConfig) {
            self.image_raw = Some(ic.clone());
            let mut cfg = self.config.clone().unwrap_or_default();
            if let Some(adj) = ic.adjustment {
                if let Some(v) = adj.brightness { cfg.brightness = v; }
                if let Some(v) = adj.contrast { cfg.contrast = v; }
                if let Some(v) = adj.saturation { cfg.saturation = v; }
                if let Some(v) = adj.sharpness { cfg.sharpness = v; }
            }
            self.config = Some(cfg);
            self.panel_dirty = true;
        }

        /// 取出待读取请求 (取出后清空)
        pub fn take_config_image_fetch_request(&mut self) -> Option<String> {
            self.config_image_fetch_pending.take()
        }

        /// 取出待下发请求 (取出后清空)
        pub fn take_config_image_apply_request(&mut self) -> Option<(String, ImageConfig)> {
            self.config_image_apply_pending.take()
        }

        /// 根据 UI 编辑结果构造待下发的完整 ImageConfig
        fn build_image_config(&self) -> Option<ImageConfig> {
            let cfg = self.config.as_ref()?;
            let adjustment = ImageAdjustment {
                contrast: Some(cfg.contrast),
                brightness: Some(cfg.brightness),
                saturation: Some(cfg.saturation),
                sharpness: Some(cfg.sharpness),
                hue: None,
            };
            Some(ImageConfig {
                adjustment: Some(adjustment),
                exposure: None,
                night_to_day: None,
                white_balance: None,
                enhancement: None,
                video_adjustment: None,
            })
        }

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

        // ---- 事件处理 ----

        /// 轮询所有 SDL 事件 (窗口/鼠标/键盘/文本输入), 返回需要主循环处理的动作
        pub fn pump_events(&mut self) -> Vec<UiAction> {
            let mut actions = Vec::new();
            let events: Vec<Event> = self.event_pump.poll_iter().collect();
            for event in events {
                match event {
                    Event::Quit { .. } => actions.push(UiAction::Quit),
                    Event::KeyDown { keycode: Some(key), keymod, .. } => {
                        if self.config_editing_name {
                            // 配置窗体: 设备名文本编辑
                            match key {
                                Keycode::Return | Keycode::KpEnter => {
                                    if let Some(cfg) = self.config.as_mut() {
                                        cfg.device_name = self.config_text.trim().to_string();
                                    }
                                    self.config_editing_name = false;
                                    self.video_subsystem.text_input().stop();
                                    self.panel_dirty = true;
                                    let _ = self.draw_now();
                                }
                                Keycode::Escape => {
                                    self.config_editing_name = false;
                                    self.video_subsystem.text_input().stop();
                                    let _ = self.draw_now();
                                }
                                Keycode::Backspace => {
                                    self.config_text.pop();
                                    let _ = self.draw_now();
                                }
                                Keycode::V if keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD) => {
                                    if let Ok(t) = self.video_subsystem.clipboard().clipboard_text() {
                                        self.config_text.push_str(&t);
                                        let _ = self.draw_now();
                                    }
                                }
                                _ => {}
                            }
                        } else if self.config.is_some() {
                            // 配置窗体打开 (非编辑): Esc 关闭(取消)
                            if key == Keycode::Escape {
                                self.config = None;
                                self.config_peer = None;
                                self.panel_dirty = true;
                                let _ = self.draw_now();
                            }
                        } else if self.adding {
                            match key {
                                Keycode::Return | Keycode::KpEnter => {
                                    if let Some(a) = self.finish_add() {
                                        actions.push(a);
                                    }
                                }
                                Keycode::Escape => self.cancel_add(),
                                Keycode::Backspace => {
                                    self.input.pop();
                                    self.panel_dirty = true;
                                }
                                Keycode::V if keymod.intersects(Mod::LCTRLMOD | Mod::RCTRLMOD) => {
                                    if let Ok(t) = self.video_subsystem.clipboard().clipboard_text() {
                                        let t: String = t.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                                        self.input.push_str(&t);
                                        self.panel_dirty = true;
                                    }
                                }
                                _ => {}
                            }
                        } else if key == Keycode::Escape {
                            actions.push(UiAction::Quit);
                        }
                    }
                    Event::TextInput { text, .. } if self.adding => {
                        let t: String = text.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
                        self.input.push_str(&t);
                        self.panel_dirty = true;
                    }
                    Event::TextInput { text, .. } if self.config_editing_name => {
                        self.config_text.push_str(&text);
                        self.panel_dirty = true;
                        let _ = self.draw_now();
                    }
                    Event::MouseButtonDown { mouse_btn, x, y, .. } => {
                        match mouse_btn {
                            MouseButton::Left => {
                                if self.config.is_some() {
                                    // 配置窗体模态: 所有左键交给配置窗体处理
                                    self.on_config_click(x, y, &mut actions);
                                } else if self.context_menu.is_some() {
                                    // 菜单内命中执行对应动作, 否则关闭菜单
                                    if let Some(act) = self.on_context_menu_click(x, y) {
                                        match act {
                                            CtxMenuAct::Connect => {
                                                if let Some((_, _, idx)) = self.context_menu {
                                                    if idx < self.devices.len() {
                                                        actions.push(UiAction::ConnectDevice(self.devices[idx].clone()));
                                                    }
                                                }
                                            }
                                            CtxMenuAct::Disconnect => actions.push(UiAction::DisconnectDevice),
                                            CtxMenuAct::Configure => self.open_config(),
                                        }
                                    }
                                    self.context_menu = None;
                                    let _ = self.draw_now();
                                } else if (x as u32) < PANEL_W {
                                    self.on_panel_click(x, y, &mut actions);
                                }
                            }
                            MouseButton::Right => {
                                if (x as u32) < PANEL_W {
                                    self.open_context_menu(x, y);
                                    let _ = self.draw_now();
                                } else {
                                    self.context_menu = None;
                                }
                            }
                            _ => {}
                        }
                    }
                    Event::Window { win_event, .. } => match win_event {
                        WindowEvent::Minimized => {
                            eprintln!("[Player] SDL window event: Minimized");
                            self.minimized = true;
                            actions.push(UiAction::Minimized);
                        }
                        WindowEvent::Restored => {
                            // 注意: 取消最大化也会触发 Restored, 仅在此前确实最小化过才上报
                            if self.minimized {
                                eprintln!("[Player] SDL window event: Restored");
                                self.minimized = false;
                                actions.push(UiAction::Restored);
                            }
                            self.panel_dirty = true;
                        }
                        WindowEvent::SizeChanged(..) | WindowEvent::Resized(..) | WindowEvent::Maximized => {
                            self.panel_dirty = true;
                        }
                        _ => {}
                    },
                    _ => {}
                }
            }
            actions
        }

        /// 面板区域鼠标点击: 设备行选中, 按钮 Add/Del (连接/断开/配置见右键菜单)
        fn on_panel_click(&mut self, x: i32, y: i32, actions: &mut Vec<UiAction>) {
            let (_, h) = self.canvas.window().size();

            if in_rect(x, y, btn_add_rect(h)) {
                self.start_add();
                return;
            }
            if in_rect(x, y, btn_del_rect(h)) {
                if self.adding {
                    return;
                }
                if let Some(i) = self.selected {
                    if i < self.devices.len() {
                        let removed = self.devices.remove(i);
                        self.selected = if self.devices.is_empty() {
                            None
                        } else {
                            Some(i.min(self.devices.len() - 1))
                        };
                        self.status = format!("Deleted {}", super::short_id(&removed));
                        self.panel_dirty = true;
                        actions.push(UiAction::DevicesChanged);
                    }
                } else {
                    self.status = "Select a device to delete".to_string();
                    self.panel_dirty = true;
                }
                return;
            }

            // 设备行: 单击选中 (连接/断开/配置见右键菜单)
            if y >= ROWS_Y0 && y < rows_end(h) {
                let idx = ((y - ROWS_Y0) / ROW_H) as usize;
                if idx < self.devices.len() {
                    self.selected = Some(idx);
                    self.panel_dirty = true;
                }
            }
        }

        fn start_add(&mut self) {
            self.adding = true;
            self.input.clear();
            self.video_subsystem.text_input().start();
            self.status = "Enter=OK  Esc=Cancel  输入16位序列号 SN (Ctrl+V粘贴)".to_string();
            self.panel_dirty = true;
        }

        fn cancel_add(&mut self) {
            self.adding = false;
            self.input.clear();
            self.video_subsystem.text_input().stop();
            self.status = "Add cancelled".to_string();
            self.panel_dirty = true;
        }

        fn finish_add(&mut self) -> Option<UiAction> {
            let cam = self.input.trim().to_string();
            if cam.is_empty() {
                self.cancel_add();
                return None;
            }
            // 校验: 16位序列号 SN 或完整 PeerId（连接时 Rust 侧自动判定）
            let is_serial = cam.len() == 16 && cam.chars().all(|c| c.is_ascii_hexdigit());
            let is_peerid = cam.len() >= 40 && cam.chars().all(|c| c.is_ascii_alphanumeric());
            if !is_serial && !is_peerid {
                self.status = "Invalid: 需要 16位序列号 或 完整 PeerId".to_string();
                self.panel_dirty = true;
                return None; // 停留在输入态, 让用户修改
            }
            if self.devices.contains(&cam) {
                self.status = "Device already exists".to_string();
                self.panel_dirty = true;
                return None;
            }
            self.devices.push(cam);
            self.selected = Some(self.devices.len() - 1);
            self.adding = false;
            self.input.clear();
            self.video_subsystem.text_input().stop();
            self.status = "Device added (右键选择「连接」开始播放)".to_string();
            self.panel_dirty = true;
            Some(UiAction::DevicesChanged)
        }

        // ============ 右键上下文菜单 ============

        /// 右键面板设备区域: 若已选中设备则弹出菜单 (记录弹出坐标与选中索引)
        fn open_context_menu(&mut self, x: i32, y: i32) {
            if let Some(idx) = self.selected {
                if idx < self.devices.len() {
                    self.context_menu = Some((x, y, idx));
                    self.panel_dirty = true;
                }
            }
        }

        /// 右键菜单命中检测: 返回所点动作 (None = 点中菜单外, 直接关闭)
        fn on_context_menu_click(&self, x: i32, y: i32) -> Option<CtxMenuAct> {
            let (mx, my, _) = self.context_menu?;
            let w = 150i32;
            let items = [CtxMenuAct::Connect, CtxMenuAct::Disconnect, CtxMenuAct::Configure];
            for (i, act) in items.iter().enumerate() {
                let ry = my + (i as i32) * 26;
                if x >= mx && x < mx + w && y >= ry && y < ry + 26 {
                    return Some(*act);
                }
            }
            None
        }

        // ============ 配置窗体 ============

        /// 打开配置窗体: 载入该设备的本地配置 (不真正下发到摄像头)
        fn open_config(&mut self) {
            if let Some(idx) = self.selected {
                if idx < self.devices.len() {
                    let peer = self.devices[idx].clone();
                    let cfg = DeviceConfig::load(&peer);
                    self.config_peer = Some(peer.clone());
                    self.config = Some(cfg);
                    self.config_stream = "main".to_string();
                    self.encoder_raw = None;
                    self.image_raw = None;
                    self.config_image_fetch_pending = Some(peer.clone());
                    self.config_tab = 0;
                    self.config_editing_name = false;
                    self.config_text.clear();
                    // 打开即真正从摄像头读取当前编码参数 (而非仅本地缓存)
                    self.config_fetch_pending = Some(peer);
                    self.panel_dirty = true;
                }
            }
        }

        /// 配置窗体几何布局 (绝对坐标: win 用于 copy, tabs/rows/buttons 相对窗体原点)
        fn config_geometry(&self) -> CfgGeo {
            let (win_w, win_h) = self.canvas.window().size();
            let wx = (win_w as i32 - CFG_W) / 2;
            let wy = (win_h as i32 - CFG_H) / 2;
            let win = (wx, wy, CFG_W, CFG_H);
            let x0 = 0i32;
            let y0 = 0i32;
            let tab_w = 140;
            let tab_h = 26;
            let tab_y = y0 + 34;
            let tabs = [
                (x0 + 12, tab_y, tab_w, tab_h),
                (x0 + 12 + tab_w + 8, tab_y, tab_w, tab_h),
                (x0 + 12 + (tab_w + 8) * 2, tab_y, tab_w, tab_h),
            ];
            let fields: Vec<ConfigField> = match self.config_tab {
                0 => vec![ConfigField::Stream, ConfigField::Codec, ConfigField::Resolution, ConfigField::Fps, ConfigField::Bitrate, ConfigField::RcMode, ConfigField::Gop],
                1 => vec![ConfigField::Brightness, ConfigField::Contrast, ConfigField::Saturation, ConfigField::Sharpness],
                _ => vec![ConfigField::DeviceName],
            };
            let row_y0 = y0 + 78;
            let row_h = 32;
            let mut rows = Vec::new();
            for (i, &f) in fields.iter().enumerate() {
                let ry = row_y0 + (i as i32) * row_h;
                let minus = (x0 + 200, ry, 24, 24);
                let value = (x0 + 230, ry, 150, 24);
                let plus = (x0 + 384, ry, 24, 24);
                rows.push((f, minus, value, plus));
            }
            let by = y0 + CFG_H - 40;
            let buttons = [
                (x0 + 20, by, 120, 28),
                (x0 + 180, by, 120, 28),
                (x0 + 340, by, 120, 28),
            ];
            // 系统页专属: 查询/下载两个按钮 + 文件列表区域
            let snap_buttons = [
                (x0 + 12, y0 + 112, 200, 28),
                (x0 + 268, y0 + 112, 200, 28),
            ];
            let list_y = y0 + 150;
            let list_h = CFG_H - 150 - 48;
            let snap_list = (x0 + 12, list_y, CFG_W - 24, list_h);
            CfgGeo { win, tabs, rows, buttons, snap_buttons, snap_list }
        }

        /// 配置参数显示标签
        fn field_label(f: ConfigField) -> &'static str {
            match f {
                ConfigField::Stream => "码流",
                ConfigField::Codec => "编码格式",
                ConfigField::Resolution => "分辨率",
                ConfigField::RcMode => "码率控制",
                ConfigField::Fps => "帧率",
                ConfigField::Bitrate => "码率",
                ConfigField::Gop => "GOP",
                ConfigField::Brightness => "亮度",
                ConfigField::Contrast => "对比度",
                ConfigField::Saturation => "饱和度",
                ConfigField::Sharpness => "锐度",
                ConfigField::DeviceName => "设备名称",
            }
        }

        /// 配置参数当前值 (字符串)
        fn field_value(&self, f: ConfigField) -> String {
            let cfg = self.config.as_ref().unwrap();
            match f {
                ConfigField::Stream => self.config_stream.clone(),
                ConfigField::Codec => cfg.codec.label().to_string(),
                ConfigField::Resolution => cfg.resolution.label().to_string(),
                ConfigField::RcMode => cfg.rc_mode.clone(),
                ConfigField::Fps => format!("{} fps", cfg.fps),
                ConfigField::Bitrate => format!("{} kbps", cfg.bitrate_kbps),
                ConfigField::Gop => format!("{} 帧", cfg.gop),
                ConfigField::Brightness => format!("{}", cfg.brightness),
                ConfigField::Contrast => format!("{}", cfg.contrast),
                ConfigField::Saturation => format!("{}", cfg.saturation),
                ConfigField::Sharpness => format!("{}", cfg.sharpness),
                ConfigField::DeviceName => cfg.device_name.clone(),
            }
        }

        /// 调整数值/枚举参数 (+1 / -1 步进)
        fn adjust(&mut self, field: ConfigField, dir: i32) {
            let d = if dir > 0 { 1 } else { -1 };
            match field {
                ConfigField::Stream => {
                    self.config_stream = match self.config_stream.as_str() {
                        "main" => "sub".to_string(),
                        "sub" => "third".to_string(),
                        _ => "main".to_string(),
                    };
                    // 切换码流后重新从摄像头读取该码流参数
                    if let Some(peer) = self.config_peer.clone() {
                        self.config_fetch_pending = Some(peer);
                    }
                }
                _ => {
                    let cfg = match self.config.as_mut() {
                        Some(c) => c,
                        None => return,
                    };
                    match field {
                        ConfigField::Codec => cfg.codec = cfg.codec.cycle(),
                        // 分辨率放开编辑: cycle 预设 (1920x1080 / 1280x720 / 960x540 / 640x360)
                        ConfigField::Resolution => cfg.resolution = cfg.resolution.cycle(),
                        // 码率模式: CBR <-> VBR
                        ConfigField::RcMode => cfg.rc_mode = if cfg.rc_mode == "VBR" { "CBR".to_string() } else { "VBR".to_string() },
                        ConfigField::Fps => cfg.fps = (cfg.fps as i32 + d * 5).clamp(5, 60) as u32,
                        ConfigField::Bitrate => cfg.bitrate_kbps = (cfg.bitrate_kbps as i32 + d * 500).clamp(100, 8000) as u32,
                        ConfigField::Gop => cfg.gop = (cfg.gop as i32 + d * 10).clamp(1, 300) as u32,
                        ConfigField::Brightness => cfg.brightness = (cfg.brightness + d * 10).clamp(0, 100),
                        ConfigField::Contrast => cfg.contrast = (cfg.contrast + d * 10).clamp(0, 100),
                        ConfigField::Saturation => cfg.saturation = (cfg.saturation + d * 10).clamp(0, 100),
                        ConfigField::Sharpness => cfg.sharpness = (cfg.sharpness + d * 10).clamp(0, 100),
                        ConfigField::DeviceName => {}
                        ConfigField::Stream => unreachable!(),
                    }
                }
            }
            self.panel_dirty = true;
        }

        /// 配置窗体左键点击处理 (坐标为窗口绝对坐标)
        fn on_config_click(&mut self, x: i32, y: i32, _actions: &mut Vec<UiAction>) {
            let geo = self.config_geometry();
            let (wx, wy, _, _) = geo.win;
            let lx = x - wx;
            let ly = y - wy;
            for (i, t) in geo.tabs.iter().enumerate() {
                if in_rect(lx, ly, *t) {
                    self.config_tab = i;
                    self.config_editing_name = false;
                    self.panel_dirty = true;
                    let _ = self.draw_now();
                    return;
                }
            }
            for (f, minus, value, plus) in &geo.rows {
                if in_rect(lx, ly, *minus) {
                    self.adjust(*f, -1);
                    let _ = self.draw_now();
                    return;
                }
                if in_rect(lx, ly, *plus) {
                    self.adjust(*f, 1);
                    let _ = self.draw_now();
                    return;
                }
                if *f == ConfigField::DeviceName && in_rect(lx, ly, *value) {
                    self.config_editing_name = true;
                    self.config_text = self.config.as_ref().map(|c| c.device_name.clone()).unwrap_or_default();
                    self.video_subsystem.text_input().start();
                    self.panel_dirty = true;
                    let _ = self.draw_now();
                    return;
                }
            }
            // 系统页专属: 查询 / 下载 按钮 + 文件列表单选
            if self.config_tab == 2 {
                for (i, b) in geo.snap_buttons.iter().enumerate() {
                    if in_rect(lx, ly, *b) {
                        match i {
                            0 => _actions.push(UiAction::QuerySnapshots),
                            _ => _actions.push(UiAction::DownloadSnapshot),
                        }
                        self.panel_dirty = true;
                        let _ = self.draw_now();
                        return;
                    }
                }
                // 文件列表项命中: 计算第几项并选中
                if in_rect(lx, ly, geo.snap_list) {
                    let (_, ly_, _, lh_) = geo.snap_list;
                    let item_h = 24i32;
                    let rel = ly - ly_ - 4;
                    if rel >= 0 {
                        let idx = (rel / item_h) as usize;
                        if idx < self.snapshot_files.len() {
                            self.selected_snapshot = Some(idx);
                            let name = self.snapshot_files[idx].clone();
                            self.set_status(format!("已选择: {name}"));
                        }
                    }
                    let _ = lh_;
                    self.panel_dirty = true;
                    let _ = self.draw_now();
                    return;
                }
            }
            for (i, b) in geo.buttons.iter().enumerate() {
                if in_rect(lx, ly, *b) {
                    match i {
                        0 => {
                            if let (Some(peer), Some(cfg)) = (self.config_peer.clone(), self.config.clone()) {
                                cfg.save(&peer);
                                println!(
                                    "[Viewer] Device config saved: {} -> device_configs/{}.json",
                                    super::short_id(&peer), peer
                                );
                            }
                            // 编码 tab: 实际下发到摄像头 (热改 + 持久化)
                            if self.config_tab == 0 {
                                if let (Some(peer), Some(ec)) = (self.config_peer.clone(), self.build_encoder_config()) {
                                    self.config_apply_pending = Some((peer, self.config_stream.clone(), ec));
                                }
                            } else if self.config_tab == 1 {
                                // 图像 tab: 实际下发到摄像头 (ISP AIQ 热改 + 持久化)
                                if let (Some(peer), Some(ic)) = (self.config_peer.clone(), self.build_image_config()) {
                                    self.config_image_apply_pending = Some((peer, ic));
                                }
                            }
                            self.config = None;
                            self.config_peer = None;
                            self.config_editing_name = false;
                        }
                        1 => {
                            self.config = None;
                            self.config_peer = None;
                            self.config_editing_name = false;
                        }
                        _ => {
                            self.config = Some(DeviceConfig::default());
                            self.config_editing_name = false;
                        }
                    }
                    self.panel_dirty = true;
                    let _ = self.draw_now();
                    return;
                }
            }
        }

        /// 确保 overlay 纹理尺寸匹配 (不匹配则重建)
        fn ensure_overlay(&mut self, w: u32, h: u32) -> Result<()> {
            let need = match &self.overlay_texture {
                Some(t) => t.query().width != w || t.query().height != h,
                None => true,
            };
            if need {
                let tc = self.canvas.texture_creator();
                let mut tex = map_sdl(
                    tc.create_texture_streaming(PixelFormatEnum::ABGR8888, w, h),
                    "create overlay texture",
                )?;
                // 允许圆角透明边与下方内容混合
                let _ = tex.set_blend_mode(sdl2::render::BlendMode::Blend);
                let tex: sdl2::render::Texture<'static> =
                    unsafe { std::mem::transmute::<sdl2::render::Texture<'_>, sdl2::render::Texture<'static>>(tex) };
                self.overlay_texture = Some(tex);
            }
            Ok(())
        }

        /// 判断字符是否为 CJK (含中文/日文假名/全角) 范围, 这些字符主等宽字体不含
        fn is_cjk(c: char) -> bool {
            let u = c as u32;
            (0x3000..=0x303F).contains(&u) // CJK 标点
                || (0x3400..=0x4DBF).contains(&u) // CJK 扩展 A
                || (0x4E00..=0x9FFF).contains(&u) // CJK 基本汉字
                || (0xF900..=0xFAFF).contains(&u) // CJK 兼容
                || (0xFF00..=0xFFEF).contains(&u) // 全角字母/数字
                || (0x3040..=0x30FF).contains(&u) // 平/片假名
        }

        /// 光栅化单个字形: CJK 字符直接走中文 fallback 字体 (fontdue 对缺失字符返回 .notdef
        /// 而非空位图, 故不能用 width/height==0 判断), ASCII 仍用主等宽字体保证观感
        fn rasterize_glyph(&self, ch: char, px: f32) -> (fontdue::Metrics, Vec<u8>) {
            if Self::is_cjk(ch) {
                if let Some(cjk) = &self.font_cjk {
                    return cjk.rasterize(ch, px);
                }
            }
            self.font.rasterize(ch, px)
        }

        /// 带缓冲区宽度的文本绘制 (draw_text 硬编码 PANEL_W, 配置窗体更宽需此版本)
        fn draw_text_w(&mut self, buf: &mut [u8], buf_w: u32, buf_h: u32, x: i32, baseline: i32, px: f32, text: &str, color: [u8; 3]) {
            let mut pen = x as f32;
            for ch in text.chars() {
                let key = (ch, px as u32);
                if !self.glyph_cache.contains_key(&key) {
                    let g = self.rasterize_glyph(ch, px);
                    self.glyph_cache.insert(key, g);
                }
                let (m, bitmap) = self.glyph_cache.get(&key).unwrap();
                let gx = pen.round() as i32 + m.xmin;
                let gy = baseline - m.height as i32 - m.ymin;
                for row in 0..m.height {
                    let py = gy + row as i32;
                    if py < 0 || py >= buf_h as i32 {
                        continue;
                    }
                    for col in 0..m.width {
                        let pxx = gx + col as i32;
                        if pxx < 0 || pxx >= buf_w as i32 {
                            continue;
                        }
                        let cov = bitmap[row * m.width + col] as u32;
                        if cov == 0 {
                            continue;
                        }
                        let idx = ((py as u32 * buf_w + pxx as u32) * 4) as usize;
                        for c in 0..3 {
                            let dst = buf[idx + c] as u32;
                            buf[idx + c] = ((color[c] as u32 * cov + dst * (255 - cov)) / 255) as u8;
                        }
                        buf[idx + 3] = 255;
                    }
                }
                pen += m.advance_width;
            }
        }

        /// 绘制右键下拉菜单 (覆盖在 panel 之上)
        fn draw_context_menu(&mut self) -> Result<()> {
            let (mx, my, _) = match self.context_menu {
                Some(v) => v,
                None => return Ok(()),
            };
            let w = 150i32;
            let items = [("连接", CtxMenuAct::Connect), ("断开", CtxMenuAct::Disconnect), ("配置", CtxMenuAct::Configure)];
            let h = (items.len() as i32) * 26;
            self.ensure_overlay(w as u32, h as u32)?;
            let mut buf = vec![0u8; (w * h * 4) as usize];
            // 圆角背景 + 1px 描边
            fill_rect_r(&mut buf, w as u32, h as u32, 0, 0, w as u32, h as u32, 6, COL_BORDER);
            fill_rect_r(&mut buf, w as u32, h as u32, 1, 1, (w - 2) as u32, (h - 2) as u32, 5, COL_BG);
            for (i, (label, _)) in items.iter().enumerate() {
                let ry = i as i32 * 26;
                self.draw_text_w(&mut buf, w as u32, h as u32, 12, ry + 19, 14.0, label, COL_TEXT);
                if i < items.len() - 1 {
                    fill_rect(&mut buf, w as u32, h as u32, 0, ry + 26, w as u32, 1, COL_BORDER);
                }
            }
            if let Some(t) = &mut self.overlay_texture {
                map_sdl(t.update(None, &buf, (w * 4) as usize), "menu update")?;
                map_sdl(self.canvas.copy(t, None, Some(Rect::new(mx, my, w as u32, h as u32))), "menu copy")?;
            }
            Ok(())
        }

        /// 绘制配置窗体 (覆盖层)
        fn draw_config(&mut self) -> Result<()> {
            let geo = self.config_geometry();
            let (wx, wy, ww, wh) = geo.win;
            let w = ww as u32;
            let h = wh as u32;
            self.ensure_overlay(w, h)?;
            let mut buf = vec![0u8; (w * h * 4) as usize];

            // 窗体背景 + 1px 圆角描边 (现代深色卡片)
            fill_rect_r(&mut buf, w, h, 0, 0, w, h, 10, COL_BORDER);
            fill_rect_r(&mut buf, w, h, 1, 1, w - 2, h - 2, 9, COL_BG);
            // 顶部仅保留设备 PeerId 小字 (取消"设备配置"标题栏)
            if let Some(peer) = &self.config_peer {
                self.draw_text_w(&mut buf, w, h, 12, 21, 12.0, &super::short_id(peer), COL_TEXT_DIM);
            }

            // tabs (字号/配色统一为 viewer 风格)
            let tab_labels = ["编码", "图像", "系统"];
            for (i, t) in geo.tabs.iter().enumerate() {
                let active = i == self.config_tab;
                fill_rect_r(
                    &mut buf,
                    w,
                    h,
                    t.0,
                    t.1,
                    t.2 as u32,
                    t.3 as u32,
                    6,
                    if active { COL_ROW_SEL } else { [30, 32, 40, 255] },
                );
                self.draw_text_w(&mut buf, w, h, t.0 + 50, t.1 + 18, 13.0, tab_labels[i], COL_TEXT);
            }

            // 参数行 (字号统一缩小, 配色沿用主面板)
            for (f, minus, value, plus) in &geo.rows {
                let label = Self::field_label(*f);
                self.draw_text_w(&mut buf, w, h, value.0 - 150, value.1 + 18, 12.0, label, COL_TEXT);
                // 减号
                fill_rect_r(&mut buf, w, h, minus.0, minus.1, minus.2 as u32, minus.3 as u32, 4, COL_BTN);
                self.draw_text_w(&mut buf, w, h, minus.0 + 8, minus.1 + 18, 14.0, "-", COL_TEXT);
                // 加号
                fill_rect_r(&mut buf, w, h, plus.0, plus.1, plus.2 as u32, plus.3 as u32, 4, COL_BTN);
                self.draw_text_w(&mut buf, w, h, plus.0 + 8, plus.1 + 18, 14.0, "+", COL_TEXT);
                // 值框 (圆角 + 1px 描边)
                fill_rect_r(&mut buf, w, h, value.0, value.1, value.2 as u32, value.3 as u32, 3, COL_BORDER);
                fill_rect_r(&mut buf, w, h, value.0 + 1, value.1 + 1, value.2 as u32 - 2, value.3 as u32 - 2, 2, COL_INPUT_BG);
                let shown = if *f == ConfigField::DeviceName && self.config_editing_name {
                    format!("{}_", self.config_text)
                } else {
                    self.field_value(*f)
                };
                let vcol = if *f == ConfigField::DeviceName && self.config_editing_name {
                    COL_ACCENT
                } else {
                    COL_TEXT
                };
                self.draw_text_w(&mut buf, w, h, value.0 + 8, value.1 + 18, 12.0, &shown, vcol);
            }

            // 底部按钮 (圆角)
            let btn_labels = ["保存", "取消", "默认"];
            for (i, b) in geo.buttons.iter().enumerate() {
                fill_rect_r(&mut buf, w, h, b.0, b.1, b.2 as u32, b.3 as u32, 6, COL_BTN);
                self.draw_text_w(&mut buf, w, h, b.0 + 42, b.1 + 19, 13.0, btn_labels[i], COL_TEXT);
            }

            // 系统页专属: 查询 / 下载 两个按钮 + AVI 文件列表 (单选)
            if self.config_tab == 2 {
                let snap_labels = ["查询", "下载"];
                for (i, b) in geo.snap_buttons.iter().enumerate() {
                    fill_rect_r(&mut buf, w, h, b.0, b.1, b.2 as u32, b.3 as u32, 6, COL_BTN);
                    self.draw_text_w(&mut buf, w, h, b.0 + 84, b.1 + 19, 13.0, snap_labels[i], COL_TEXT);
                }
                // 文件列表标题
                self.draw_text_w(&mut buf, w, h, geo.snap_list.0, geo.snap_list.1 - 18, 12.0, "已合成 AVI 文件 (点击选择):", COL_TEXT_DIM);
                // 列表区域边框 + 滚动(仅显示前若干项)
                let (lx, ly, lw, lh) = geo.snap_list;
                fill_rect_r(&mut buf, w, h, lx, ly, lw as u32, lh as u32, 4, COL_BORDER);
                fill_rect_r(&mut buf, w, h, lx + 1, ly + 1, lw as u32 - 2, lh as u32 - 2, 3, COL_INPUT_BG);
                let item_h = 24i32;
                let max_items = (lh / item_h) as usize;
                let shown: Vec<(usize, String)> = self.snapshot_files.iter()
                    .take(max_items)
                    .enumerate()
                    .map(|(i, n)| (i, n.clone()))
                    .collect();
                for (i, name) in shown {
                    let ry = ly + 4 + (i as i32) * item_h;
                    if Some(i) == self.selected_snapshot {
                        fill_rect_r(&mut buf, w, h, lx + 3, ry, lw as u32 - 6, (item_h - 4) as u32, 3, COL_ROW_SEL);
                    }
                    self.draw_text_w(&mut buf, w, h, lx + 8, ry + 17, 12.0, &name, COL_TEXT);
                }
                if self.snapshot_files.is_empty() {
                    self.draw_text_w(&mut buf, w, h, lx + 8, ly + 4 + 17, 12.0, "(空, 请点击「查询」)", COL_TEXT_DIM);
                }
            }

            if let Some(t) = &mut self.overlay_texture {
                map_sdl(t.update(None, &buf, (w * 4) as usize), "config update")?;
                map_sdl(self.canvas.copy(t, None, Some(Rect::new(wx, wy, w, h))), "config copy")?;
            }
            Ok(())
        }

        /// 绘制瞬时提示浮层 (居中). 过期后不再绘制, 下次 draw() 自然消失.
        fn draw_toast(&mut self) -> Result<()> {
            let (msg, expire) = match &self.toast {
                Some(v) => (v.0.clone(), v.1),
                None => return Ok(()),
            };
            if Instant::now() >= expire {
                self.toast = None;
                self.panel_dirty = true;
                return Ok(());
            }
            // 2 行: 主提示 + 副提示 (用 \n 分隔)
            let lines: Vec<&str> = msg.split('\n').collect();
            let line_h = 22i32;
            let pad_x = 22i32;
            let pad_y = 14i32;
            // 计算宽度 (取最长行)
            let mut max_w = 0i32;
            for l in &lines {
                let cw: i32 = l.chars().map(|c| if Self::is_cjk(c) { 14 } else { 8 }).sum();
                max_w = max_w.max(cw);
            }
            let tw = (max_w + pad_x * 2).max(80);
            let th = (lines.len() as i32) * line_h + pad_y * 2;

            self.ensure_overlay(tw as u32, th as u32)?;
            let mut buf = vec![0u8; (tw * th * 4) as usize];
            fill_rect_r(&mut buf, tw as u32, th as u32, 0, 0, tw as u32, th as u32, 8, COL_BORDER);
            fill_rect_r(&mut buf, tw as u32, th as u32, 1, 1, tw as u32 - 2, th as u32 - 2, 7, [26, 28, 36, 240]);
            for (i, l) in lines.iter().enumerate() {
                let ry = pad_y + i as i32 * line_h;
                self.draw_text_w(&mut buf, tw as u32, th as u32, pad_x, ry + 16, 14.0, l, COL_TEXT);
            }
            if let Some(t) = &mut self.overlay_texture {
                map_sdl(t.update(None, &buf, (tw * 4) as usize), "toast update")?;
                let (win_w, win_h) = map_sdl(self.canvas.output_size(), "output_size")?;
                let tx = (win_w as i32 - tw) / 2;
                let ty = (win_h as i32 - th) / 2;
                map_sdl(self.canvas.copy(t, None, Some(Rect::new(tx, ty, tw as u32, th as u32))), "toast copy")?;
            }
            Ok(())
        }

        // ---- 渲染 ----

        /// 解码一个 H.265 access unit 并渲染。
        /// SDL 事件由 pump_events 统一处理, 此处只做解码 + 绘制。
        pub fn render(&mut self, au: &[u8], is_keyframe: bool) -> Result<()> {
            // 窗口最小化时跳过解码渲染 (最小化时 session 已 shutdown, 防御性处理)
            if self.minimized {
                return Ok(());
            }

            // 解码器 flush 后等待关键帧：跳过非关键帧避免花屏
            if self.waiting_for_keyframe {
                let waited = self.waiting_since.map(|t| t.elapsed());
                // 收到关键帧，或等待超时（cam 关键帧标记异常时强制开门，避免永久黑屏）
                if !is_keyframe && !waited.map_or(false, |w| w >= super::IDR_WAIT_TIMEOUT) {
                    return Ok(());
                }
                self.waiting_for_keyframe = false;
                if is_keyframe {
                    eprintln!("[Player] Keyframe received, resuming decode");
                } else {
                    eprintln!(
                        "[Player] No IDR within {:?}, resuming decode anyway (cam keyframe flag may be unreliable)",
                        super::IDR_WAIT_TIMEOUT
                    );
                }
            }

            let mut packet = ffmpeg::Packet::new(au.len());
            if let Some(data) = packet.data_mut() {
                data.copy_from_slice(au);
            }
            self.decoder.send_packet(&packet)?;

            let mut frame = ffmpeg::frame::Video::empty();
            let mut got_frame = false;
            loop {
                match self.decoder.receive_frame(&mut frame) {
                    Ok(()) => {
                        self.upload_frame(&frame)?;
                        got_frame = true;
                    }
                    Err(_) => break,
                }
            }

            if got_frame {
                self.draw()?;
            }
            Ok(())
        }

        /// 将解码帧上传到视频纹理 (不负责 present)
        fn upload_frame(&mut self, frame: &ffmpeg::frame::Video) -> Result<()> {
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

                // 仅在首次确定视频分辨率时设置一次窗口初始尺寸: 面板 + 视频原始尺寸,
                // 等比缩小到不超过屏幕可用区域; 之后尊重用户的拖动/最大化
                if !self.window_sized {
                    // 若窗口处于最大化状态，不强行改变窗口尺寸（否则会取消最大化），
                    // 视频区域按当前窗口尺寸做 letterbox 即可。
                    // SDL_WINDOW_MAXIMIZED = 0x00000080 (window_flags() 返回原始 u32 标志位)
                    const SDL_WINDOW_MAXIMIZED: u32 = 0x00000080;
                    let flags = self.canvas.window().window_flags();
                    let (cur_w, cur_h) = self.canvas.window().size();
                    // 已最大化（标志位置位），或窗口尺寸已接近全屏可用区，都视为最大化，
                    // 跳过 set_size，避免把启动即最大化的窗口改回视频原始尺寸。
                    let maximized = (flags & SDL_WINDOW_MAXIMIZED != 0)
                        || (cur_w >= self.display_max.0 * 9 / 10
                            && cur_h >= self.display_max.1 * 9 / 10);
                    if !maximized {
                        let (max_w, max_h) = self.display_max;
                        let avail_w = max_w.saturating_sub(PANEL_W).max(320);
                        let scale = (avail_w as f32 / w as f32)
                            .min(max_h as f32 / h as f32)
                            .min(1.0);
                        let vid_w = ((w as f32) * scale).round().max(1.0) as u32;
                        let vid_h = ((h as f32) * scale).round().max(1.0) as u32;
                        let win = self.canvas.window_mut();
                        let _ = win.set_size(vid_w + PANEL_W, vid_h);
                        win.set_position(
                            sdl2::video::WindowPos::Centered,
                            sdl2::video::WindowPos::Centered,
                        );
                    }
                    self.window_sized = true;
                    self.panel_dirty = true;
                }
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

            self.frame_count += 1;
            if self.frame_count % 100 == 0 {
                println!("[Player] Rendered {} frames", self.frame_count);
            }

            Ok(())
        }

        /// 整窗绘制: 背景 + 视频 (右侧 letterbox) + 设备面板 (左侧), 然后 present
        pub fn draw(&mut self) -> Result<()> {
            if self.panel_dirty {
                self.rebuild_panel()?;
            }

            self.canvas.set_draw_color(Color::RGB(14, 15, 19));
            self.canvas.clear();

            let (win_w, win_h) = map_sdl(self.canvas.output_size(), "output_size")?;

            // 视频: 右侧区域内等比缩放居中 (letterbox)
            if self.width > 0 && win_w > PANEL_W {
                if let Some(tex) = &self.texture {
                    let avail_w = win_w - PANEL_W;
                    let scale = (avail_w as f32 / self.width as f32)
                        .min(win_h as f32 / self.height as f32);
                    let dst_w = ((self.width as f32) * scale).round().max(1.0) as u32;
                    let dst_h = ((self.height as f32) * scale).round().max(1.0) as u32;
                    let dst_x = PANEL_W as i32 + (avail_w.saturating_sub(dst_w) / 2) as i32;
                    let dst_y = (win_h.saturating_sub(dst_h) / 2) as i32;
                    map_sdl(
                        self.canvas.copy(tex, None, Some(Rect::new(dst_x, dst_y, dst_w, dst_h))),
                        "copy video",
                    )?;
                    // 视频描边 (1px, 现代深色分隔)
                    self.canvas.set_draw_color(Color::RGB(46, 49, 60));
                    let _ = self.canvas.draw_rect(Rect::new(dst_x, dst_y, dst_w, dst_h));
                }
            }

            // 左侧设备面板
            if let Some(pt) = &self.panel_texture {
                map_sdl(
                    self.canvas.copy(pt, None, Some(Rect::new(0, 0, PANEL_W, self.panel_h))),
                    "copy panel",
                )?;
            }

            // 覆盖层: 右键菜单 / 配置窗体 (须在 present 之前 copy)
            if self.context_menu.is_some() {
                let _ = self.draw_context_menu();
            }
            if self.config.is_some() {
                let _ = self.draw_config();
            }

            // 瞬时提示 (下载完成等) 居中浮层
            let _ = self.draw_toast();

            self.canvas.present();
            self.last_present = Instant::now();

            // 首次 present 后窗口已被窗口管理器映射，此时调用 maximize() 最可靠
            // （部分 WM 会忽略 build 阶段尚未映射窗口的 maximize）。仅执行一次。
            if !self.maximize_applied {
                self.canvas.window_mut().maximize();
                self.maximize_applied = true;
            }
            Ok(())
        }

        /// 立即重绘 (忽略错误), 用于阻塞操作前刷新状态提示
        pub fn draw_now(&mut self) {
            let _ = self.draw();
        }

        /// 空闲重绘: 面板有变化立即绘制, 否则限频 ~10fps 刷新
        /// (有视频帧时 render() 已经在 present, 此处不会频繁触发)
        pub fn maybe_draw(&mut self) {
            if self.panel_dirty || self.last_present.elapsed().as_millis() > 100 {
                let _ = self.draw();
            }
        }

        // ---- 面板绘制 ----

        /// 重建面板纹理 (CPU RGBA 缓冲绘制后上传, 仅在状态变化时调用)
        fn rebuild_panel(&mut self) -> Result<()> {
            let (_, win_h) = self.canvas.window().size();
            let h = win_h.max(240);

            if self.panel_texture.is_none() || self.panel_h != h {
                let tc = self.canvas.texture_creator();
                let tex = map_sdl(
                    tc.create_texture_streaming(PixelFormatEnum::ABGR8888, PANEL_W, h),
                    "create panel texture",
                )?;
                let tex: sdl2::render::Texture<'static> =
                    unsafe { std::mem::transmute::<sdl2::render::Texture<'_>, sdl2::render::Texture<'static>>(tex) };
                self.panel_texture = Some(tex);
                self.panel_h = h;
            }

            let mut buf = vec![0u8; (PANEL_W * h * 4) as usize];
            fill_rect(&mut buf, PANEL_W, h, 0, 0, PANEL_W, h, COL_BG);
            fill_rect(&mut buf, PANEL_W, h, PANEL_W as i32 - 2, 0, 2, h, COL_BORDER);

            // 标题
            self.draw_text(&mut buf, h, 12, 26, 16.0, "Devices", COL_TITLE);
            fill_rect(&mut buf, PANEL_W, h, 8, 34, PANEL_W - 16, 1, COL_BORDER);

            // 设备列表
            let devices = self.devices.clone();
            let connected = self.connected_cam.clone();
            let re = rows_end(h);
            if devices.is_empty() {
                self.draw_text(&mut buf, h, 12, ROWS_Y0 + 17, 12.0, "No devices.", COL_TEXT_DIM);
                self.draw_text(&mut buf, h, 12, ROWS_Y0 + 35, 12.0, "Click [+ Add] below.", COL_TEXT_DIM);
            }
            for (i, dev) in devices.iter().enumerate() {
                let ry = ROWS_Y0 + i as i32 * ROW_H;
                if ry + ROW_H > re {
                    self.draw_text(&mut buf, h, 12, ry + 14, 12.0, "...", COL_TEXT_DIM);
                    break;
                }
                if self.selected == Some(i) {
                    fill_rect(&mut buf, PANEL_W, h, 4, ry, PANEL_W - 10, (ROW_H - 2) as u32, COL_ROW_SEL);
                }
                let is_conn = connected.as_deref() == Some(dev.as_str());
                let color = if is_conn { COL_GREEN } else { COL_TEXT };
                let marker = if is_conn { ">" } else { " " };
                let label = format!("{} {}", marker, super::short_id(dev));
                self.draw_text(&mut buf, h, 8, ry + 17, 13.0, &label, color);
            }

            // 状态行
            let status = self.status.clone();
            let status_col = if self.adding { COL_ACCENT } else { COL_TEXT_DIM };
            self.draw_text(&mut buf, h, 8, h as i32 - 84, 11.0, &status, status_col);

            // 添加设备输入框
            if self.adding {
                let bx = 8i32;
                let by = h as i32 - 76;
                let bw = PANEL_W - 16;
                let bh = 24u32;
                // 圆角输入框 + accent 描边
                fill_rect_r(&mut buf, PANEL_W, h, bx, by, bw as u32, bh, 4, COL_ACCENT_BG);
                fill_rect_r(&mut buf, PANEL_W, h, bx + 1, by + 1, bw as u32 - 2, bh - 2, 3, COL_INPUT_BG);
                // 输入内容 (过长时显示尾部) + 光标
                let shown = if self.input.len() > 26 {
                    format!("..{}_", &self.input[self.input.len() - 24..])
                } else {
                    format!("{}_", self.input)
                };
                self.draw_text(&mut buf, h, 12, h as i32 - 58, 13.0, &shown, COL_TEXT);
            }

            // 按钮 (圆角 + 顶部高光)
            let (ax, ay, aw, ah) = btn_add_rect(h);
            fill_rect_r(&mut buf, PANEL_W, h, ax, ay, aw as u32, ah as u32, 6, COL_BTN);
            fill_rect(&mut buf, PANEL_W, h, ax, ay, aw as u32, 1, COL_BTN_HI);
            self.draw_text(&mut buf, h, ax + 32, ay + 20, 13.0, "+ Add", COL_TEXT);
            let (dx, dy, dw, dh) = btn_del_rect(h);
            fill_rect_r(&mut buf, PANEL_W, h, dx, dy, dw as u32, dh as u32, 6, COL_BTN);
            fill_rect(&mut buf, PANEL_W, h, dx, dy, dw as u32, 1, COL_BTN_HI);
            self.draw_text(&mut buf, h, dx + 32, dy + 20, 13.0, "- Del", COL_TEXT);

            // 上传到纹理
            if let Some(t) = &mut self.panel_texture {
                map_sdl(t.update(None, &buf, (PANEL_W * 4) as usize), "panel update")?;
            }
            self.panel_dirty = false;
            Ok(())
        }

        /// 在 RGBA 缓冲上绘制一行文字 (fontdue 光栅化 + 灰度 alpha 混合)
        fn draw_text(&mut self, buf: &mut [u8], buf_h: u32, x: i32, baseline: i32, px: f32, text: &str, color: [u8; 3]) {
            let mut pen = x as f32;
            for ch in text.chars() {
                let key = (ch, px as u32);
                if !self.glyph_cache.contains_key(&key) {
                    let g = self.rasterize_glyph(ch, px);
                    self.glyph_cache.insert(key, g);
                }
                let (m, bitmap) = self.glyph_cache.get(&key).unwrap();
                let gx = pen.round() as i32 + m.xmin;
                let gy = baseline - m.height as i32 - m.ymin;
                for row in 0..m.height {
                    let py = gy + row as i32;
                    if py < 0 || py >= buf_h as i32 {
                        continue;
                    }
                    for col in 0..m.width {
                        let pxx = gx + col as i32;
                        if pxx < 0 || pxx >= PANEL_W as i32 {
                            continue;
                        }
                        let cov = bitmap[row * m.width + col] as u32;
                        if cov == 0 {
                            continue;
                        }
                        let idx = ((py as u32 * PANEL_W + pxx as u32) * 4) as usize;
                        for c in 0..3 {
                            let dst = buf[idx + c] as u32;
                            buf[idx + c] = ((color[c] as u32 * cov + dst * (255 - cov)) / 255) as u8;
                        }
                        buf[idx + 3] = 255;
                    }
                }
                pen += m.advance_width;
            }
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

    /// 摄像头 (DeviceCam) PeerId 或短序列号 serial (覆盖配置文件)
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

/// 结构化中继配置 (可读性优先的新格式, TOML 以 `[[relay_list]]` 表数组书写)
///
/// 例:
/// ```toml
/// [[relay_list]]
/// name = "主中继-QUIC"
/// ip = "101.35.90.171"
/// port = 4001
/// transport = "quic"      # "quic" (udp+quic-v1, 推荐) | "tcp"
/// peer_id = "12D3KooW..."
/// ```
/// 由 `relay_multiaddrs()` 转换为 multiaddr 字符串, 兼容旧格式 relays/relay。
#[derive(Debug, Clone, Serialize, Deserialize)]
struct RelayConfig {
    #[serde(default)]
    name: Option<String>,
    ip: String,
    #[serde(default = "default_relay_port")]
    port: u16,
    #[serde(default = "default_relay_transport")]
    transport: String,
    peer_id: String,
}

fn default_relay_port() -> u16 { 4001 }
fn default_relay_transport() -> String { "quic".to_string() }

impl RelayConfig {
    fn to_multiaddr(&self) -> Option<String> {
        let ip = self.ip.trim().trim_matches(|c| c == '[' || c == ']');
        if ip.is_empty() || self.peer_id.trim().is_empty() {
            return None;
        }
        let family = if ip.contains(':') { "ip6" } else { "ip4" };
        let transport = self.transport.to_lowercase();
        let ma = match transport.as_str() {
            "tcp" => format!("/{family}/{ip}/tcp/{}/p2p/{}", self.port, self.peer_id),
            "quic" => format!("/{family}/{ip}/udp/{}/quic-v1/p2p/{}", self.port, self.peer_id),
            other => {
                tracing::warn!("[Viewer] 未知 relay transport '{other}', 已跳过该中继");
                return None;
            }
        };
        Some(ma)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct ViewerConfig {
    /// 结构化中继配置 (推荐格式, 可读性强, TOML `[[relay_list]]`)
    #[serde(default)]
    relay_list: Vec<RelayConfig>,
    /// 多 Relay 地址列表 (旧格式字符串数组, 含 CLI 覆盖, 优先级最高)
    /// 空数组保存时省略 (避免回写冗余空字段)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    relays: Vec<String>,
    /// 单 Relay 地址 (旧格式, 向后兼容, 解析时合并到 relays)
    /// 空字符串保存时省略
    #[serde(default, skip_serializing_if = "String::is_empty")]
    relay: String,
    /// 是否启用 mDNS 局域网发现 (默认 true)
    #[serde(default = "default_enable_mdns")]
    enable_mdns: bool,
    /// 设备列表: 每个元素可以是长 PeerId 或短序列号 serial (如树莓派 /proc/cpuinfo 的
    /// Serial, 形如 e33700a6620dfddc)。viewer 会自动判定——能 parse 成 PeerId 就直连，
    /// 否则当 serial 经 relay 注册表解析出真实 PeerId 再连接。GUI 设备管理面板可增删。
    /// 兼容旧配置的 `cameras` 字段 (serde alias 合并进来)。
    #[serde(default, alias = "cameras")]
    camera_serials: Vec<String>,
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
    /// 本地静态 serial→peer_id 映射 (TOML table)。
    /// 命中时无需连接 Relay 即可解析 SN，适合局域网 / Relay 不可达场景。
    /// 例: serial_map = { "e33700a6620dfddc" = "12D3KooW..." }
    #[serde(default)]
    serial_map: std::collections::HashMap<String, String>,
}

fn default_stream() -> String { "auto".to_string() }
fn default_network_type() -> String { "auto".to_string() }

impl Default for ViewerConfig {
    fn default() -> Self {
        Self {
            relay_list: Vec::new(),
            relays: Vec::new(),
            relay: String::new(),
            enable_mdns: default_enable_mdns(),
            camera_serials: Vec::new(),
            stream: default_stream(),
            output: None,
            no_audio: false,
            play: false,
            udp_port: None,
            network_type: default_network_type(),
            serial_map: std::collections::HashMap::new(),
        }
    }
}

impl ViewerConfig {
    fn load(path: &PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {e}", path.display()))?;
            let config: ViewerConfig = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config file {}: {e}", path.display()))?;
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

    /// 保存配置回文件 (GUI 增删设备后调用)
    /// 注意: toml 序列化会丢失原文件中的注释
    #[cfg_attr(not(feature = "player"), allow(dead_code))]
    fn save(&self, path: &PathBuf) -> anyhow::Result<()> {
        let c = self.clone();
        let content = toml::to_string_pretty(&c)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {e}"))?;
        std::fs::write(path, content)
            .map_err(|e| anyhow::anyhow!("Failed to write config file {}: {e}", path.display()))?;
        Ok(())
    }

    /// 返回所有中继的 multiaddr 字符串 (合并旧格式 relays/relay 与结构化 relay_list)
    ///
    /// 优先级: relays (字符串数组, 含 CLI 覆盖) > relay (单字符串) > relay_list (结构化)
    fn relay_multiaddrs(&self) -> Vec<String> {
        if !self.relays.is_empty() {
            return self.relays.clone();
        }
        let mut out = Vec::new();
        if !self.relay.is_empty() {
            out.push(self.relay.clone());
        }
        for rc in &self.relay_list {
            if let Some(ma) = rc.to_multiaddr() {
                out.push(ma);
            }
        }
        out
    }

    /// 设备列表 (统一为 camera_serials，连接时自动判定 PeerId / serial)
    fn device_list(&self) -> Vec<String> {
        self.camera_serials.clone()
    }

    /// headless 模式使用的单设备: 设备列表第一个
    fn primary_camera(&self) -> Option<String> {
        self.device_list().into_iter().next()
    }
}
