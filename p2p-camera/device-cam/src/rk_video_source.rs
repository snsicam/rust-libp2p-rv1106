//! RV1106 摄像头视频源 — 通过 Rockchip SDK 获取 H.265/H.264 硬编码流 (三码流)
//!
//! 编译要求: --features rv1106 + 交叉编译到 armv7-unknown-linux-gnueabihf
//! 链接: librk_camera.a (由 rk_camera.c 编译) + librockit_full.so + librkaiq.so
//!
//! 数据流 (三码流):
//!   VI → VPSS ┬→ VENC_chn0 (主码流) → 回调 → crossbeam → broadcast[0]
//!             ├→ VENC_chn1 (子码流) → 回调 → crossbeam → broadcast[1]
//!             └→ VENC_chn2 (第三码流) → 回调 → crossbeam → broadcast[2]
//!
//! 回调签名: fn(chn_id, data, len, pts_us)

use bytes::Bytes;
use crossbeam_channel::Sender;
use proto::media_packet::MediaPacket;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

/// C 侧的帧回调签名: fn(chn_id, data, len, pts_us)
/// 注意: 不再传 is_keyframe —— 关键帧判定改由 viewer 侧字节扫描完成。
type FrameCallback = extern "C" fn(std::ffi::c_int, *const u8, u32, u64);
type AudioCallback = extern "C" fn(*const u8, u32, u64);

