//! 音视频分离 Jitter Buffer (用于移动端接收侧)
//!
//! 设计目标:
//! - 视频 target delay 100ms (缓冲 2-3 帧)
//! - 音频 target delay 50ms
//! - 音视频同步: 视频不领先音频超过 50ms

use proto::media_packet::{MediaPacket, MediaTrack};
use std::collections::VecDeque;
use std::time::{Duration, Instant};

/// 音视频 Jitter Buffer
pub struct AvJitterBuffer {
    video: TrackBuffer,
    audio: TrackBuffer,
    sync_threshold: Duration,
}

struct TrackBuffer {
    frames: VecDeque<QueuedPacket>,
    target_delay: Duration,
    max_size: usize,
    last_played_ts: Option<u64>,
}

struct QueuedPacket {
    packet: MediaPacket,
    _arrived_at: Instant,
}

impl AvJitterBuffer {
    /// 创建新的 Jitter Buffer
    /// video_delay: 视频缓冲目标 (建议 100ms)
    /// audio_delay: 音频缓冲目标 (建议 50ms)
    pub fn new(video_delay: Duration, audio_delay: Duration) -> Self {
        Self {
            video: TrackBuffer::new(video_delay, 60),   // 视频最多 60 帧
            audio: TrackBuffer::new(audio_delay, 200),   // 音频最多 200 帧
            sync_threshold: Duration::from_millis(50),
        }
    }

    /// 推入一个媒体包
    pub fn push(&mut self, packet: MediaPacket) {
        match packet.track {
            MediaTrack::Video => self.video.push(packet),
            MediaTrack::Audio => self.audio.push(packet),
        }
    }

    /// 获取下一视频帧 (考虑音视频同步)
    pub fn next_video(&mut self) -> Option<MediaPacket> {
        let video = self.video.pop_next()?;

        // 音视频同步: 视频不能领先音频太多
        if let Some(audio_ts) = self.audio.last_played_ts {
            let diff = video.timestamp_ms as i64 - audio_ts as i64;
            if diff > self.sync_threshold.as_millis() as i64 {
                // 视频太快, 放回去等待音频跟上
                self.video.push_front(video);
                return None;
            }
        }

        Some(video)
    }

    /// 获取下一音频帧
    pub fn next_audio(&mut self) -> Option<MediaPacket> {
        self.audio.pop_next()
    }

    /// 视频帧缓冲时长
    pub fn video_buffered(&self) -> Duration {
        self.video.buffered_duration()
    }

    /// 音频帧缓冲时长
    pub fn audio_buffered(&self) -> Duration {
        self.audio.buffered_duration()
    }

    /// 是否准备好开始播放 (视频侧)
    pub fn video_ready(&self) -> bool {
        self.video.last_played_ts.is_some()
            || self.video.buffered_duration() >= self.video.target_delay
    }
}

impl TrackBuffer {
    fn new(target_delay: Duration, max_size: usize) -> Self {
        Self {
            frames: VecDeque::new(),
            target_delay,
            max_size,
            last_played_ts: None,
        }
    }

    fn push(&mut self, packet: MediaPacket) {
        if self.frames.len() >= self.max_size {
            self.frames.pop_front(); // 丢弃最旧帧
        }
        self.frames.push_back(QueuedPacket {
            packet,
            _arrived_at: Instant::now(),
        });
    }

    fn push_front(&mut self, packet: MediaPacket) {
        self.frames.push_front(QueuedPacket {
            packet,
            _arrived_at: Instant::now(),
        });
    }

    fn pop_next(&mut self) -> Option<MediaPacket> {
        if self.frames.is_empty() {
            return None;
        }

        // 首帧等待足够缓冲
        if self.last_played_ts.is_none()
            && self.buffered_duration() < self.target_delay
        {
            return None;
        }

        let queued = self.frames.pop_front()?;
        self.last_played_ts = Some(queued.packet.timestamp_ms);

        // 缓冲过多则跳帧追赶
        while self.buffered_duration() > self.target_delay * 2 {
            self.frames.pop_front();
        }

        Some(queued.packet)
    }

    fn buffered_duration(&self) -> Duration {
        if self.frames.len() < 2 {
            return Duration::ZERO;
        }
        let first = self.frames.front().unwrap().packet.timestamp_ms;
        let last = self.frames.back().unwrap().packet.timestamp_ms;
        Duration::from_millis(last - first)
    }
}
