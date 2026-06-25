//! Viewer CLI — 端到端测试工具
//!
//! 用法:
//!   cargo run --example viewer_cli -- \
//!     --relay /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER> \
//!     --camera <GATEWAY_PEER_ID> \
//!     --output output.h265
//!
//! 实时播放 (需 --features player):
//!   cargo build --example viewer_cli --features player
//!   viewer_cli --relay ... --camera ... --play
//!
//! 自动重连: 连接断开时自动重新连接 Relay + Gateway + 打开 stream，
//!           播放器和输出文件持续运行不中断。
//!
//! 验证流程:
//!   1. 连接 Relay Server
//!   2. 通过 Circuit 拨号 Gateway
//!   3. 打开视频 stream 接收帧
//!   4. 保存到文件 (可用 ffplay 播放) 或 SDL 实时播放 (--play, 需 --features player)
//!   5. 打印接收统计

use std::time::Duration;

use anyhow::{Context, Result};
use bytes::BytesMut;
use clap::Parser;
use futures::{AsyncReadExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    dcutr, identify, noise, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, PeerId,
};
use libp2p_stream;
use proto::{media_packet::MediaPacket, stream_protocols};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

const STREAM_READ_BUF: usize = 65536;
const RECONNECT_DELAY: Duration = Duration::from_secs(3);