extern "C" {
    fn rk_camera_set_chn_config(
        chn_id: std::ffi::c_int,
        codec: *const std::ffi::c_char,
        width: std::ffi::c_int, height: std::ffi::c_int,
        src_fps_num: std::ffi::c_int, src_fps_den: std::ffi::c_int,
        dst_fps_num: std::ffi::c_int, dst_fps_den: std::ffi::c_int,
        bitrate_kbps: std::ffi::c_int,
        rc_mode: *const std::ffi::c_char,
        rc_quality: *const std::ffi::c_char,
        gop: std::ffi::c_int,
        gop_mode: *const std::ffi::c_char,
        h264_profile: *const std::ffi::c_char,
        smartp_viridrlen: std::ffi::c_int,
        stream_buf_cnt: std::ffi::c_int,
        mirror: *const std::ffi::c_char,
    );
    fn rk_camera_init(
        main_w: std::ffi::c_int, main_h: std::ffi::c_int,
        fps: std::ffi::c_int, bitrate_kbps: std::ffi::c_int,
        sensor_fps: std::ffi::c_int,
    ) -> std::ffi::c_int;
    fn rk_camera_set_callback(cb: FrameCallback);
    fn rk_camera_request_idr(chn_id: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_camera_request_idr_all() -> std::ffi::c_int;
    fn rk_camera_deinit();

    fn rk_audio_init(
        sample_rate: std::ffi::c_int,
        card_name: *const std::ffi::c_char,
        channels: std::ffi::c_int,
        frame_size: std::ffi::c_int,
        volume: std::ffi::c_int,
        encode_type: *const std::ffi::c_char,
        format: *const std::ffi::c_char,
        bit_rate: std::ffi::c_int,
        enable_vqe: std::ffi::c_int,
        vqe_cfg: *const std::ffi::c_char,
    ) -> std::ffi::c_int;
    fn rk_audio_set_callback(cb: AudioCallback);
    fn rk_audio_deinit();
}

// ---- 码流配置 (从 TOML 传入) ----

/// 单个码流的编码参数
#[derive(Debug, Clone)]
pub struct StreamParams {
    pub codec: String,
    pub width: u32,
    pub height: u32,
    pub src_fps_num: u32,
    pub src_fps_den: u32,
    pub dst_fps_num: u32,
    pub dst_fps_den: u32,
    pub bitrate_kbps: u32,
    pub rc_mode: String,
    pub rc_quality: String,
    pub gop: u32,
    pub gop_mode: String,
    pub h264_profile: String,
    pub smartp_viridrlen: u32,
    pub stream_buf_cnt: u32,
    pub mirror: String,
}

impl Default for StreamParams {
    fn default() -> Self {
        Self {
            codec: "H265".to_string(),
            width: 2304,
            height: 1296,
            src_fps_num: 25, src_fps_den: 1,
            dst_fps_num: 25, dst_fps_den: 1,
            bitrate_kbps: 2048,
            rc_mode: "CBR".to_string(),
            rc_quality: "high".to_string(),
            gop: 50,
            gop_mode: "normalP".to_string(),
            h264_profile: "high".to_string(),
            smartp_viridrlen: 25,
            stream_buf_cnt: 2,
            mirror: "none".to_string(),
        }
    }
}

/// 请求特定通道的 IDR 关键帧
pub fn request_idr(chn_id: u8) {
    unsafe { rk_camera_request_idr(chn_id as i32); }
}

/// 请求所有通道的 IDR
pub fn request_idr_all() {
    unsafe { rk_camera_request_idr_all(); }
}

// ---- 全局状态 ----

const MAX_CHN: usize = 3;

/// 每通道的 crossbeam sender (C 回调写入)
static GLOBAL_SENDERS: [Mutex<Option<Sender<MediaPacket>>>; MAX_CHN] = [
    Mutex::new(None),
    Mutex::new(None),
    Mutex::new(None),
];
static GLOBAL_START_TIMES: [Mutex<Option<Instant>>; MAX_CHN] = [
    Mutex::new(None),
    Mutex::new(None),
    Mutex::new(None),
];

/// C 帧回调 — 在 VENC 取流线程中调用
/// chn_id: 0=main, 1=sub, 2=third
extern "C" fn on_frame(
    chn_id: std::ffi::c_int, data: *const u8, len: u32,
    pts_us: u64,
) {
    let chn = chn_id as usize;
    if chn >= MAX_CHN { return; }

    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };

    // VENC pack_mode=0 时, 一个 pack 内含多个 NAL (VPS/SPS/PPS/IDR...), 且每个 NAL 前带
    // Annex B start code (00 00 00 01 或 00 00 01)。因此:
    //   - 不能取 slice.first() 当 NAL header (那是 start code 的 0x00);
    //   - 也不能只读第一个 NAL (H.265 IDR 帧的首个 NAL 是 VPS, 而非 IDR)。
    // 关键帧标志(cam 侧)已不再计算: C 回调 rk_camera.c 不再扫描 NAL、也不再传 is_keyframe,
    // 关键帧判定由 viewer 侧字节扫描 `is_nal_keyframe` 完成(receiver 自包含、不信任对端 flag),
    // 且 JNI 桥根本不把 flag 转发给原生 APP。`MediaPacket::video` 也不再接收 is_keyframe 参数
    // (视频包 flags 字节保留置 0), 关键帧完全由接收端判定。
    // 参数集(VPS/SPS/PPS)不在此缓存: 实测 RK 编码器默认在每个 IDR/CRA 前内联携带
    // VPS/SPS/PPS, viewer 等到首个 IDR 即可直接解码, 无需补发 init_nals。

    let timestamp_ms = GLOBAL_START_TIMES[chn].lock()
        .ok()
        .and_then(|t| t.as_ref().map(|s| s.elapsed().as_millis() as u64))
        .unwrap_or(pts_us / 1000);

    let packet = MediaPacket::video(
        timestamp_ms,
        Bytes::copy_from_slice(slice),
    );

    if let Ok(sender) = GLOBAL_SENDERS[chn].lock() {
        if let Some(tx) = sender.as_ref() {
            let _ = tx.send(packet);
        }
    }
}

/// RV1106 摄像头视频源 (三码流)
pub struct RkVideoSource {
    main_params: StreamParams,
    sub_params: Option<StreamParams>,
    third_params: Option<StreamParams>,
    /// sensor 原生输出帧率 (摄像头模组实际产出率, 如 30)。
    /// 必须等于 VENC 实际接收帧率, 编码器才能按目标帧率正确丢帧;
    /// 不能用某码流的配置 fps 充当, 否则 ratio=1 不丢帧、实测跑满原生帧率。
    sensor_frame_rate: u32,
}

