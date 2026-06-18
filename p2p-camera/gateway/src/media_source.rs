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
}

impl FileVideoSource {
    /// 从 H.265 裸流文件加载所有 NAL units
    /// 简化: 按 start code (0x00000001) 分帧
    pub fn from_file(data: Vec<u8>) -> Self {
        let nal_units = split_h265_nal_units(&data);
        tracing::info!("Loaded {} H.265 NAL units from file", nal_units.len());

        Self {
            frame_interval: Duration::from_millis(40),
            nal_units,
            current: 0,
        }
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
    pub fn spawn(self, sender: Sender<MediaPacket>) -> thread::JoinHandle<()> {
        thread::spawn(move || {
            let mut this = self;
            let start = Instant::now();
            let mut frame_count: u64 = 0;

            loop {
                let data = this.next_frame();
                if data.is_empty() {
                    thread::sleep(this.frame_interval);
                    continue;
                }

                let timestamp_ms = start.elapsed().as_millis() as u64;
                // 简化: 每隔 50 帧认为是一个关键帧
                let is_keyframe = frame_count % 50 == 0;

                let packet = MediaPacket::video(timestamp_ms, is_keyframe, Bytes::from(data));
                if sender.send(packet).is_err() {
                    break; // 接收端已关闭
                }

                frame_count += 1;
                thread::sleep(this.frame_interval);
            }
        })
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

/// 简单按 start code 分帧 (仅用于原型)
fn split_h265_nal_units(data: &[u8]) -> Vec<Vec<u8>> {
    let start_code: &[u8] = &[0, 0, 0, 1];
    let mut units = Vec::new();
    let mut start = None;

    for i in 0..data.len().saturating_sub(3) {
        if &data[i..i + 4] == start_code {
            if let Some(s) = start {
                if i > s {
                    units.push(data[s..i].to_vec());
                }
            }
            start = Some(i + 4);
        }
    }

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