// SDL2 要求事件循环在主线程, 使用 current_thread runtime
#[cfg_attr(feature = "player", tokio::main(flavor = "current_thread"))]
#[cfg_attr(not(feature = "player"), tokio::main)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let opt = Opt::parse();

    // ---- 初始化播放器/输出 (独立于 P2P 连接，重连期间不中断) ----
    #[cfg(feature = "player")]
    let mut player = if opt.play {
        println!("[Viewer] Initializing SDL player...");
        Some(player::VideoPlayer::new()?)
    } else {
        None
    };

    #[cfg(feature = "player")]
    let mut audio_player = if opt.play && !opt.no_audio {
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

    let mut output_file = if let Some(path) = &opt.output {
        Some(std::fs::File::create(path).context("Failed to create output file")?)
    } else {
        None
    };

    // ---- 持久化 channel (重连时复用，不重建) ----
    let (tx, mut rx) = mpsc::channel::<MediaPacket>(60);
    let (audio_tx, mut audio_rx) = mpsc::channel::<MediaPacket>(60);

    // 用于与后台 session 通信
    let (session_tx, mut session_rx) = mpsc::channel::<SessionEvent>(1);

    let relay_addr_str = opt.relay.clone();
    let gateway_str = opt.camera.clone();
    let no_audio = opt.no_audio;

    let mut frame_count: u64 = 0;
    let mut bytes_received: u64 = 0;
    let mut audio_count: u64 = 0;
    let mut direct_upgraded = false;
    let start = std::time::Instant::now();

    // 启动初始 session (后台任务)
    spawn_session(
        relay_addr_str.clone(),
        gateway_str.clone(),
        no_audio,
        tx.clone(),
        audio_tx.clone(),
        session_tx.clone(),
    );

    println!("[Viewer] Receiving video frames... (Ctrl+C to stop)");

    // ---- 主循环: 消费帧 + 监控 session 状态 + 触发重连 ----
    loop {
        tokio::select! {
            // Session 事件 (断开/直连升级)
            session_event = session_rx.recv() => {
                match session_event {
                    Some(SessionEvent::Disconnected { reason }) => {
                        tracing::warn!("[Viewer] Session disconnected: {reason}. Reconnecting in {}s...",
                            RECONNECT_DELAY.as_secs());
                        tokio::time::sleep(RECONNECT_DELAY).await;

                        // 消费残留缓冲帧 (旧 session 已 abort，不再有新数据)
                        drain_channel(&mut rx, &mut frame_count, &mut bytes_received,
                            &start, &mut output_file,
                            #[cfg(feature = "player")]
                            player.as_mut());
                        drain_audio_channel(&mut audio_rx, &mut audio_count,
                            #[cfg(feature = "player")]
                            audio_player.as_mut());

                        // 重新启动 session
                        spawn_session(
                            relay_addr_str.clone(),
                            gateway_str.clone(),
                            no_audio,
                            tx.clone(),
                            audio_tx.clone(),
                            session_tx.clone(),
                        );
                    }
                    Some(SessionEvent::DirectUpgraded) => {
                        direct_upgraded = true;
                    }
                    None => break, // channel 关闭 → 退出
                }
            }

            // 接收视频帧
            packet = rx.recv() => {
                let Some(packet) = packet else { continue; };
                if !process_video_frame(
                    packet,
                    &mut frame_count,
                    &mut bytes_received,
                    &start,
                    &mut output_file,
                    #[cfg(feature = "player")]
                    player.as_mut(),
                ) {
                    break; // 用户关闭窗口
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
        }
    }

    // ---- Summary ----
    let elapsed = start.elapsed().as_secs_f64();
    println!("\n[Viewer] === Summary ===");
    println!("[Viewer] Direct connection (DCUtR): {}", if direct_upgraded { "YES (hole punched, no relay bandwidth)" } else { "NO (relay circuit)" });
    println!("[Viewer] Total frames: {frame_count}");
    println!("[Viewer] Total bytes: {bytes_received}");
    if elapsed > 0.0 {
        println!("[Viewer] Duration: {elapsed:.1}s");
        println!("[Viewer] Avg fps: {:.1}", frame_count as f64 / elapsed);
        println!("[Viewer] Avg bitrate: {:.0} kbps", (bytes_received * 8) as f64 / elapsed / 1000.0);
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
    /// DCUtR 直连建立
    DirectUpgraded,
}

/// 在后台启动一个 viewer session
fn spawn_session(
    relay_addr_str: String,
    gateway_str: String,
    no_audio: bool,
    video_tx: mpsc::Sender<MediaPacket>,
    audio_tx: mpsc::Sender<MediaPacket>,
    event_tx: mpsc::Sender<SessionEvent>,
) {
    tokio::spawn(async move {
        let result = run_viewer_session(
            &relay_addr_str,
            &gateway_str,
            no_audio,
            video_tx,
            audio_tx,
            event_tx.clone(),
        ).await;

        match result {
            Ok(()) => {} // 正常退出，event_tx drop 通知主循环
            Err(e) => {
                let _ = event_tx.send(SessionEvent::Disconnected {
                    reason: e.to_string(),
                }).await;
            }
        }
    });
}

/// 消费 channel 中所有残留视频帧
#[allow(unused_variables)]
fn drain_channel(
    rx: &mut mpsc::Receiver<MediaPacket>,
    frame_count: &mut u64,
    bytes_received: &mut u64,
    start: &std::time::Instant,
    output_file: &mut Option<std::fs::File>,
    #[cfg(feature = "player")] mut player: Option<&mut player::VideoPlayer>,
) {
    while let Ok(packet) = rx.try_recv() {
        if !process_video_frame(
            packet, frame_count, bytes_received, start, output_file,
            #[cfg(feature = "player")]
            player.as_mut().map(|p| &mut **p),
        ) {
            break;
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

/// 一次 Viewer 会话: 连接 Relay → Circuit 拨号 Gateway → 打开 stream → 驱动 swarm
///
/// 帧接收通过 spawn 的 receive_frames task → channel → 主循环消费。
/// 本函数只负责 swarm 事件循环，连接断开时返回 Err 通知主循环重连。
async fn run_viewer_session(
    relay_addr_str: &str,
    gateway_str: &str,
    no_audio: bool,
    video_tx: mpsc::Sender<MediaPacket>,
    audio_tx: mpsc::Sender<MediaPacket>,
    event_tx: mpsc::Sender<SessionEvent>,
) -> Result<()> {
    let relay_addr: Multiaddr = relay_addr_str.parse()
        .context("Invalid relay address")?;
    let gateway: PeerId = gateway_str.parse()
        .context("Invalid camera PeerId")?;

    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let local_peer_id = keypair.public().to_peer_id();
    println!("[Viewer] PeerId: {local_peer_id}");

    // ---- 构建 Swarm ----
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
            Ok(ViewerBehaviour::new(key.public(), relay_client))
        })?
        // Viewer 不需要 idle timeout: 视频流持续传输保持连接活跃
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(0)))
        .build();

    // ---- 监听本地 QUIC + TCP ----
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()
        .context("Invalid local QUIC listen addr")?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()
        .context("Invalid local TCP listen addr")?)?;
    println!("[Viewer] Listening on local QUIC + TCP (for DCUtR hole punch)");

    // ---- 1. 连接 Relay ----
    println!("[Viewer] Dialing relay: {relay_addr}");
    swarm.dial(relay_addr.clone())?;
    wait_for_event(&mut swarm, |e| matches!(
        e,
        SwarmEvent::ConnectionEstablished { .. }
    ), "relay connection").await?;

    // ---- 2. 通过 Circuit 拨号 Gateway ----
    let circuit_addr = relay_addr
        .with(Protocol::P2pCircuit)
        .with(Protocol::P2p(gateway));
    println!("[Viewer] Dialing gateway via circuit: {circuit_addr}");
    swarm.dial(circuit_addr)?;
    wait_for_event(&mut swarm, |e| matches!(
        e,
        SwarmEvent::ConnectionEstablished { peer_id, .. } if *peer_id == gateway
    ), "gateway circuit connection").await?;

    println!("[Viewer] Connected to gateway {gateway}");

    // ---- 3. 打开 video stream ----
    let mut stream_control = swarm.behaviour().stream.new_control();
    let video_stream = stream_control
        .open_stream(gateway, stream_protocols::VIDEO_PROTOCOL)
        .await
        .context("Failed to open video stream")?;
    println!("[Viewer] Video stream opened");

    // ---- 3b. 打开 audio stream (可选) ----
    let mut audio_abort_handle: Option<tokio::task::AbortHandle> = None;
    if !no_audio {
        match stream_control.open_stream(gateway, stream_protocols::AUDIO_PROTOCOL).await {
            Ok(audio_stream) => {
                println!("[Viewer] Audio stream opened");
                let h = tokio::spawn(receive_frames(gateway, audio_stream, audio_tx.clone()))
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
        Some(tokio::spawn(receive_frames(gateway, video_stream, video_tx.clone())).abort_handle());

    let mut direct_upgraded = false;

    // ---- 5. Swarm 事件循环 (帧消费在主循环中，不在此处) ----
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                dcutr::Event { result: Ok(_), remote_peer_id, .. },
            )) if !direct_upgraded => {
                println!("[Viewer] DCUtR direct connection established with {remote_peer_id}, upgrading streams...");
                match stream_control.open_stream(gateway, stream_protocols::VIDEO_PROTOCOL).await {
                    Ok(new_stream) => {
                        if let Some(h) = video_abort_handle.take() { h.abort(); }
                        let handle = tokio::spawn(receive_frames(gateway, new_stream, video_tx.clone())).abort_handle();
                        video_abort_handle = Some(handle);
                        direct_upgraded = true;
                        let _ = event_tx.send(SessionEvent::DirectUpgraded).await;
                        println!("[Viewer] Video stream upgraded to direct connection");
                    }
                    Err(e) => {
                        println!("[Viewer] Failed to open direct video stream (staying on circuit): {e}");
                    }
                }
                if direct_upgraded && !no_audio {
                    match stream_control.open_stream(gateway, stream_protocols::AUDIO_PROTOCOL).await {
                        Ok(new_stream) => {
                            if let Some(h) = audio_abort_handle.take() { h.abort(); }
                            let handle = tokio::spawn(receive_frames(gateway, new_stream, audio_tx.clone())).abort_handle();
                            audio_abort_handle = Some(handle);
                            println!("[Viewer] Audio stream upgraded to direct connection");
                        }
                        Err(e) => {
                            println!("[Viewer] Failed to open direct audio stream: {e}");
                        }
                    }
                }
            }
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                dcutr::Event { result: Ok(_), .. },
            )) => {}
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                dcutr::Event { result: Err(e), remote_peer_id, .. },
            )) => {
                println!("[Viewer] DCUtR hole punch FAILED with {remote_peer_id}: {e} (staying on relay circuit)");
            }
            SwarmEvent::Behaviour(ViewerBehaviourEvent::Identify(
                identify::Event::Received { info, .. },
            )) => {
                println!("[Viewer] Identify: observed_addr={}, listen_addrs={}",
                    info.observed_addr,
                    info.listen_addrs.len());
            }
            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                if peer_id == gateway {
                    println!("[Viewer] Gateway connection closed");
                    return Err(anyhow::anyhow!("Gateway connection closed"));
                }
                tracing::warn!("[Viewer] Connection closed: {peer_id}");
            }
            e => {
                tracing::debug!("[Viewer] Event: {:?}", e);
            }
        }
    }
}