impl RkVideoSource {
    pub fn new(
        main: StreamParams,
        sub: Option<StreamParams>,
        third: Option<StreamParams>,
        sensor_frame_rate: u32,
    ) -> Self {
        Self {
            main_params: main,
            sub_params: sub,
            third_params: third,
            sensor_frame_rate: if sensor_frame_rate > 0 { sensor_frame_rate } else { 30 },
        }
    }

    /// 在独立线程中启动摄像头 (三码流)
    /// 返回 (JoinHandle, Vec<(StreamType, Sender<()>)>)
    pub fn spawn(
        self,
        main_sender: Sender<MediaPacket>,
        sub_sender: Option<Sender<MediaPacket>>,
        third_sender: Option<Sender<MediaPacket>>,
    ) -> (thread::JoinHandle<()>, Sender<()>) {
        let (start_tx, start_rx) = crossbeam_channel::bounded::<()>(1);
        let main = self.main_params.clone();
        let sub = self.sub_params.clone();
        let third = self.third_params.clone();

        let handle = thread::spawn(move || {
            let _ = start_rx.recv();

            let streams_enabled: Vec<&str> = {
                let mut v = vec!["main"];
                if sub.is_some() { v.push("sub"); }
                if third.is_some() { v.push("third"); }
                v
            };

            println!("[RkVideoSource] Starting camera: {}x{} @{}fps, streams: {:?}",
                     main.width, main.height,
                     main.dst_fps_num / main.dst_fps_den,
                     streams_enabled);

            // 设置全局 sender
            {
                let mut s = GLOBAL_SENDERS[0].lock().unwrap();
                *s = Some(main_sender);
            }
            if let Some(s) = sub_sender {
                let mut guard = GLOBAL_SENDERS[1].lock().unwrap();
                *guard = Some(s);
            }
            if let Some(s) = third_sender {
                let mut guard = GLOBAL_SENDERS[2].lock().unwrap();
                *guard = Some(s);
            }

            // 设置全局 start time
            for i in 0..MAX_CHN {
                let mut guard = GLOBAL_START_TIMES[i].lock().unwrap();
                *guard = Some(Instant::now());
            }

            // ---- 调用 C 侧 rk_camera_set_chn_config ----
            // 主码流
            unsafe {
                let codec_c = std::ffi::CString::new(main.codec.as_str()).unwrap();
                let rc_c = std::ffi::CString::new(main.rc_mode.as_str()).unwrap();
                let q_c = std::ffi::CString::new(main.rc_quality.as_str()).unwrap();
                let gopm_c = std::ffi::CString::new(main.gop_mode.as_str()).unwrap();
                let prof_c = std::ffi::CString::new(main.h264_profile.as_str()).unwrap();
                let mir_c = std::ffi::CString::new(main.mirror.as_str()).unwrap();

                rk_camera_set_chn_config(
                    0, codec_c.as_ptr(),
                    main.width as i32, main.height as i32,
                    main.src_fps_num as i32, main.src_fps_den as i32,
                    main.dst_fps_num as i32, main.dst_fps_den as i32,
                    main.bitrate_kbps as i32,
                    rc_c.as_ptr(), q_c.as_ptr(),
                    main.gop as i32, gopm_c.as_ptr(),
                    prof_c.as_ptr(),
                    main.smartp_viridrlen as i32,
                    main.stream_buf_cnt as i32,
                    mir_c.as_ptr(),
                );
            }

            // 子码流
            if let Some(ref p) = sub {
                unsafe {
                    let codec_c = std::ffi::CString::new(p.codec.as_str()).unwrap();
                    let rc_c = std::ffi::CString::new(p.rc_mode.as_str()).unwrap();
                    let q_c = std::ffi::CString::new(p.rc_quality.as_str()).unwrap();
                    let gopm_c = std::ffi::CString::new(p.gop_mode.as_str()).unwrap();
                    let prof_c = std::ffi::CString::new(p.h264_profile.as_str()).unwrap();
                    let mir_c = std::ffi::CString::new(p.mirror.as_str()).unwrap();

                    rk_camera_set_chn_config(
                        1, codec_c.as_ptr(),
                        p.width as i32, p.height as i32,
                        p.src_fps_num as i32, p.src_fps_den as i32,
                        p.dst_fps_num as i32, p.dst_fps_den as i32,
                        p.bitrate_kbps as i32,
                        rc_c.as_ptr(), q_c.as_ptr(),
                        p.gop as i32, gopm_c.as_ptr(),
                        prof_c.as_ptr(),
                        p.smartp_viridrlen as i32,
                        p.stream_buf_cnt as i32,
                        mir_c.as_ptr(),
                    );
                }
            }

            // 第三码流
            if let Some(ref p) = third {
                unsafe {
                    let codec_c = std::ffi::CString::new(p.codec.as_str()).unwrap();
                    let rc_c = std::ffi::CString::new(p.rc_mode.as_str()).unwrap();
                    let q_c = std::ffi::CString::new(p.rc_quality.as_str()).unwrap();
                    let gopm_c = std::ffi::CString::new(p.gop_mode.as_str()).unwrap();
                    let prof_c = std::ffi::CString::new(p.h264_profile.as_str()).unwrap();
                    let mir_c = std::ffi::CString::new(p.mirror.as_str()).unwrap();

                    rk_camera_set_chn_config(
                        2, codec_c.as_ptr(),
                        p.width as i32, p.height as i32,
                        p.src_fps_num as i32, p.src_fps_den as i32,
                        p.dst_fps_num as i32, p.dst_fps_den as i32,
                        p.bitrate_kbps as i32,
                        rc_c.as_ptr(), q_c.as_ptr(),
                        p.gop as i32, gopm_c.as_ptr(),
                        prof_c.as_ptr(),
                        p.smartp_viridrlen as i32,
                        p.stream_buf_cnt as i32,
                        mir_c.as_ptr(),
                    );
                }
            }

            // 初始化摄像头硬件
            let main_fps = main.dst_fps_num / main.dst_fps_den.max(1);
            // sensor 原生帧率 (来自配置 sensor_frame_rate, 默认 30), 而非某码流配置 fps。
            // 这是 VENC 的实际输入帧率, 编码器据此按目标帧率丢帧 (对标 rkipc isp.0.adjustment:fps)。
            let sensor_fps = self.sensor_frame_rate;
            let ret = unsafe {
                rk_camera_init(
                    main.width as i32, main.height as i32,
                    main_fps as i32, main.bitrate_kbps as i32,
                    sensor_fps as i32,
                )
            };
            if ret != 0 {
                eprintln!("[RkVideoSource] rk_camera_init failed: {}", ret);
                return;
            }

            // 设置帧回调
            unsafe { rk_camera_set_callback(on_frame); }

            println!("[RkVideoSource] Camera started, waiting for frames...");

            // 持续运行直到停止
            loop {
                let should_stop = GLOBAL_SENDERS.iter().all(|s| {
                    s.lock().map(|g| g.is_none()).unwrap_or(true)
                });
                if should_stop {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(1000));
            }

            unsafe { rk_camera_deinit(); }
            println!("[RkVideoSource] Camera stopped");
        });

        (handle, start_tx)
    }
}

