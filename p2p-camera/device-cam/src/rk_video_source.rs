//! RV1106 摄像头视频源 — 通过 Rockchip SDK 获取 H.265 硬编码流
//!
//! 编译要求: --features rv1106 + 交叉编译到 armv7-unknown-linux-gnueabihf
//! 链接: librk_camera.so (由 rk_camera.c 编译) + librockit_full.so + librkaiq.so
//!
//! 数据流:
//!   VI (摄像头) → VPSS → VENC (H.265 硬编码) → 回调 → crossbeam → broadcast

use bytes::Bytes;
use crossbeam_channel::Sender;
use proto::media_packet::MediaPacket;
use std::sync::Arc;
use std::sync::Mutex;
use std::thread;
use std::time::Instant;

/// C 侧的帧回调签名: fn(data, len, pts_us, is_keyframe)
type FrameCallback = extern "C" fn(*const u8, u32, u64, std::ffi::c_int);

extern "C" {
    fn rk_camera_init(width: std::ffi::c_int, height: std::ffi::c_int,
                      fps: std::ffi::c_int, bitrate_kbps: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_camera_set_callback(cb: FrameCallback);
    fn rk_camera_request_idr() -> std::ffi::c_int;
    fn rk_camera_deinit();
}

/// 请求 IDR 关键帧 — 用于 broadcast 丢帧后让 viewer 解码器重新同步
pub fn request_idr() {
    unsafe { rk_camera_request_idr(); }
}

/// 获取当前缓存的 VPS/SPS/PPS (从全局状态实时读取)
/// 新 viewer 连接时调用，确保拿到最新的参数集
pub fn get_param_sets() -> Vec<Vec<u8>> {
    GLOBAL_PARAM_SETS.lock().map(|ps| ps.clone()).unwrap_or_default()
}

/// 全局状态 — C 回调是全局函数, 需要全局访问 sender
/// 用 Arc<Mutex<Option<Sender>>> 存储
static GLOBAL_SENDER: Mutex<Option<Sender<MediaPacket>>> = Mutex::new(None);
static GLOBAL_PARAM_SETS: Mutex<Vec<Vec<u8>>> = Mutex::new(Vec::new());
static GLOBAL_START_TIME: Mutex<Option<Instant>> = Mutex::new(None);

/// C 回调函数 — 在 VENC 取流线程中调用
/// 将 H.265 NAL 数据封装为 MediaPacket 发送到 crossbeam channel
extern "C" fn on_frame(data: *const u8, len: u32, pts_us: u64, is_keyframe: std::ffi::c_int) {
    // SAFETY: data 来自 SDK, len 是有效长度
    let slice = unsafe { std::slice::from_raw_parts(data, len as usize) };

    // 查找 NAL type (跳过可能的 start code)
    let nal_type = slice.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0);

    // 缓存 VPS/SPS/PPS
    if nal_type == 32 || nal_type == 33 || nal_type == 34 {
        if let Ok(mut ps) = GLOBAL_PARAM_SETS.lock() {
            ps.retain(|n| n.first().map(|b| (b >> 1) & 0x3F).unwrap_or(0) != nal_type);
            ps.push(slice.to_vec());
        }
    }

    // 计算时间戳 (ms)
    let timestamp_ms = GLOBAL_START_TIME.lock()
        .ok()
        .and_then(|t| t.as_ref().map(|s| s.elapsed().as_millis() as u64))
        .unwrap_or(pts_us / 1000);

    let packet = MediaPacket::video(
        timestamp_ms,
        is_keyframe != 0,
        Bytes::copy_from_slice(slice),
    );

    if let Ok(sender) = GLOBAL_SENDER.lock() {
        if let Some(tx) = sender.as_ref() {
            let _ = tx.send(packet); // 接收端关闭时忽略
        }
    }
}

/// RV1106 摄像头视频源
pub struct RkVideoSource {
    width: u32,
    height: u32,
    fps: u32,
    bitrate_kbps: u32,
    /// 参数集缓存 handle (供 device-cam 在新 viewer 连接时读取)
    param_sets: Arc<Mutex<Option<Vec<Vec<u8>>>>>,
}

impl RkVideoSource {
    pub fn new(width: u32, height: u32, fps: u32, bitrate_kbps: u32) -> Self {
        Self {
            width,
            height,
            fps,
            bitrate_kbps,
            param_sets: Arc::new(Mutex::new(None)),
        }
    }

    /// 获取参数集缓存的 Arc clone
    pub fn param_sets_handle(&self) -> Arc<Mutex<Option<Vec<Vec<u8>>>>> {
        self.param_sets.clone()
    }

