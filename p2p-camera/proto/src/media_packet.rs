//! 音视频统一媒体包协议 (所有模块共用)
//!
//! 包格式:
//! +--------+------------+-------+--------+--------+
//! |Track(1B)|Timestamp(8B)|Flags(1B)|Len(4B)|Data(N)|
//! +--------+------------+-------+--------+--------+
//!
//! Track:  0x01=Video(H.265 NAL), 0x02=Audio(PCM/AAC/G711)
//!
//! Flags: 音频包用低 2 位区分 codec: 0=PCM16LE, 1=AAC, 2=G711A, 3=G711U;
//!        视频包用 bit 2 (FLAG_VIDEO_KEYFRAME=0x04) 标记关键帧。
//!        关键帧由接收端字节扫描判定(见 viewer 的 is_nal_keyframe, H.265 IRAP 16-21 / H.264 IDR 5),
//!        cam 不计算也不传该标志(实测不可靠), 接收端扫描后写入 flags 的 bit 2 供下游复用,
//!        避免每帧重复字节扫描。JNI 桥可顺带转发该 bit 给原生 APP。

use bytes::{Buf, BufMut, Bytes, BytesMut};

/// 音频包 flags 低 2 位: 音频 codec 类型
pub const AUDIO_CODEC_PCM: u8 = 0;
pub const AUDIO_CODEC_AAC: u8 = 1;
pub const AUDIO_CODEC_G711A: u8 = 2;
pub const AUDIO_CODEC_G711U: u8 = 3;

/// 视频包 flags bit 2: 关键帧标志 (由接收端字节扫描判定, 非 cam wire 值)
pub const FLAG_VIDEO_KEYFRAME: u8 = 0x04;

#[derive(Debug, Clone, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaTrack {
    Video = 0x01,
    Audio = 0x02,
}

impl MediaTrack {
    pub fn from_u8(v: u8) -> Option<Self> {
        match v {
            0x01 => Some(MediaTrack::Video),
            0x02 => Some(MediaTrack::Audio),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct MediaPacket {
    pub track: MediaTrack,
    pub timestamp_ms: u64,
    pub flags: u8,
    pub data: Bytes,
}

impl MediaPacket {
    const HEADER_SIZE: usize = 1 + 8 + 1 + 4; // track + ts + flags + data_len

    /// 是否关键帧: 仅视频包有效。由接收端字节扫描 `is_nal_keyframe` 后写入 `flags` 的
    /// `FLAG_VIDEO_KEYFRAME` bit (cam 不计算该标志, 实测不可靠), 此处为唯一权威判定。
    /// 下游 (drain / media_viewer / JNI) 直接复用, 避免每帧重复字节扫描。
    pub fn is_keyframe(&self) -> bool {
        self.track == MediaTrack::Video && (self.flags & FLAG_VIDEO_KEYFRAME) != 0
    }

    /// 创建视频帧包
    /// 注意: 不接收 is_keyframe 参数 —— 关键帧由接收端字节扫描判定后写入 `flags` 的
    /// `FLAG_VIDEO_KEYFRAME` bit (cam 不计算也不传该标志, 实测不可靠), 下游用 `is_keyframe()` 读取。
    pub fn video(timestamp_ms: u64, data: Bytes) -> Self {
        MediaPacket {
            track: MediaTrack::Video,
            timestamp_ms,
            flags: 0, // 视频包关键帧 bit 由 viewer receive_frames 扫描后设置
            data,
        }
    }

    /// 创建 PCM 音频包
    pub fn audio_pcm(timestamp_ms: u64, data: Bytes) -> Self {
        MediaPacket {
            track: MediaTrack::Audio,
            timestamp_ms,
            flags: AUDIO_CODEC_PCM, // PCM16LE
            data,
        }
    }

    /// 创建 G.711A 音频包 (硬件编码)
    pub fn audio_g711a(timestamp_ms: u64, data: Bytes) -> Self {
        MediaPacket {
            track: MediaTrack::Audio,
            timestamp_ms,
            flags: AUDIO_CODEC_G711A, // G711A
            data,
        }
    }

    /// 创建 G.711U 音频包 (硬件编码)
    pub fn audio_g711u(timestamp_ms: u64, data: Bytes) -> Self {
        MediaPacket {
            track: MediaTrack::Audio,
            timestamp_ms,
            flags: AUDIO_CODEC_G711U, // G711U
            data,
        }
    }

    /// 编码为字节
    pub fn encode(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::HEADER_SIZE + self.data.len());

        buf.put_u8(self.track.clone() as u8);
        buf.put_u64(self.timestamp_ms);
        buf.put_u8(self.flags);
        buf.put_u32(self.data.len() as u32);
        buf.extend_from_slice(&self.data);

        buf.freeze()
    }

    /// 从 BytesMut 解码一个包，成功则消费数据
    pub fn try_decode(buf: &mut BytesMut) -> Option<Self> {
        if buf.len() < Self::HEADER_SIZE {
            return None;
        }

        let track = MediaTrack::from_u8(buf[0])?;
        let timestamp_ms = u64::from_be_bytes(buf[1..9].try_into().ok()?);
        let flags = buf[9];
        let data_len = u32::from_be_bytes(buf[10..14].try_into().ok()?) as usize;

        if buf.len() < Self::HEADER_SIZE + data_len {
            return None;
        }

        // 消费整个包
        buf.advance(Self::HEADER_SIZE);
        let data = buf.copy_to_bytes(data_len);

        Some(MediaPacket {
            track,
            timestamp_ms,
            flags,
            data,
        })
    }

    /// 查看下一个包的长度 (HEADER + data)，不消费数据
    pub fn peek_len(buf: &BytesMut) -> Option<usize> {
        if buf.len() < Self::HEADER_SIZE {
            return None;
        }
        let data_len = u32::from_be_bytes(buf[10..14].try_into().ok()?) as usize;
        if buf.len() < Self::HEADER_SIZE + data_len {
            return None;
        }
        Some(Self::HEADER_SIZE + data_len)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_video_packet_roundtrip() {
        let data = Bytes::from_static(&[0, 0, 0, 1, 0x65, 0x01, 0x02]); // fake NAL
        let pkt = MediaPacket::video(12345, data);
        let encoded = pkt.encode();

        let mut buf = BytesMut::from(encoded.as_ref());
        let decoded = MediaPacket::try_decode(&mut buf).unwrap();

        assert_eq!(decoded.track, MediaTrack::Video);
        assert_eq!(decoded.timestamp_ms, 12345);
        assert_eq!(decoded.flags, 0); // 视频包 flags 保留未用
        assert_eq!(decoded.data.len(), 7);
    }

    #[test]
    fn test_audio_packet_roundtrip() {
        let data = Bytes::from(vec![0u8; 320]); // 20ms PCM16LE @ 16kHz = 320 bytes
        let pkt = MediaPacket::audio_pcm(67890, data);
        let encoded = pkt.encode();

        let mut buf = BytesMut::from(encoded.as_ref());
        let decoded = MediaPacket::try_decode(&mut buf).unwrap();

        assert_eq!(decoded.track, MediaTrack::Audio);
        assert_eq!(decoded.timestamp_ms, 67890);
        assert_eq!(decoded.flags, 0); // PCM16LE
        assert_eq!(decoded.data.len(), 320);
    }
}
