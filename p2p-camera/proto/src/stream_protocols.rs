//! libp2p-stream 协议名称常量 (所有模块共用)

use libp2p_stream::StreamProtocol;

/// 视频流协议: H.265 NAL units 封装在 MediaPacket 中
pub const VIDEO_PROTOCOL: StreamProtocol = StreamProtocol::new("/p2p-camera/video/1.0.0");

/// 音频流协议: PCM/AAC 封装在 MediaPacket 中
pub const AUDIO_PROTOCOL: StreamProtocol = StreamProtocol::new("/p2p-camera/audio/1.0.0");
