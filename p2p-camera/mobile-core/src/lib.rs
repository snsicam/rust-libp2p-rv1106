//! P2P Camera Viewer — 移动端 Rust 核心库
//!
//! 提供 C FFI 接口供 Android (JNI) / iOS (C FFI) 调用。
//! 当前阶段: 提供 Rust API, C FFI 在后续阶段添加。

pub mod jitter_buffer;
pub mod viewer;

// Re-export key types
pub use proto::media_packet::{MediaPacket, MediaTrack};
pub use viewer::P2pViewer;
