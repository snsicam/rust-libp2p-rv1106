//! P2P Camera Viewer — 移动端 Rust 核心库
//!
//! 提供 C FFI 接口供 Android (JNI) / iOS (C FFI) 调用。

pub mod jitter_buffer;
pub mod net_diag;
pub mod viewer;

// JNI 桥接 (Android)
// 编译时自动启用，不影响 iOS/桌面平台
pub mod jni_bridge;

// Re-export key types
pub use net_diag::{ConnectionType, ConnectionQuality, NatDiagnosis, NatType};
pub use proto::media_packet::{MediaPacket, MediaTrack};
pub use viewer::{MediaPlayer, MediaPlayerEvent, ConnectOptions, is_nal_keyframe, IDR_WAIT_TIMEOUT};
