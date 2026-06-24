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
    dcutr, identify, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux, PeerId,
};
use libp2p_stream;
use proto::{media_packet::MediaPacket, stream_protocols};
use tokio::sync::mpsc;
use tracing_subscriber::EnvFilter;

const STREAM_READ_BUF: usize = 65536;

// SDL2 要求事件循环在主线程, 使用 current_thread runtime
#[cfg_attr(feature = "player", tokio::main(flavor = "current_thread"))]
#[cfg_attr(not(feature = "player"), tokio::main)]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let opt = Opt::parse();

    let keypair = libp2p::identity::Keypair::generate_ed25519();
    let local_peer_id = keypair.public().to_peer_id();
    println!("[Viewer] PeerId: {local_peer_id}");

    // ---- 构建 Swarm ----
    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_relay_client(noise::Config::new, yamux::Config::default)?
        .with_behaviour(|key, relay_client| {
            Ok(ViewerBehaviour::new(key.public(), relay_client))
        })?
        .with_swarm_config(|c| c.with_idle_connection_timeout(Duration::from_secs(60)))
        .build();

    // ---- 监听本地 QUIC + TCP (DCUtR hole punch 前提: 双方都需要本地 socket) ----
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse()
        .context("Invalid local QUIC listen addr")?)?;
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse()
        .context("Invalid local TCP listen addr")?)?;
    println!("[Viewer] Listening on local QUIC + TCP (for DCUtR hole punch)");

    // ---- 1. 连接 Relay ----
    let relay_addr: Multiaddr = opt.relay.parse().context("Invalid relay address")?;
    println!("[Viewer] Dialing relay: {relay_addr}");
    swarm.dial(relay_addr.clone())?;

    // 等待连接
    wait_for_event(&mut swarm, |e| matches!(
        e,
        SwarmEvent::ConnectionEstablished { .. }
    ), "relay connection")
    .await?;

    // ---- 2. 通过 Circuit 拨号 Gateway ----
    let gateway: PeerId = opt.camera.parse().context("Invalid camera PeerId")?;
    let circuit_addr = relay_addr
        .with(Protocol::P2pCircuit)
        .with(Protocol::P2p(gateway));
    println!("[Viewer] Dialing gateway via circuit: {circuit_addr}");
    swarm.dial(circuit_addr)?;

    wait_for_event(&mut swarm, |e| matches!(
        e,
        SwarmEvent::ConnectionEstablished { peer_id, .. } if *peer_id == gateway
    ), "gateway circuit connection")
    .await?;

    println!("[Viewer] Connected to gateway {gateway}");

    // ---- 3. 打开视频 stream (初始走 circuit, DCUtR 成功后迁移到直连) ----
    let mut stream_control = swarm.behaviour().stream.new_control();
    let video_stream = stream_control
        .open_stream(gateway, stream_protocols::VIDEO_PROTOCOL)
        .await
        .context("Failed to open video stream")?;
    println!("[Viewer] Video stream opened (via relay circuit)");

    // ---- 3b. 打开音频 stream (可选) ----
    let (audio_tx, mut audio_rx) = mpsc::channel::<MediaPacket>(60);
    let mut audio_abort_handle: Option<tokio::task::AbortHandle> = None;
    if !opt.no_audio {
        match stream_control.open_stream(gateway, stream_protocols::AUDIO_PROTOCOL).await {
            Ok(audio_stream) => {
                println!("[Viewer] Audio stream opened (via relay circuit)");
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
    let (tx, mut rx) = mpsc::channel::<MediaPacket>(60);
    let mut video_abort_handle: Option<tokio::task::AbortHandle> =
        Some(tokio::spawn(receive_frames(gateway, video_stream, tx.clone())).abort_handle());

    // DCUtR 直连升级标记 (用于 Summary 统计)
    let mut direct_upgraded = false;

    // ---- 5. 初始化播放器/输出 ----
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

    let mut frame_count: u64 = 0;
    let mut bytes_received: u64 = 0;
    let mut audio_count: u64 = 0;
    let start = std::time::Instant::now();

    println!("[Viewer] Receiving video frames... (Ctrl+C to stop)");

    loop {
        tokio::select! {
            // 驱动 Swarm 事件
            event = swarm.select_next_some() => {
                match event {
                    SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                        dcutr::Event { result: Ok(_), remote_peer_id, .. },
                    )) if !direct_upgraded => {
                        println!("[Viewer] DCUtR direct connection established with {remote_peer_id}, upgrading streams...");
                        // 在直连上开新 video stream (libp2p 会优先选择最新建立的直连 connection)
                        match stream_control.open_stream(gateway, stream_protocols::VIDEO_PROTOCOL).await {
                            Ok(new_stream) => {
                                // abort 旧的 circuit 接收 task (会关闭旧 stream, gateway 侧收到 EOF 自然结束)
                                if let Some(h) = video_abort_handle.take() { h.abort(); }
                                let handle = tokio::spawn(receive_frames(gateway, new_stream, tx.clone())).abort_handle();
                                video_abort_handle = Some(handle);
                                direct_upgraded = true;
                                println!("[Viewer] Video stream upgraded to direct connection");
                            }
                            Err(e) => {
                                println!("[Viewer] Failed to open direct video stream (staying on circuit): {e}");
                            }
                        }
                        // 音频也迁移到直连
                        if direct_upgraded && !opt.no_audio {
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
                    )) => {
                        // 已迁移, 忽略重复 DCUtR 事件
                    }
                    SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(
                        dcutr::Event { result: Err(e), remote_peer_id, .. },
                    )) => {
                        println!("[Viewer] DCUtR hole punch FAILED with {remote_peer_id}: {e} (staying on relay circuit)");
                    }
                    SwarmEvent::Behaviour(ViewerBehaviourEvent::Identify(
                        identify::Event::Received { info, .. },
                    )) => {
                        // 打印 identify 观察到的地址 (DCUtR 依赖这些地址做 hole punch)
                        println!("[Viewer] Identify: observed_addr={}, listen_addrs={}",
                            info.observed_addr,
                            info.listen_addrs.len());
                    }
                    e => {
                        tracing::debug!("[Viewer] Event: {:?}", e);
                    }
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
                    // SDL 音频播放
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
            // 接收视频帧
            packet = rx.recv() => {
                let Some(packet) = packet else {
                    println!("[Viewer] Video stream ended");
                    break;
                };

                frame_count += 1;
                bytes_received += packet.data.len() as u64;

                // 写入文件 (H.265 裸流, 可用 ffplay 播放)
                if let Some(file) = &mut output_file {
                    use std::io::Write;
                    file.write_all(&packet.data)?;
                    file.flush()?;
                }

                // SDL 实时播放
                #[cfg(feature = "player")]
                if let Some(p) = player.as_mut() {
                    match p.render(&packet.data) {
                        Ok(false) => {
                            println!("[Viewer] Player window closed, stopping...");
                            break;
                        }
                        Ok(true) => {}
                        Err(e) => {
                            tracing::error!("[Viewer] Player error: {e}");
                        }
                    }
                }

                // 每 100 帧打印统计
                if frame_count % 100 == 0 {
                    let elapsed = start.elapsed().as_secs_f64();
                    let fps = frame_count as f64 / elapsed;
                    let kbps = (bytes_received * 8) as f64 / elapsed / 1000.0;
                    let keyframe = if packet.is_keyframe() { "[I]" } else { "   " };
                    println!(
                        "[Viewer] {keyframe} frame #{frame_count} | {fps:.1} fps | {kbps:.0} kbps | ts={}",
                        packet.timestamp_ms
                    );
                }
            }
        }
    }

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
                // 尝试解码所有完整的包
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

        // 处理错误事件
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
    use ffmpeg_next::codec::traits::Decoder;
    use sdl2::event::Event;
    use sdl2::keyboard::Keycode;
    use sdl2::pixels::PixelFormatEnum;
    use sdl2::rect::Rect;

    /// 将 sdl2 的各种错误类型 (String / IntegerOrSdlError / UpdateTextureYUVError ...)
    /// 统一转为 anyhow::Error
    fn map_sdl<T, E: std::string::ToString>(r: std::result::Result<T, E>, ctx: &str) -> Result<T> {
        r.map_err(|e| anyhow::anyhow!("SDL {ctx}: {}", e.to_string()))
    }

    /// H.265 解码 + SDL2 渲染的实时播放器
    ///
    /// SAFETY: `texture` 字段使用 `Texture<'static>`，实际生命周期绑定到 `canvas`。
    /// Rust 保证 struct 字段按声明顺序 drop，因此 texture (在前) 先于 canvas drop。
    pub struct VideoPlayer {
        // texture 必须在 canvas 之前声明 (先 drop)
        texture: Option<sdl2::render::Texture<'static>>,
        canvas: sdl2::render::Canvas<sdl2::video::Window>,
        decoder: ffmpeg::decoder::Video,
        event_pump: sdl2::EventPump,
        /// 格式转换器 (非 YUV420P → YUV420P)
        scaler: Option<ffmpeg::software::scaling::Context>,
        /// 转换后的 YUV 帧
        yuv_frame: ffmpeg::frame::Video,
        width: u32,
        height: u32,
        frame_count: u64,
    }

    impl VideoPlayer {
        pub fn new() -> Result<Self> {
            ffmpeg::init()?;

            // 创建 H.265 解码器
            let codec = ffmpeg::decoder::find(ffmpeg::codec::Id::HEVC)
                .context("HEVC decoder not found (install libavcodec-dev / libavcodec-extra)")?;
            let decoder = ffmpeg::codec::Context::new()
                .decoder()
                .open_as(codec)
                .context("Failed to open HEVC decoder")?
                .video()?;

            // 初始化 SDL2
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
            // 处理 SDL 事件 (退出检测)
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

            // 发送数据给解码器
            let mut packet = ffmpeg::Packet::new(au.len());
            if let Some(data) = packet.data_mut() {
                data.copy_from_slice(au);
            }
            self.decoder.send_packet(&packet)?;

            // 接收并渲染所有可用帧
            let mut frame = ffmpeg::frame::Video::empty();
            loop {
                match self.decoder.receive_frame(&mut frame) {
                    Ok(()) => self.render_frame(&frame)?,
                    Err(_) => break, // EAGAIN = 需要更多数据, 其他错误也停止
                }
            }

            Ok(true)
        }

        fn render_frame(&mut self, frame: &ffmpeg::frame::Video) -> Result<()> {
            use ffmpeg::format::pixel::Pixel;

            let w = frame.width();
            let h = frame.height();

            // 分辨率变化 → 重建 texture + 调整窗口
            if w != self.width || h != self.height || self.texture.is_none() {
                self.width = w;
                self.height = h;
                let tc = self.canvas.texture_creator();
                let tex = map_sdl(
                    tc.create_texture_streaming(PixelFormatEnum::IYUV, w, h),
                    "create_texture",
                )?;
                // SAFETY: tex 的生命周期绑定到 tc, tc 绑定到 self.canvas。
                // 我们将 tex 存储在 self.texture 中 (声明在 canvas 之前, 先 drop)。
                let tex: sdl2::render::Texture<'static> =
                    unsafe { std::mem::transmute::<sdl2::render::Texture<'_>, sdl2::render::Texture<'static>>(tex) };
                self.texture = Some(tex);
                map_sdl(self.canvas.window_mut().set_size(w, h), "set_size")?;
                println!("[Player] Video: {w}x{h} ({:?})", frame.format());
            }

            // 获取 YUV 平面数据 (转为 owned 以避免借用冲突)
            let (y, ys, u, us, v, vs) = if frame.format() == Pixel::YUV420P {
                (
                    frame.data(0).to_vec(), frame.stride(0) as usize,
                    frame.data(1).to_vec(), frame.stride(1) as usize,
                    frame.data(2).to_vec(), frame.stride(2) as usize,
                )
            } else {
                // 创建/更新 scaler
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
                // 转换到 YUV420P
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

            // 更新 texture + 渲染
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

    // SDL 音频回调使用的队列
    struct AudioQueue {
        buffer: std::collections::VecDeque<u8>,
    }

    impl sdl2::audio::AudioCallback for AudioQueue {
        type Channel = i16;

        fn callback(&mut self, out: &mut [i16]) {
            for sample in out.iter_mut() {
                // 从队列读取 2 字节 (一个 i16 采样)
                if self.buffer.len() >= 2 {
                    let lo = self.buffer.pop_front().unwrap();
                    let hi = self.buffer.pop_front().unwrap();
                    *sample = i16::from_le_bytes([lo, hi]);
                } else {
                    *sample = 0; // 队列空, 输出静音
                }
            }
        }
    }

    impl AudioPlayer {
        /// 创建音频播放器 (16kHz mono PCM16LE)
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

        /// 写入 PCM 数据 (16LE mono)
        pub fn write(&mut self, data: &[u8]) {
            let mut queue = self.device.lock();
            queue.buffer.extend(data);
            // 限制缓冲大小, 避免延迟过大 (最多 0.5 秒)
            let max_bytes = (self.sample_rate as usize) * 2 / 2; // 0.5s * 2 bytes
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
    ping: ping::Behaviour,
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
            ping: ping::Behaviour::new(
                ping::Config::default()
                    .with_interval(Duration::from_secs(10)),
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