/// 处理单个视频帧，返回 false 表示需要退出
#[allow(unused_variables)]
fn process_video_frame(
    packet: MediaPacket,
    frame_count: &mut u64,
    bytes_received: &mut u64,
    start: &std::time::Instant,
    output_file: &mut Option<std::fs::File>,
    #[cfg(feature = "player")] player: Option<&mut player::VideoPlayer>,
) -> bool {
    *frame_count += 1;
    *bytes_received += packet.data.len() as u64;

    if let Some(file) = output_file {
        use std::io::Write;
        if file.write_all(&packet.data).is_err() {
            return false;
        }
        let _ = file.flush();
    }

    #[cfg(feature = "player")]
    if let Some(p) = player {
        match p.render(&packet.data) {
            Ok(false) => {
                println!("[Viewer] Player window closed, stopping...");
                return false;
            }
            Ok(true) => {}
            Err(e) => {
                tracing::error!("[Viewer] Player error: {e}");
            }
        }
    }

    if *frame_count % 100 == 0 {
        let elapsed = start.elapsed().as_secs_f64();
        let fps = *frame_count as f64 / elapsed;
        let kbps = (*bytes_received * 8) as f64 / elapsed / 1000.0;
        let keyframe = if packet.is_keyframe() { "[I]" } else { "   " };
        println!(
            "[Viewer] {keyframe} frame #{} | {:.1} fps | {:.0} kbps | ts={}",
            frame_count, fps, kbps, packet.timestamp_ms
        );
    }

    true
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

        if let SwarmEvent::OutgoingConnectionError { error, .. } = &event {
            tracing::warn!("[Viewer] Connection error ({label}): {error}");
        }
    }
}

