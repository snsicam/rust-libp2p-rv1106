//! 控制通道请求处理
//!
//! DeviceCam 侧接收 Viewer 的控制请求，分发到对应的处理函数，
//! 通过 RkVideoSource 的 FFI 接口直接调用 RK SDK API 执行参数操作。

use anyhow::Result;
use libp2p::PeerId;
use proto::control::{
    ControlRequest, ControlResponse, EncoderConfig, ImageConfig, SystemConfigSet,
    read_frame, write_frame,
};
use tracing;

/// 处理一条控制流的生命周期
pub async fn handle_control_stream(peer_id: PeerId, mut stream: libp2p::swarm::Stream) {
    tracing::info!("[ControlHandler] Handling control stream from {peer_id}");

    loop {
        // 读取一帧请求
        let payload = match read_frame(&mut stream).await {
            Ok(p) => p,
            Err(e) => {
                // EOF 或连接断开
                tracing::info!("[ControlHandler] Control stream closed for {peer_id}: {e}");
                break;
            }
        };

        // 反序列化请求
        let req: ControlRequest = match serde_json::from_slice(&payload) {
            Ok(r) => r,
            Err(e) => {
                tracing::warn!("[ControlHandler] Invalid control request from {peer_id}: {e}");
                let resp = ControlResponse::err("invalid json");
                let _ = send_response(&mut stream, &resp).await;
                continue;
            }
        };

        tracing::debug!("[ControlHandler] Request from {peer_id}: {:?}", req);

        // 处理请求
        let resp = handle_request(req).await;

        // 发送响应
        if let Err(e) = send_response(&mut stream, &resp).await {
            tracing::warn!("[ControlHandler] Failed to send response to {peer_id}: {e}");
            break;
        }
    }

    tracing::info!("[ControlHandler] Control stream ended for {peer_id}");
}

/// 分发控制请求到对应的处理函数
async fn handle_request(req: ControlRequest) -> ControlResponse {
    match req {
        // ---- 编码参数 ----
        ControlRequest::GetEncoderConfig { stream } => get_encoder_config(&stream),
        ControlRequest::SetEncoderConfig { stream, config } => set_encoder_config(&stream, &config).await,

        // ---- 图像参数 ----
        ControlRequest::GetImageConfig { cam_id } => get_image_config(cam_id),
        ControlRequest::SetImageConfig { cam_id, config } => set_image_config(cam_id, &config),

        // ---- 系统参数 ----
        ControlRequest::GetSystemConfig => get_system_config(),
        ControlRequest::SetSystemConfig { config } => set_system_config(&config),
        ControlRequest::SystemReboot => system_reboot(),
        ControlRequest::FactoryReset => factory_reset(),
    }
}

/// 发送控制响应
async fn send_response(stream: &mut libp2p::swarm::Stream, resp: &ControlResponse) -> Result<()> {
    let json = serde_json::to_vec(resp)?;
    write_frame(stream, &json).await
}

// ============================================================
// 编码参数
// ============================================================

/// 码流名称 → RK SDK 通道 ID
fn stream_name_to_id(stream: &str) -> Option<u32> {
    match stream {
        "main" => Some(0),
        "sub" => Some(1),
        "third" => Some(2),
        _ => None,
    }
}