impl Drop for RkVideoSource {
    fn drop(&mut self) {
        for i in 0..MAX_CHN {
            if let Ok(mut s) = GLOBAL_SENDERS[i].lock() {
                *s = None;
            }
        }
    }
}

// ============== 音频源 (AI + [AENC 编码]) ==============

static GLOBAL_AUDIO_SENDER: Mutex<Option<Sender<MediaPacket>>> = Mutex::new(None);
static GLOBAL_AUDIO_START_TIME: Mutex<Option<Instant>> = Mutex::new(None);
/// Audio flags for MediaPacket: 0=PCM16LE, 2=G711A, 3=G711U
static GLOBAL_AUDIO_FLAGS: AtomicU8 = AtomicU8::new(0);

extern "C" fn on_audio_frame(data: *const u8, len: u32, _pts_us: u64) {
    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };

    let timestamp_ms = GLOBAL_AUDIO_START_TIME.lock()
        .ok()
        .and_then(|t| t.as_ref().map(|s| s.elapsed().as_millis() as u64))
        .unwrap_or(0);

    let flags = GLOBAL_AUDIO_FLAGS.load(Ordering::Relaxed);
    let packet = MediaPacket {
        track: proto::media_packet::MediaTrack::Audio,
        timestamp_ms,
        flags,
        data: Bytes::copy_from_slice(slice),
    };

    if let Ok(sender) = GLOBAL_AUDIO_SENDER.lock() {
        if let Some(tx) = sender.as_ref() {
            let _ = tx.send(packet);
        }
    }
}

