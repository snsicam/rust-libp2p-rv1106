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
//! 回调签名: fn(chn_id, data, len, pts_us, is_keyframe)

use bytes::Bytes;
use crossbeam_channel::Sender;
use proto::media_packet::MediaPacket;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

/// C 侧的帧回调签名: fn(chn_id, data, len, pts_us, is_keyframe)
type FrameCallback = extern "C" fn(std::ffi::c_int, *const u8, u32, u64, std::ffi::c_int);
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
    ) -> std::ffi::c_int;
    fn rk_camera_set_callback(cb: FrameCallback);
    fn rk_camera_request_idr(chn_id: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_camera_request_idr_all() -> std::ffi::c_int;
    fn rk_camera_deinit();

    fn rk_audio_init(sample_rate: std::ffi::c_int) -> std::ffi::c_int;
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

/// 获取指定通道缓存的 VPS/SPS/PPS
pub fn get_param_sets(chn_id: usize) -> Vec<Vec<u8>> {
    let all = GLOBAL_PARAM_SETS.lock().unwrap();
    all.get(chn_id).cloned().unwrap_or_default()
}

/// 获取所有通道的参数集
pub fn get_all_param_sets() -> Vec<Vec<Vec<u8>>> {
    GLOBAL_PARAM_SETS.lock().unwrap().clone()
}

// ---- 全局状态 ----

const MAX_CHN: usize = 3;

/// 每通道的 crossbeam sender (C 回调写入)
static GLOBAL_SENDERS: [Mutex<Option<Sender<MediaPacket>>>; MAX_CHN] = [
    Mutex::new(None),
    Mutex::new(None),
    Mutex::new(None),
];
/// 每通道缓存的 VPS/SPS/PPS
static GLOBAL_PARAM_SETS: Mutex<Vec<Vec<Vec<u8>>>> = Mutex::new(Vec::new());
static GLOBAL_START_TIMES: [Mutex<Option<Instant>>; MAX_CHN] = [
    Mutex::new(None),
    Mutex::new(None),
    Mutex::new(None),
];

/// C 帧回调 — 在 VENC 取流线程中调用
/// chn_id: 0=main, 1=sub, 2=third
extern "C" fn on_frame(
    chn_id: std::ffi::c_int, data: *const u8, len: u32,
    pts_us: u64, is_keyframe: std::ffi::c_int,
) {
    let chn = chn_id as usize;
    if chn >= MAX_CHN { return; }

    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };

    // 查找 NAL type 并缓存参数集
    let nal_type = slice.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0);
    if nal_type == 32 || nal_type == 33 || nal_type == 34 {
        // VPS(32) / SPS(33) / PPS(34) for H.265
        // SPS(7) / PPS(8) for H.264
        if let Ok(mut all) = GLOBAL_PARAM_SETS.lock() {
            while all.len() <= chn { all.push(Vec::new()); }
            let ps = &mut all[chn];
            ps.retain(|n| n.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0) != nal_type);
            ps.push(slice.to_vec());
        }
    }
    // Also cache H.264 param sets (nal_type 7=SPS, 8=PPS)
    if nal_type == 7 || nal_type == 8 {
        if let Ok(mut all) = GLOBAL_PARAM_SETS.lock() {
            while all.len() <= chn { all.push(Vec::new()); }
            let ps = &mut all[chn];
            ps.retain(|n| n.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0) != nal_type);
            ps.push(slice.to_vec());
        }
    }

    let timestamp_ms = GLOBAL_START_TIMES[chn].lock()
        .ok()
        .and_then(|t| t.as_ref().map(|s| s.elapsed().as_millis() as u64))
        .unwrap_or(pts_us / 1000);

    let packet = MediaPacket::video(
        timestamp_ms,
        is_keyframe != 0,
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
}

impl RkVideoSource {
    pub fn new(
        main: StreamParams,
        sub: Option<StreamParams>,
        third: Option<StreamParams>,
    ) -> Self {
        Self {
            main_params: main,
            sub_params: sub,
            third_params: third,
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

            // 清除旧的 param_sets
            {
                let mut ps = GLOBAL_PARAM_SETS.lock().unwrap();
                ps.clear();
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
            let ret = unsafe {
                rk_camera_init(
                    main.width as i32, main.height as i32,
                    main_fps as i32, main.bitrate_kbps as i32,
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

// ============== 音频源 (不变) ==============

static GLOBAL_AUDIO_SENDER: Mutex<Option<Sender<MediaPacket>>> = Mutex::new(None);
static GLOBAL_AUDIO_START_TIME: Mutex<Option<Instant>> = Mutex::new(None);

extern "C" fn on_audio_frame(data: *const u8, len: u32, _pts_us: u64) {
    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };

    let timestamp_ms = GLOBAL_AUDIO_START_TIME.lock()
        .ok()
        .and_then(|t| t.as_ref().map(|s| s.elapsed().as_millis() as u64))
        .unwrap_or(0);

    let packet = MediaPacket::audio_pcm(timestamp_ms, Bytes::copy_from_slice(slice));

    if let Ok(sender) = GLOBAL_AUDIO_SENDER.lock() {
        if let Some(tx) = sender.as_ref() {
            let _ = tx.send(packet);
        }
    }
}

pub struct RkAudioSource {
    sample_rate: u32,
}

impl RkAudioSource {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate }
    }

    pub fn spawn(self, sender: Sender<MediaPacket>) -> thread::JoinHandle<()> {
        let sample_rate = self.sample_rate;

        thread::spawn(move || {
            {
                let mut guard = GLOBAL_AUDIO_SENDER.lock().unwrap();
                *guard = Some(sender);
            }
            {
                let mut guard = GLOBAL_AUDIO_START_TIME.lock().unwrap();
                *guard = Some(Instant::now());
            }

            let ret = unsafe { rk_audio_init(sample_rate as i32) };
            if ret != 0 {
                eprintln!("[RkAudioSource] rk_audio_init failed: {}", ret);
                return;
            }

            unsafe { rk_audio_set_callback(on_audio_frame); }
            println!("[RkAudioSource] Audio started ({}Hz)", sample_rate);

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
