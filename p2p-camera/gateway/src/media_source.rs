//! 媒体源抽象 — 从 SDK 回调 / 文件 读取音视频帧
//!
//! 当前阶段: 从文件模拟读取 (H.265 raw + PCM raw)
//! 后续替换: RV1106 SDK FFI 回调 → crossbeam_channel

use bytes::Bytes;
use crossbeam_channel::Sender;
use proto::media_packet::MediaPacket;
use std::thread;
use std::time::{Duration, Instant};

/// 模拟从 SDK 读取视频帧 (从 H.265 裸流文件)
pub struct FileVideoSource {
    /// 40ms 一个视频帧 (25fps)
    frame_interval: Duration,
    nal_units: Vec<Vec<u8>>,
    current: usize,
    /// 缓存最新的 VPS/SPS/PPS (用于新 viewer 快速恢复)
    /// 通过 Arc<Mutex> 共享给 gateway，在新 viewer 连接时读取
    latest_param_sets: std::sync::Arc<std::sync::Mutex<Option<Vec<Vec<u8>>>>>,
}

impl FileVideoSource {
    /// 从 H.265 裸流文件加载所有 access units (每帧画面)
    pub fn from_file(data: Vec<u8>) -> Self {
        let access_units = split_h265_access_units(&data);

        // 统计 NAL 类型分布 (从每个 AU 的第一个 NAL 判断)
        let mut vps = 0; let mut sps = 0; let mut pps = 0;
        let mut idr = 0; let mut other = 0;
        for au in &access_units {
            // AU 格式: [00 00 00 01] [NAL...] [00 00 00 01] [NAL...]
            // 找第一个 NAL 的 type
            if let Some(first_nal) = first_nal_in_au(au) {
                match nal_type(first_nal) {
                    32 => vps += 1,
                    33 => sps += 1,
                    34 => pps += 1,
                    19 | 20 => idr += 1,
                    _ => other += 1,
                }
            }
        }
        println!(
            "[Gateway] FileVideoSource: {} bytes → {} access units (VPS={}, SPS={}, PPS={}, IDR={}, other={})",
            data.len(), access_units.len(), vps, sps, pps, idr, other
        );

        Self {
            frame_interval: Duration::from_millis(40),
            nal_units: access_units,
            current: 0,
            latest_param_sets: std::sync::Arc::new(std::sync::Mutex::new(None)),
        }
    }

    /// 获取参数集缓存的 Arc clone (用于新 viewer 快速恢复)
    pub fn param_sets_handle(&self) -> std::sync::Arc<std::sync::Mutex<Option<Vec<Vec<u8>>>>> {
        self.latest_param_sets.clone()
    }

    /// 循环读取下一帧 (到头就循环)
    fn next_frame(&mut self) -> Vec<u8> {
        if self.nal_units.is_empty() {
            return vec![];
        }
        let frame = self.nal_units[self.current].clone();
        self.current = (self.current + 1) % self.nal_units.len();
        frame
    }

    /// 在独立线程中运行，通过 Sender 发送视频帧
    /// 返回 (JoinHandle, start_trigger) — 调用 start_trigger.send(()) 开始播放
    /// 这样可以让 gateway 在第一个 viewer 连接时才开始播放，从文件头发送
    pub fn spawn(self, sender: Sender<MediaPacket>) -> (thread::JoinHandle<()>, Sender<()>) {
        let (start_tx, start_rx) = crossbeam_channel::bounded::<()>(1);
        let handle = thread::spawn(move || {
            let mut this = self;
            // 等待开始信号 (第一个 viewer 连接时触发)
            let _ = start_rx.recv();

            let start = Instant::now();
            println!("[Gateway] Video source started (from beginning of file)");

            loop {
                let data = this.next_frame();
                if data.is_empty() {
                    thread::sleep(this.frame_interval);
                    continue;
                }

                let timestamp_ms = start.elapsed().as_millis() as u64;
                // 从 AU 的第一个 NAL 判断类型
                let first_nal = first_nal_in_au(&data).unwrap_or(&[]);
                let nal_t = nal_type(first_nal);
                let is_keyframe = nal_t == 19 || nal_t == 20;

                // 缓存 VPS/SPS/PPS (新 viewer 连接时需要)
                if nal_t == 32 || nal_t == 33 || nal_t == 34 {
                    if let Ok(mut ps) = this.latest_param_sets.lock() {
                        let ps_vec = ps.get_or_insert_with(Vec::new);
                        ps_vec.retain(|n| nal_type(n) != nal_t);
                        ps_vec.push(first_nal.to_vec());
                    }
                }

                let packet = MediaPacket::video(timestamp_ms, is_keyframe, Bytes::from(data));
                if sender.send(packet).is_err() {
                    break; // 接收端已关闭
                }

                thread::sleep(this.frame_interval);
            }
        });
        (handle, start_tx)
    }
}

/// 模拟音频源 (生成静音 PCM)
pub struct SilenceAudioSource {
    /// 20ms 一个音频帧
    frame_interval: Duration,
    sample_rate: u32,
    channels: u8,
}

impl SilenceAudioSource {
    pub fn new(sample_rate: u32, channels: u8) -> Self {
        Self {
            frame_interval: Duration::from_millis(20),
            sample_rate,
            channels,
        }
    }

    /// 生成一帧静音 PCM16LE 数据
    fn next_frame(&self) -> Vec<u8> {
        let samples_per_channel = (self.sample_rate as f64 * self.frame_interval.as_secs_f64()) as usize;
        let total_samples = samples_per_channel * self.channels as usize;
        let total_bytes = total_samples * 2; // 16bit = 2 bytes
        vec![0u8; total_bytes]
    }

