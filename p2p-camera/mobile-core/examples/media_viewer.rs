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
use mobile_core::net_diag::NatType;
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

    let relay_addrs = config.relays.clone();
    let device_cam_str = config.camera.clone();
    let no_audio = config.no_audio;
    let udp_port = config.udp_port.unwrap_or(0);
    let enable_mdns = config.enable_mdns;
    let stream_type = config.stream.clone();
    let network_type = config.network_type.clone();

    let mut frame_count: u64 = 0;
    let mut bytes_received: u64 = 0;
    let mut _audio_count: u64 = 0;
    let mut _direct_upgraded = false;
    let mut _direct_via_lan = false;
    let mut _local_nat_type: Option<NatType> = None;
    let mut _remote_nat_hint: Option<String> = None;
    let mut video_start: Option<std::time::Instant> = None;
    let mut stream_disconnected = false;
    let mut window_minimized = false;
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
                MediaPlayerEvent::Disconnected { reason } => {
                    if stream_disconnected {
                        continue;
                    }
                    eprintln!("[Viewer] Session disconnected: {reason}");
                    stream_disconnected = true;

                    #[cfg(not(feature = "player"))]
                    {
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
                    #[cfg(feature = "player")]
                    {
                        // 先轮询 SDL 事件，检查窗口是否已最小化
                        if let Some(p) = player.as_mut() {
                            let action = p.poll_events();
                            if let RenderAction::WindowMinimized = action {
                                window_minimized = true;
                            } else if let RenderAction::Quit = action {
                                std::process::exit(0);
                            }
                        }
                        if !window_minimized {
                            eprintln!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
                            reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                            if let Some(p) = player.as_mut() {
                                p.reset_decoder();
                            }
                            match viewer.reconnect().await {
                                Ok(()) => {
                                    stream_disconnected = false;
                                }
                                Err(e) => {
                                    eprintln!("[Viewer] Reconnect failed: {e}");
                                }
                            }
                        }
                    }
                }
                MediaPlayerEvent::StreamEOF { reason } => {
                    if stream_disconnected {
                        continue;
                    }
                    eprintln!("[Viewer] Stream EOF: {reason}");
                    stream_disconnected = true;
                    #[cfg(not(feature = "player"))]
                    {
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
                    #[cfg(feature = "player")]
                    {
                        if !window_minimized {
                            eprintln!("[Viewer] Reconnecting in {}s...", RECONNECT_DELAY.as_secs());
                            reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                            if let Some(p) = player.as_mut() {
                                p.reset_decoder();
                            }
                            match viewer.reconnect().await {
                                Ok(()) => {
                                    stream_disconnected = false;
                                }
                                Err(e) => {
                                    eprintln!("[Viewer] Reconnect failed: {e}");
                                }
                            }
                        }
                    }
                }
                MediaPlayerEvent::DirectUpgraded { via_lan } => {
                    _direct_upgraded = true;
                    _direct_via_lan = via_lan;
                    let via = if via_lan { "LAN direct" } else { "DCUtR hole punch" };
                    println!("[Viewer] Direct connection established via {via}, streams upgraded");
                }
                MediaPlayerEvent::NatDiagnosis { local_nat, remote_nat } => {
                    _local_nat_type = Some(local_nat);
                    _remote_nat_hint = remote_nat.clone();
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
            match process_video_frame(
                packet, &mut frame_count, &mut bytes_received, &mut video_start, &mut output_file,
                #[cfg(feature = "player")]
                player.as_mut(),
            ) {
                RenderAction::Quit => {
                    if let Some(path) = &opt.output {
                        println!("[Viewer] Output saved to: {}", path.display());
                        println!("[Viewer] Play with: ffplay -f hevc {}", path.display());
                    }
                    return Ok(());
                }
                RenderAction::WindowMinimized => {
                    window_minimized = true;
                    // 最小化时关闭连接节约资源
                    viewer.shutdown();
                    stream_disconnected = true;
                }
                RenderAction::WindowRestored => {
                    window_minimized = false;
                }
                _ => {}
            }
        }

        // 轮询音频帧
        while let Some(packet) = viewer.poll_audio_frame() {
            _audio_count += 1;
            #[cfg(feature = "player")]
            if let Some(ap) = audio_player.as_mut() {
                ap.write(&packet.data);
            }
        }

        // 窗口恢复时重连
        #[cfg(feature = "player")]
        if window_minimized {
            if let Some(p) = player.as_mut() {
                let action = p.poll_events();
                match action {
                    RenderAction::WindowRestored => {
                        eprintln!("[Viewer] Window restored, reconnecting...");
                        window_minimized = false;
                        reset_stats(&mut frame_count, &mut bytes_received, &mut video_start);
                        p.reset_decoder();
                        match viewer.reconnect().await {
                            Ok(()) => {
                                stream_disconnected = false;
                            }
                            Err(e) => {
                                eprintln!("[Viewer] Reconnect failed: {e}");
                            }
                        }
                    }
                    RenderAction::Quit => {
                        if let Some(path) = &opt.output {
                            println!("[Viewer] Output saved to: {}", path.display());
                            println!("[Viewer] Play with: ffplay -f hevc {}", path.display());
                        }
                        return Ok(());
                    }
                    _ => {}
                }
            }
        }

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
            packet.is_keyframe(),
        ) {
            Ok(action) => return action,
            Err(e) => {
                tracing::error!("[Viewer] Player error: {e}");
            }
        }
    }

    RenderAction::Continue
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
        /// 显示屏可用区域最大尺寸，用于限制窗口初始尺寸不超过屏幕
        display_max: (u32, u32),
        /// 是否已根据视频分辨率设置过初始窗口尺寸（只在首帧设置一次，之后尊重用户拖动/最大化）
        window_sized: bool,
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

            // 显示屏可用区域(排除任务栏), 用于限制窗口初始及最大尺寸不超过屏幕
            let display_max = video_subsystem
                .display_usable_bounds(0)
                .map(|r| (r.width(), r.height()))
                .unwrap_or((1920, 1080));

            // 初始窗口尺寸不超过屏幕可用区域
            let init_w = 1280u32.min(display_max.0);
            let init_h = 720u32.min(display_max.1);

            let window = video_subsystem
                .window("P2P Camera Viewer", init_w, init_h)
                .position_centered()
                .resizable()
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
                display_max,
                window_sized: false,
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
                if !is_keyframe && !waited.map_or(false, |w| w >= super::IDR_WAIT_TIMEOUT) {
                    return Ok(RenderAction::Continue);
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

                // 仅在首次确定视频分辨率时设置一次窗口初始尺寸: 使用视频原始尺寸,
                // 但等比缩小到不超过屏幕可用区域; 之后尊重用户的拖动/最大化, 不再强制改窗口大小
                if !self.window_sized {
                    let (max_w, max_h) = self.display_max;
                    let scale = (max_w as f32 / w as f32)
                        .min(max_h as f32 / h as f32)
                        .min(1.0);
                    let win_w = ((w as f32) * scale).round().max(1.0) as u32;
                    let win_h = ((h as f32) * scale).round().max(1.0) as u32;
                    let win = self.canvas.window_mut();
                    let _ = win.set_size(win_w, win_h);
                    win.set_position(
                        sdl2::video::WindowPos::Centered,
                        sdl2::video::WindowPos::Centered,
                    );
                    self.window_sized = true;
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

            // 以黑色填充背景, 再把视频等比缩放居中绘制(letterbox), 随窗口大小/最大化自适应
            self.canvas.set_draw_color(sdl2::pixels::Color::RGB(0, 0, 0));
            self.canvas.clear();

            let (win_w, win_h) = self
                .canvas
                .output_size()
                .map_err(|e| anyhow::anyhow!("SDL output_size: {e}"))?;
            let scale = (win_w as f32 / self.width as f32)
                .min(win_h as f32 / self.height as f32);
            let dst_w = ((self.width as f32) * scale).round().max(1.0) as u32;
            let dst_h = ((self.height as f32) * scale).round().max(1.0) as u32;
            let dst_x = (win_w.saturating_sub(dst_w) / 2) as i32;
            let dst_y = (win_h.saturating_sub(dst_h) / 2) as i32;

            map_sdl(
                self.canvas.copy(
                    self.texture.as_ref().unwrap(),
                    None,
                    Some(Rect::new(dst_x, dst_y, dst_w, dst_h)),
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