pub struct RkAudioSource {
    sample_rate: u32,
    card_name: String,
    channels: u32,
    frame_size: u32,
    volume: u32,
    format: String,
    encode_type: String,
    bit_rate: u32,
    enable_vqe: bool,
    vqe_cfg: String,
}

impl RkAudioSource {
    pub fn new(
        sample_rate: u32, card_name: String, channels: u32, frame_size: u32, volume: u32,
        format: String, encode_type: String, bit_rate: u32,
        enable_vqe: bool, vqe_cfg: String,
    ) -> Self {
        Self {
            sample_rate, card_name, channels, frame_size, volume,
            format, encode_type, bit_rate, enable_vqe, vqe_cfg,
        }
    }

    /// 返回编码类型对应的 MediaPacket flags 值
    fn audio_flags(encode_type: &str) -> u8 {
        match encode_type {
            "G711A" => 2,
            "G711U" => 3,
            _ => 0, // PCM16LE
        }
    }

    pub fn spawn(self, sender: Sender<MediaPacket>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let card_name_c = std::ffi::CString::new(self.card_name.as_str())
                .expect("card_name must not contain null bytes");
            let encode_type_c = std::ffi::CString::new(self.encode_type.as_str())
                .expect("encode_type must not contain null bytes");
            let format_c = std::ffi::CString::new(self.format.as_str())
                .expect("format must not contain null bytes");
            let vqe_cfg_c = std::ffi::CString::new(self.vqe_cfg.as_str())
                .expect("vqe_cfg must not contain null bytes");

            let sample_rate = self.sample_rate;
            let channels = self.channels as i32;
            let frame_size = self.frame_size as i32;
            let volume = self.volume as i32;
            let bit_rate = self.bit_rate as i32;
            let enable_vqe = self.enable_vqe as i32;
            let encode_type_str = self.encode_type.clone();

            // Set global audio flags before any callback fires
            let flags = Self::audio_flags(&encode_type_str);
            GLOBAL_AUDIO_FLAGS.store(flags, Ordering::Relaxed);

            {
                let mut guard = GLOBAL_AUDIO_SENDER.lock().unwrap();
                *guard = Some(sender);
            }
            {
                let mut guard = GLOBAL_AUDIO_START_TIME.lock().unwrap();
                *guard = Some(Instant::now());
            }

            let ret = unsafe {
                rk_audio_init(
                    sample_rate as i32,
                    card_name_c.as_ptr(),
                    channels,
                    frame_size,
                    volume,
                    encode_type_c.as_ptr(),
                    format_c.as_ptr(),
                    bit_rate,
                    enable_vqe,
                    vqe_cfg_c.as_ptr(),
                )
            };
            if ret != 0 {
                eprintln!("[RkAudioSource] rk_audio_init failed: {}", ret);
                return;
            }

            unsafe { rk_audio_set_callback(on_audio_frame); }
            println!("[RkAudioSource] Audio started: {}Hz {}ch {}samples/frame encode={}",
                sample_rate, channels, frame_size, encode_type_str);

            loop {
                let should_stop = GLOBAL_AUDIO_SENDER.lock()
                    .map(|s| s.is_none())
                    .unwrap_or(true);
                if should_stop {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(1000));
            }

            unsafe { rk_audio_deinit(); }
            println!("[RkAudioSource] Audio stopped");
        })
    }
}