    pub fn spawn(self, sender: Sender<MediaPacket>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let this = self;
            let start = Instant::now();
            loop {
                let data = this.next_frame();
                let timestamp_ms = start.elapsed().as_millis() as u64;
                let packet = MediaPacket::audio_pcm(timestamp_ms, Bytes::from(data));

                if sender.send(packet).is_err() {
                    break;
                }

                thread::sleep(this.frame_interval);
            }
        })
    }
}

/// 按 access unit 分组 H.265 NAL units (Annex B 格式)
/// 
/// 一个 access unit = 一帧画面，可能包含多个 NAL:
///   - VPS+SPS+PPS+IDR (关键帧)
///   - 一组 slice NAL (P/B 帧)
/// 
/// Access unit 边界判断:
///   1. VPS(32)/SPS(33)/PPS(34) 总是新 access unit 的开始
///   2. IDR(19/20) 总是新 access unit 的开始  
///   3. slice NAL (type < 32) 的 first_slice_segment_in_pic_flag=1 表示新 access unit
///      H.265 NAL header: 2 bytes, slice_segment_header 第一字节 bit 7 = first_slice
fn split_h265_access_units(data: &[u8]) -> Vec<Vec<u8>> {
    let nal_units = split_h265_nal_units(data);
    if nal_units.is_empty() {
        return Vec::new();
    }

    let mut access_units: Vec<Vec<u8>> = Vec::new();
    let mut current_au: Vec<u8> = Vec::new();

    for nal in &nal_units {
        let nal_t = nal_type(nal);
        // 判断是否是新 access unit 的开始
        let is_new_au_start = if nal_t == 32 || nal_t == 33 || nal_t == 34 {
            // VPS/SPS/PPS — 如果当前 AU 非空，先保存
            true
        } else if nal_t == 19 || nal_t == 20 {
            // IDR — 总是新 AU
            true
        } else if nal_t < 32 {
            // slice NAL — 检查 first_slice_segment_in_pic_flag
            // NAL header = 2 bytes, 第3字节 bit 7 = first_slice_segment_in_pic_flag
            nal.len() >= 3 && (nal[2] & 0x80) != 0
        } else {
            // SEI 等其他类型 — 归入当前 AU
            false
        };

        if is_new_au_start && !current_au.is_empty() {
            access_units.push(std::mem::take(&mut current_au));
        }

        // 将 NAL + start code 加入当前 AU (用 4 字节 start code)
        current_au.extend_from_slice(&[0, 0, 0, 1]);
        current_au.extend_from_slice(nal);
    }

    if !current_au.is_empty() {
        access_units.push(current_au);
    }

    access_units
}

/// 按 start code 分割 H.265 NAL units (Annex B 格式)
/// 同时支持 4 字节 (0x00000001) 和 3 字节 (0x000001) start code
fn split_h265_nal_units(data: &[u8]) -> Vec<Vec<u8>> {
    let mut units = Vec::new();
    let mut start = None;
    let mut i = 0;

    while i + 3 <= data.len() {
        // 快速跳过: 只有前两字节都为 0 才可能是 start code
        if data[i] == 0 && data[i + 1] == 0 {
            // 4 字节 start code: 0x00000001
            if i + 4 <= data.len() && data[i + 2] == 0 && data[i + 3] == 1 {
                if let Some(s) = start {
                    units.push(data[s..i].to_vec());
                }
                start = Some(i + 4);
                i += 4;
                continue;
            }
            // 3 字节 start code: 0x000001
            if data[i + 2] == 1 {
                if let Some(s) = start {
                    units.push(data[s..i].to_vec());
                }
                start = Some(i + 3);
                i += 3;
                continue;
            }
        }
        i += 1;
    }

    // 最后一个 NAL unit
    if let Some(s) = start {
        if data.len() > s {
            units.push(data[s..].to_vec());
        }
    }

    if units.is_empty() && !data.is_empty() {
        // 没有 start code, 整个文件当作一帧
        units.push(data.to_vec());
    }

    units
}

/// 提取 H.265 NAL unit type
/// H.265 NAL header: 2 bytes
///   byte0: forbidden_zero_bit(1) | nal_unit_type(6) | nuh_layer_id_high(1)
///   nal_unit_type = (byte0 >> 1) & 0x3F
/// 常见类型: 32=VPS, 33=SPS, 34=PPS, 19=IDR_W_RADL, 20=IDR_N_LP, 1=TRAIL_R
fn nal_type(data: &[u8]) -> u8 {
    if data.is_empty() {
        return 0;
    }
    (data[0] >> 1) & 0x3F
}

/// 从 access unit (含 start code 的字节流) 中提取第一个 NAL unit 的内容
/// AU 格式: [00 00 00 01] [NAL data...] [00 00 00 01] [NAL data...]
/// 返回第一个 NAL data (不含 start code)
fn first_nal_in_au(au: &[u8]) -> Option<&[u8]> {
    // 跳过 start code (3 或 4 字节)
    let start = if au.len() >= 4 && au[0..4] == [0, 0, 0, 1] {
        4
    } else if au.len() >= 3 && au[0..3] == [0, 0, 1] {
        3
    } else {
        return None;
    };

    // 找下一个 start code
    let nal_end = (start..au.len().saturating_sub(3))
        .find(|&i| {
            (au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 1)
                || (i + 3 < au.len() && au[i] == 0 && au[i + 1] == 0 && au[i + 2] == 0 && au[i + 3] == 1)
        })
        .unwrap_or(au.len());

    Some(&au[start..nal_end])
}