// ---- SDL Player (player feature) ----

#[cfg(feature = "player")]
mod player {
    use anyhow::{Context, Result};
    use ffmpeg_next as ffmpeg;
    use sdl2::event::Event;
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
    }

    impl VideoPlayer {
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
            })
        }

        /// 渲染一个 H.265 access unit, 返回 false 表示用户关闭窗口
        pub fn render(&mut self, au: &[u8]) -> Result<bool> {
            for event in self.event_pump.poll_iter() {
                match event {
                    Event::Quit { .. }
                    | Event::KeyDown {
                        keycode: Some(Keycode::Escape),
                        ..
                    } => return Ok(false),
                    _ => {}
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
                    Ok(()) => self.render_frame(&frame)?,
                    Err(_) => break,
                }
            }

            Ok(true)
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

            let (y, ys, u, us, v, vs) = if frame.format() == Pixel::YUV420P {
                (
                    frame.data(0).to_vec(), frame.stride(0) as usize,
                    frame.data(1).to_vec(), frame.stride(1) as usize,
                    frame.data(2).to_vec(), frame.stride(2) as usize,
                )
            } else {
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
                (
                    self.yuv_frame.data(0).to_vec(), self.yuv_frame.stride(0) as usize,
                    self.yuv_frame.data(1).to_vec(), self.yuv_frame.stride(1) as usize,
                    self.yuv_frame.data(2).to_vec(), self.yuv_frame.stride(2) as usize,
                )
            };

            if let Some(tex) = &mut self.texture {
                map_sdl(tex.update_yuv(None, &y, ys, &u, us, &v, vs), "update_yuv")?;
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

// ---- NetworkBehaviour ----

#[derive(NetworkBehaviour)]
struct ViewerBehaviour {
    relay_client: relay::client::Behaviour,
    dcutr: dcutr::Behaviour,
    identify: identify::Behaviour,
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

    /// 禁用音频流接收
    #[arg(long, default_value_t = false)]
    no_audio: bool,

    /// SDL 实时播放 (需 --features player 编译)
    #[cfg(feature = "player")]
    #[arg(long)]
    play: bool,
}