fn get_encoder_config(stream: &str) -> ControlResponse {
    tracing::info!("[ControlHandler] >>> GetEncoderConfig stream={stream}");
    let chn_id = match stream_name_to_id(stream) {
        Some(id) => id,
        None => {
            tracing::warn!("[ControlHandler] GetEncoderConfig invalid stream name: {stream}");
            return ControlResponse::err("invalid stream name");
        }
    };

    #[cfg(feature = "rv1106")]
    {
        match crate::rk_video_source::get_encoder_config(chn_id) {
            Some(config) => {
                tracing::info!(
                    "[ControlHandler] <<< GetEncoderConfig response stream={stream}: {} {}x{} fps={}/{} bitrate={}kbps gop={} rc={}/{} gop_mode={} h264_profile={} smart={} rotation={}",
                    config.output_data_type, config.width, config.height,
                    config.dst_frame_rate_num, config.dst_frame_rate_den, config.max_rate,
                    config.gop, config.rc_mode, config.rc_quality, config.gop_mode,
                    config.h264_profile, config.smart, config.rotation
                );
                ControlResponse {
                    ok: true,
                    error: None,
                    encoder_config: Some(config),
                    image_config: None,
                    system_config: None,
                }
            }
            None => ControlResponse::err("stream not enabled"),
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        let _ = chn_id;
        ControlResponse::err("not available on this platform")
    }
}

async fn set_encoder_config(stream: &str, config: &EncoderConfig) -> ControlResponse {
    tracing::info!(
        "[ControlHandler] >>> SetEncoderConfig stream={stream}: {} {}x{} fps={}/{} bitrate={}kbps gop={} rc={}/{} gop_mode={} h264_profile={} smart={} rotation={}",
        config.output_data_type, config.width, config.height,
        config.dst_frame_rate_num, config.dst_frame_rate_den, config.max_rate,
        config.gop, config.rc_mode, config.rc_quality, config.gop_mode,
        config.h264_profile, config.smart, config.rotation
    );
    let chn_id = match stream_name_to_id(stream) {
        Some(id) => id,
        None => return ControlResponse::err("invalid stream name"),
    };

    // 参数校验
    if let Err(e) = validate_encoder_config(config) {
        tracing::warn!("[ControlHandler] SetEncoderConfig validation failed: {e}");
        return ControlResponse::err(&e);
    }

    #[cfg(feature = "rv1106")]
    {
        match crate::rk_video_source::set_encoder_config(chn_id, config).await {
            Ok(()) => {
                tracing::info!("[ControlHandler] <<< SetEncoderConfig applied stream={stream}");
                ControlResponse::ok()
            }
            Err(e) => {
                tracing::error!("[ControlHandler] SetEncoderConfig failed stream={stream}: {e}");
                ControlResponse::err(&e.to_string())
            }
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        let _ = chn_id;
        ControlResponse::err("not available on this platform")
    }
}

fn validate_encoder_config(config: &EncoderConfig) -> Result<(), String> {
    if config.width < 320 || config.width > 4096 {
        return Err(format!("width {} out of range 320-4096", config.width));
    }
    if config.height < 240 || config.height > 2160 {
        return Err(format!("height {} out of range 240-2160", config.height));
    }
    if config.max_rate == 0 || config.max_rate > 16384 {
        return Err(format!("max_rate {} out of range 1-16384", config.max_rate));
    }
    if config.dst_frame_rate_num == 0 || config.dst_frame_rate_num > 30 {
        return Err(format!("dst_frame_rate_num {} out of range 1-30", config.dst_frame_rate_num));
    }
    if config.dst_frame_rate_den == 0 {
        return Err("dst_frame_rate_den must be positive".to_string());
    }
    if config.gop == 0 || config.gop > 400 {
        return Err(format!("gop {} out of range 1-400", config.gop));
    }
    match config.output_data_type.as_str() {
        "H.264" | "H.265" => {}
        _ => return Err(format!("invalid output_data_type: {}", config.output_data_type)),
    }
    match config.rc_mode.as_str() {
        "CBR" | "VBR" => {}
        _ => return Err(format!("invalid rc_mode: {}", config.rc_mode)),
    }
    match config.rc_quality.as_str() {
        "lowest" | "lower" | "low" | "medium" | "high" | "higher" | "highest" => {}
        _ => return Err(format!("invalid rc_quality: {}", config.rc_quality)),
    }
    match config.gop_mode.as_str() {
        "normalP" | "smartP" => {}
        _ => return Err(format!("invalid gop_mode: {}", config.gop_mode)),
    }
    match config.h264_profile.as_str() {
        "high" | "main" | "baseline" => {}
        _ => return Err(format!("invalid h264_profile: {}", config.h264_profile)),
    }
    match config.smart.as_str() {
        "open" | "close" => {}
        _ => return Err(format!("invalid smart: {}", config.smart)),
    }
    match config.rotation {
        0 | 90 | 180 | 270 => {}
        _ => return Err(format!("invalid rotation: {}", config.rotation)),
    }
    Ok(())
}

// ============================================================
// 图像参数
// ============================================================

fn get_image_config(cam_id: u32) -> ControlResponse {
    #[cfg(feature = "rv1106")]
    {
        match crate::rk_video_source::get_image_config(cam_id) {
            Some(config) => ControlResponse {
                ok: true,
                error: None,
                encoder_config: None,
                image_config: Some(config),
                system_config: None,
            },
            None => ControlResponse::err("cam_id not available"),
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        let _ = cam_id;
        ControlResponse::err("not available on this platform")
    }
}

fn set_image_config(cam_id: u32, config: &ImageConfig) -> ControlResponse {
    #[cfg(feature = "rv1106")]
    {
        match crate::rk_video_source::set_image_config(cam_id, config) {
            Ok(()) => ControlResponse::ok(),
            Err(e) => ControlResponse::err(&e.to_string()),
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        let _ = (cam_id, config);
        ControlResponse::err("not available on this platform")
    }
}

// ============================================================
// 系统参数
// ============================================================

fn get_system_config() -> ControlResponse {
    #[cfg(feature = "rv1106")]
    {
        match crate::rk_video_source::get_system_config() {
            Some(config) => ControlResponse {
                ok: true,
                error: None,
                encoder_config: None,
                image_config: None,
                system_config: Some(config),
            },
            None => ControlResponse::err("failed to read system config"),
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        ControlResponse::err("not available on this platform")
    }
}

fn set_system_config(config: &SystemConfigSet) -> ControlResponse {
    #[cfg(feature = "rv1106")]
    {
        match crate::rk_video_source::set_system_config(config) {
            Ok(()) => ControlResponse::ok(),
            Err(e) => ControlResponse::err(&e.to_string()),
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        let _ = config;
        ControlResponse::err("not available on this platform")
    }
}

fn system_reboot() -> ControlResponse {
    #[cfg(feature = "rv1106")]
    {
        match crate::rk_video_source::system_reboot() {
            Ok(()) => ControlResponse::ok(),
            Err(e) => ControlResponse::err(&e.to_string()),
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        ControlResponse::err("not available on this platform")
    }
}

fn factory_reset() -> ControlResponse {
    #[cfg(feature = "rv1106")]
    {
        match crate::rk_video_source::factory_reset() {
            Ok(()) => ControlResponse::ok(),
            Err(e) => ControlResponse::err(&e.to_string()),
        }
    }

    #[cfg(not(feature = "rv1106"))]
    {
        ControlResponse::err("not available on this platform")
    }
}