impl Drop for RkAudioSource {
    fn drop(&mut self) {
        if let Ok(mut s) = GLOBAL_AUDIO_SENDER.lock() {
            *s = None;
        }
    }
}

// ============== 控制通道 FFI 接口 (编码/图像/系统参数) ==============

extern "C" {
    // INI 持久化 (参考 rkipc rk_param_* 接口)
    fn rk_param_get_int(key: *const std::ffi::c_char, default: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_param_set_int(key: *const std::ffi::c_char, value: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_param_get_string(key: *const std::ffi::c_char, default: *const std::ffi::c_char) -> *mut std::ffi::c_char;
    fn rk_param_set_string(key: *const std::ffi::c_char, value: *const std::ffi::c_char) -> std::ffi::c_int;

    // ISP 图像参数 (参考 rkipc rk_isp_* 接口)
    fn rk_isp_get_contrast(cam_id: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_set_contrast(cam_id: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_get_brightness(cam_id: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_set_brightness(cam_id: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_get_saturation(cam_id: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_set_saturation(cam_id: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_get_sharpness(cam_id: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_set_sharpness(cam_id: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_get_hue(cam_id: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_isp_set_hue(cam_id: std::ffi::c_int, value: std::ffi::c_int) -> std::ffi::c_int;

    // 系统操作
    fn rk_system_reboot() -> std::ffi::c_int;
    fn rk_system_factory_reset() -> std::ffi::c_int;
}

use proto::control::{EncoderConfig, ImageConfig, ImageAdjustment, SystemConfig, SystemConfigSet};

/// 获取编码参数 (从 INI 配置读取)
pub fn get_encoder_config(chn_id: u32) -> Option<EncoderConfig> {
    let stream_prefix = match chn_id {
        0 => "video.0",
        1 => "video.1",
        2 => "video.2",
        _ => return None,
    };

    unsafe {
        let output_data_type = param_get_string(&format!("{stream_prefix}.output_data_type"), "H.265");
        let width = param_get_int(&format!("{stream_prefix}.width"), 2304) as u32;
        let height = param_get_int(&format!("{stream_prefix}.height"), 1296) as u32;
        let rc_mode = param_get_string(&format!("{stream_prefix}.rc_mode"), "CBR");
        let rc_quality = param_get_string(&format!("{stream_prefix}.rc_quality"), "high");
        let gop = param_get_int(&format!("{stream_prefix}.gop"), 50) as u32;
        let gop_mode = param_get_string(&format!("{stream_prefix}.gop_mode"), "normalP");
        let max_rate = param_get_int(&format!("{stream_prefix}.max_rate"), 2048) as u32;
        let dst_frame_rate_num = param_get_int(&format!("{stream_prefix}.dst_frame_rate_num"), 25) as u32;
        let dst_frame_rate_den = param_get_int(&format!("{stream_prefix}.dst_frame_rate_den"), 1) as u32;
        let h264_profile = param_get_string(&format!("{stream_prefix}.h264_profile"), "high");
        let smart = param_get_string(&format!("{stream_prefix}.smart"), "close");
        let rotation = param_get_int(&format!("{stream_prefix}.rotation"), 0) as u32;

        Some(EncoderConfig {
            output_data_type,
            width,
            height,
            rc_mode,
            rc_quality,
            gop,
            gop_mode,
            max_rate,
            dst_frame_rate_num,
            dst_frame_rate_den,
            h264_profile,
            smart,
            rotation,
        })
    }
}

/// 设置编码参数 (写入 INI + 热改/重建)
pub fn set_encoder_config(chn_id: u32, config: &EncoderConfig) -> anyhow::Result<()> {
    let stream_prefix = match chn_id {
        0 => "video.0",
        1 => "video.1",
        2 => "video.2",
        _ => return Err(anyhow::anyhow!("invalid chn_id: {chn_id}")),
    };

    unsafe {
        // 写入 INI 持久化
        param_set_string(&format!("{stream_prefix}.output_data_type"), &config.output_data_type);
        param_set_int(&format!("{stream_prefix}.width"), config.width as i32);
        param_set_int(&format!("{stream_prefix}.height"), config.height as i32);
        param_set_string(&format!("{stream_prefix}.rc_mode"), &config.rc_mode);
        param_set_string(&format!("{stream_prefix}.rc_quality"), &config.rc_quality);
        param_set_int(&format!("{stream_prefix}.gop"), config.gop as i32);
        param_set_string(&format!("{stream_prefix}.gop_mode"), &config.gop_mode);
        param_set_int(&format!("{stream_prefix}.max_rate"), config.max_rate as i32);
        param_set_int(&format!("{stream_prefix}.dst_frame_rate_num"), config.dst_frame_rate_num as i32);
        param_set_int(&format!("{stream_prefix}.dst_frame_rate_den"), config.dst_frame_rate_den as i32);
        param_set_string(&format!("{stream_prefix}.h264_profile"), &config.h264_profile);
        param_set_string(&format!("{stream_prefix}.smart"), &config.smart);
        param_set_int(&format!("{stream_prefix}.rotation"), config.rotation as i32);

        // TODO: 热改编码参数 — 需确认 RK MPI SDK 是否支持运行时修改
        // 可选方案:
        //   1. RK_MPI_VENC_SetChnAttr (热改码率/GOP/帧率, 不中断视频流)
        //   2. rk_camera_deinit + rk_camera_init (重建编码器, 短暂中断视频流)
        // 当前先仅持久化 INI, 编码参数在下次重启后生效
        tracing::warn!("[RkVideoSource] Encoder config saved to INI, hot-update not yet implemented");
    }

    Ok(())
}

/// 获取图像参数 (从 ISP AIQ 读取)
pub fn get_image_config(cam_id: u32) -> Option<ImageConfig> {
    let cam = cam_id as i32;

    unsafe {
        let adjustment = Some(ImageAdjustment {
            contrast: rk_isp_get_contrast(cam),
            brightness: rk_isp_get_brightness(cam),
            saturation: rk_isp_get_saturation(cam),
            sharpness: rk_isp_get_sharpness(cam),
            hue: rk_isp_get_hue(cam),
        });

        Some(ImageConfig {
            adjustment,
            // 其他子类别暂不实现, 后续按需扩展
            exposure: None,
            night_to_day: None,
            white_balance: None,
            enhancement: None,
            video_adjustment: None,
        })
    }
}

/// 设置图像参数 (调用 ISP AIQ 接口 + INI 持久化)
pub fn set_image_config(cam_id: u32, config: &ImageConfig) -> anyhow::Result<()> {
    let cam = cam_id as i32;

    unsafe {
        if let Some(ref adj) = config.adjustment {
            rk_isp_set_contrast(cam, adj.contrast);
            rk_isp_set_brightness(cam, adj.brightness);
            rk_isp_set_saturation(cam, adj.saturation);
            rk_isp_set_sharpness(cam, adj.sharpness);
            rk_isp_set_hue(cam, adj.hue);

            // INI 持久化
            let prefix = format!("isp.{}.adjustment", cam_id);
            param_set_int(&format!("{prefix}.contrast"), adj.contrast);
            param_set_int(&format!("{prefix}.brightness"), adj.brightness);
            param_set_int(&format!("{prefix}.saturation"), adj.saturation);
            param_set_int(&format!("{prefix}.sharpness"), adj.sharpness);
            param_set_int(&format!("{prefix}.hue"), adj.hue);
        }

        // 其他子类别暂不实现
        if config.exposure.is_some() {
            tracing::warn!("[RkVideoSource] ImageExposure set not yet implemented");
        }
        if config.night_to_day.is_some() {
            tracing::warn!("[RkVideoSource] ImageNightToDay set not yet implemented");
        }
        if config.white_balance.is_some() {
            tracing::warn!("[RkVideoSource] ImageWhiteBalance set not yet implemented");
        }
        if config.enhancement.is_some() {
            tracing::warn!("[RkVideoSource] ImageEnhancement set not yet implemented");
        }
        if config.video_adjustment.is_some() {
            tracing::warn!("[RkVideoSource] ImageVideoAdjustment set not yet implemented");
        }
    }

    Ok(())
}

/// 获取系统参数 (从 INI 读取)
pub fn get_system_config() -> Option<SystemConfig> {
    unsafe {
        let device_name = param_get_string("system.device_info.device_name", "RK IP Camera");
        let telecontrol_id = param_get_string("system.device_info.telecontrol_id", "0");
        let model = param_get_string("system.device_info.model", "RV1106");
        let serial_number = param_get_string("system.device_info.serial_number", "unknown");
        let firmware_version = param_get_string("system.device_info.firmware_version", "1.0.0");
        let manufacturer = param_get_string("system.device_info.manufacturer", "Rockchip");

        Some(SystemConfig {
            device_name,
            telecontrol_id,
            model,
            serial_number,
            firmware_version,
            manufacturer,
        })
    }
}

/// 设置系统参数 (写入 INI 持久化)
pub fn set_system_config(config: &SystemConfigSet) -> anyhow::Result<()> {
    unsafe {
        if let Some(ref name) = config.device_name {
            param_set_string("system.device_info.device_name", name);
        }
        if let Some(ref id) = config.telecontrol_id {
            param_set_string("system.device_info.telecontrol_id", id);
        }
    }
    Ok(())
}

/// 系统重启
pub fn system_reboot() -> anyhow::Result<()> {
    unsafe {
        let ret = rk_system_reboot();
        if ret != 0 {
            return Err(anyhow::anyhow!("rk_system_reboot failed: {ret}"));
        }
    }
    Ok(())
}

/// 恢复出厂设置
pub fn factory_reset() -> anyhow::Result<()> {
    unsafe {
        let ret = rk_system_factory_reset();
        if ret != 0 {
            return Err(anyhow::anyhow!("rk_system_factory_reset failed: {ret}"));
        }
    }
    Ok(())
}

// ---- INI 参数读写辅助 ----

/// 从 INI 读取整数
fn param_get_int(key: &str, default: i32) -> i32 {
    unsafe {
        let key_c = std::ffi::CString::new(key).unwrap();
        rk_param_get_int(key_c.as_ptr(), default)
    }
}

/// 写入 INI 整数
fn param_set_int(key: &str, value: i32) {
    unsafe {
        let key_c = std::ffi::CString::new(key).unwrap();
        rk_param_set_int(key_c.as_ptr(), value);
    }
}

/// 从 INI 读取字符串
fn param_get_string(key: &str, default: &str) -> String {
    unsafe {
        let key_c = std::ffi::CString::new(key).unwrap();
        let default_c = std::ffi::CString::new(default).unwrap();
        let ptr = rk_param_get_string(key_c.as_ptr(), default_c.as_ptr());
        if ptr.is_null() {
            return default.to_string();
        }
        let s = std::ffi::CStr::from_ptr(ptr).to_string_lossy().into_owned();
        // 注意: rk_param_get_string 返回的指针可能需要释放, 取决于 rkipc 实现
        // 当前假设返回静态缓冲区指针, 无需释放
        s
    }
}

/// 写入 INI 字符串
fn param_set_string(key: &str, value: &str) {
    unsafe {
        let key_c = std::ffi::CString::new(key).unwrap();
        let value_c = std::ffi::CString::new(value).unwrap();
        rk_param_set_string(key_c.as_ptr(), value_c.as_ptr());
    }
}
