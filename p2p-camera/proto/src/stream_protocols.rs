//! libp2p-stream 协议名称常量 (所有模块共用)

// StreamProtocol 定义在 libp2p-swarm 中
use libp2p_swarm::StreamProtocol;

/// 主码流协议: 高清 H.264/H.265 NAL units 封装在 MediaPacket 中
pub const VIDEO_MAIN_PROTOCOL: StreamProtocol = StreamProtocol::new("/p2p-camera/video/main/1.0.0");

/// 子码流协议: 标清 H.264/H.265 NAL units (低码率, 省带宽)
pub const VIDEO_SUB_PROTOCOL: StreamProtocol = StreamProtocol::new("/p2p-camera/video/sub/1.0.0");

/// 第三码流协议: 中清 H.264/H.265 NAL units
pub const VIDEO_THIRD_PROTOCOL: StreamProtocol = StreamProtocol::new("/p2p-camera/video/third/1.0.0");

/// 音频流协议: PCM/AAC 封装在 MediaPacket 中
pub const AUDIO_PROTOCOL: StreamProtocol = StreamProtocol::new("/p2p-camera/audio/1.0.0");