    /// 在独立线程中启动摄像头
    /// 返回 (JoinHandle, start_trigger) — 与 FileVideoSource 接口一致
    pub fn spawn(self, sender: Sender<MediaPacket>) -> (thread::JoinHandle<()>, Sender<()>) {
        let (start_tx, start_rx) = crossbeam_channel::bounded::<()>(1);
        let param_sets = self.param_sets.clone();
        let (width, height, fps, bitrate) = (self.width, self.height, self.fps, self.bitrate_kbps);

        let handle = thread::spawn(move || {
            // 等待开始信号 (第一个 viewer 连接时触发)
            let _ = start_rx.recv();

            println!("[RkVideoSource] Starting camera {}x{} @{}fps", width, height, fps);

            // 设置全局状态
            {
                let mut sender_guard = GLOBAL_SENDER.lock().unwrap();
                *sender_guard = Some(sender);
            }
            {
                let mut time_guard = GLOBAL_START_TIME.lock().unwrap();
                *time_guard = Some(Instant::now());
            }

            // 初始化摄像头
            let ret = unsafe {
                rk_camera_init(width as i32, height as i32, fps as i32, bitrate as i32)
            };
            if ret != 0 {
                eprintln!("[RkVideoSource] rk_camera_init failed: {}", ret);
                return;
            }

            // 设置回调
            unsafe { rk_camera_set_callback(on_frame); }

            println!("[RkVideoSource] Camera started, waiting for frames...");

            // 持续运行, 直到 sender 关闭 (主线程会通过 drop sender 来停止)
            // 这里用一个轻量循环等待
            loop {
                // 检查是否还需要继续 (sender 是否还有人接收)
                let should_stop = GLOBAL_SENDER.lock()
                    .map(|s| s.is_none())
                    .unwrap_or(true);
                if should_stop {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(1000));
            }

            // 清理
            unsafe { rk_camera_deinit(); }

            // 把缓存的 param_sets 复制到 Arc
            if let Ok(ps) = GLOBAL_PARAM_SETS.lock() {
                if let Ok(mut dest) = param_sets.lock() {
                    *dest = Some(ps.clone());
                }
            }

            println!("[RkVideoSource] Camera stopped");
        });

        (handle, start_tx)
    }
}

impl Drop for RkVideoSource {
    fn drop(&mut self) {
        // 清理全局 sender
        if let Ok(mut s) = GLOBAL_SENDER.lock() {
            *s = None;
        }
    }
}

// ============== 音频源 ==============

/// C 侧的音频回调签名: fn(data, len, pts_us)
type AudioCallback = extern "C" fn(*const u8, u32, u64);

extern "C" {
    fn rk_audio_init(sample_rate: std::ffi::c_int) -> std::ffi::c_int;
    fn rk_audio_set_callback(cb: AudioCallback);
    fn rk_audio_deinit();
}

/// 全局音频状态
static GLOBAL_AUDIO_SENDER: Mutex<Option<Sender<MediaPacket>>> = Mutex::new(None);
static GLOBAL_AUDIO_START_TIME: Mutex<Option<Instant>> = Mutex::new(None);

/// C 音频回调 — 在 AI 取流线程中调用
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

/// RV1106 真实音频源 (AI 采集 PCM)
pub struct RkAudioSource {
    sample_rate: u32,
}

impl RkAudioSource {
    pub fn new(sample_rate: u32) -> Self {
        Self { sample_rate }
    }

    /// 在独立线程中启动音频采集
    pub fn spawn(self, sender: Sender<MediaPacket>) -> thread::JoinHandle<()> {
        let sample_rate = self.sample_rate;

        thread::spawn(move || {
            // 设置全局状态
            {
                let mut sender_guard = GLOBAL_AUDIO_SENDER.lock().unwrap();
                *sender_guard = Some(sender);
            }
            {
                let mut time_guard = GLOBAL_AUDIO_START_TIME.lock().unwrap();
                *time_guard = Some(Instant::now());
            }

            // 初始化音频
            let ret = unsafe { rk_audio_init(sample_rate as i32) };
            if ret != 0 {
                eprintln!("[RkAudioSource] rk_audio_init failed: {}", ret);
                return;
            }

            // 设置回调
            unsafe { rk_audio_set_callback(on_audio_frame); }

            println!("[RkAudioSource] Audio started ({}Hz)", sample_rate);

            // 等待停止
            loop {
                let should_stop = GLOBAL_AUDIO_SENDER.lock()
                    .map(|s| s.is_none())
                    .unwrap_or(true);
                if should_stop {
                    break;
                }
                thread::sleep(std::time::Duration::from_millis(1000));
            }

            // 清理
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
