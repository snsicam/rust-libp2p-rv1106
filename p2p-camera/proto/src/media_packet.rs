//! 音视频统一媒体包协议 (所有模块共用)
//!
//! 包格式:
//! +--------+------------+-------+--------+--------+
//! |Track(1B)|Timestamp(8B)|Flags(1B)|Len(4B)|Data(N)|
//! +--------+------------+-------+--------+--------+
//!
//! Track:  0x01=Video(H.265 NAL), 0x02=Audio(PCM/AAC)
//!
//! Flags (Video):  bit 0: 0=IDR关键帧, 1=非关键帧
//! Flags (Audio):  bit 0-1: 0=PCM16LE, 1=AAC

use bytes::{Buf, BufMut, Bytes, BytesMut};

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

    /// 创建视频帧包
    pub fn video(timestamp_ms: u64, is_keyframe: bool, data: Bytes) -> Self {
        MediaPacket {
            track: MediaTrack::Video,
            timestamp_ms,
            flags: if is_keyframe { 0 } else { 1 },
            data,
        }
    }

    /// 创建 PCM 音频包
    pub fn audio_pcm(timestamp_ms: u64, data: Bytes) -> Self {
        MediaPacket {
            track: MediaTrack::Audio,
            timestamp_ms,
            flags: 0, // PCM16LE
            data,
        }
    }

    pub fn is_keyframe(&self) -> bool {
        self.track == MediaTrack::Video && (self.flags & 0x01) == 0
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
        let pkt = MediaPacket::video(12345, true, data);
        let encoded = pkt.encode();

        let mut buf = BytesMut::from(encoded.as_ref());
        let decoded = MediaPacket::try_decode(&mut buf).unwrap();

        assert_eq!(decoded.track, MediaTrack::Video);
        assert_eq!(decoded.timestamp_ms, 12345);
        assert!(decoded.is_keyframe());
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
