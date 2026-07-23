use anyhow::{anyhow, Result};
use serde::{Deserialize, Serialize};

// ============================================================
// ControlRequest — Viewer → DeviceCam
// ============================================================

/// 控制请求消息
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum ControlRequest {
    // ---- 编码参数 ----
    GetEncoderConfig {
        stream: String, // "main" | "sub" | "third"
    },
    SetEncoderConfig {
        stream: String,
        config: EncoderConfig,
    },

    // ---- 图像参数 ----
    GetImageConfig {
        cam_id: u32,
    },
    SetImageConfig {
        cam_id: u32,
        #[serde(flatten)]
        config: ImageConfig,
    },

    // ---- 系统参数 ----
    GetSystemConfig,
    SetSystemConfig {
        config: SystemConfigSet,
    },
    SystemReboot,
    FactoryReset,
}

// ============================================================
// ControlResponse — DeviceCam → Viewer
// ============================================================

/// 控制响应消息
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlResponse {
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub encoder_config: Option<EncoderConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_config: Option<ImageConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_config: Option<SystemConfig>,
}

impl ControlResponse {
    pub fn ok() -> Self {
        Self {
            ok: true,
            error: None,
            encoder_config: None,
            image_config: None,
            system_config: None,
        }
    }

    pub fn err(msg: &str) -> Self {
        Self {
            ok: false,
            error: Some(msg.to_string()),
            encoder_config: None,
            image_config: None,
            system_config: None,
        }
    }
}

// ============================================================
// EncoderConfig — 编码器参数 (参考 rkipc video.{stream_id})
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct EncoderConfig {
    pub output_data_type: String, // "H.264" | "H.265"
    pub width: u32,
    pub height: u32,
    pub rc_mode: String,   // "CBR" | "VBR"
    pub rc_quality: String, // "lowest" | "low" | "medium" | "high" | "highest"
    pub gop: u32,
    pub gop_mode: String,  // "normalP" | "smartP"
    pub max_rate: u32,     // kbps
    pub dst_frame_rate_num: u32,
    pub dst_frame_rate_den: u32,
    pub h264_profile: String, // "high" | "main" | "baseline"
    pub smart: String,     // "open" | "close"
    pub rotation: u32,     // 0, 90, 180, 270
}

// ============================================================
// ImageConfig — 图像参数 (参考 rkipc isp.{cam_id}.*)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub adjustment: Option<ImageAdjustment>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exposure: Option<ImageExposure>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub night_to_day: Option<ImageNightToDay>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub white_balance: Option<ImageWhiteBalance>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enhancement: Option<ImageEnhancement>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub video_adjustment: Option<ImageVideoAdjustment>,
}

/// 图像调整参数 (isp.{cam_id}.adjustment)
/// 字段全部为 Option, 支持部分更新 (只传需要修改的字段)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageAdjustment {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub contrast: Option<i32>,     // 0-100
    #[serde(skip_serializing_if = "Option::is_none")]
    pub brightness: Option<i32>,   // 0-100
    #[serde(skip_serializing_if = "Option::is_none")]
    pub saturation: Option<i32>,   // 0-100
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sharpness: Option<i32>,    // 0-100
    #[serde(skip_serializing_if = "Option::is_none")]
    pub hue: Option<i32>,          // 0-100
}

/// 曝光参数 (isp.{cam_id}.exposure)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageExposure {
    pub exposure_mode: String, // "auto" | "manual"
    pub gain_mode: String,     // "auto" | "manual"
    pub exposure_time: i32,    // 手动曝光时间
    pub exposure_gain: i32,    // 手动增益
}

/// 日夜切换参数 (isp.{cam_id}.night_to_day)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageNightToDay {
    pub night_to_day_mode: String, // "day" | "night" | "auto"
    pub switch_sensitivity: i32,   // 日夜切换灵敏度
    pub ir_cut_status: i32,        // IR-CUT 状态
}

/// 白平衡参数 (isp.{cam_id}.white_balance)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageWhiteBalance {
    pub wb_mode: String,     // "auto" | "manual"
    pub r_gain: i32,         // 红色增益
    pub b_gain: i32,         // 蓝色增益
    pub auto_wb_speed: i32,  // 自动白平衡速度
}

/// 图像增强参数 (isp.{cam_id}.enhancement)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageEnhancement {
    pub denoise_level: i32,       // 2D 降噪等级
    pub spatial_denoise_level: i32, // 3D 降噪等级
    pub dehaze_level: i32,        // 去雾等级
    pub fec_level: i32,           // FEC 等级
    pub hdr_mode: String,         // "close" | "auto" | "manual"
    pub hdr_level: i32,           // HDR 等级
    pub distortion_correction: String, // "open" | "close"
    pub wdr_level: i32,           // WDR 等级
}

/// 视频调整参数 (isp.{cam_id}.video_adjustment)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ImageVideoAdjustment {
    pub flip: String,   // "open" | "close"
    pub mirror: String, // "open" | "close"
}

// ============================================================
// SystemConfig — 系统参数 (参考 rkipc system.device_info)
// ============================================================

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfig {
    pub device_name: String,
    pub telecontrol_id: String,
    pub model: String,
    pub serial_number: String,
    pub firmware_version: String,
    pub manufacturer: String,
}

/// 系统参数设置 (仅可写字段)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemConfigSet {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub telecontrol_id: Option<String>,
}

// ============================================================
// 帧编解码 — [4B len (big-endian u32)][JSON payload]
// ============================================================

const LEN_SIZE: usize = 4;

/// 编码为帧: [4B len][JSON]
pub fn encode_request(req: &ControlRequest) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(req)?;
    let len = json.len() as u32;
    let mut buf = Vec::with_capacity(LEN_SIZE + json.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// 编码响应为帧
pub fn encode_response(resp: &ControlResponse) -> Result<Vec<u8>> {
    let json = serde_json::to_vec(resp)?;
    let len = json.len() as u32;
    let mut buf = Vec::with_capacity(LEN_SIZE + json.len());
    buf.extend_from_slice(&len.to_be_bytes());
    buf.extend_from_slice(&json);
    Ok(buf)
}

/// 从缓冲区尝试解码请求，返回 (请求, 消耗字节数)
pub fn try_decode_request(buf: &[u8]) -> Result<Option<(ControlRequest, usize)>> {
    if buf.len() < LEN_SIZE {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < LEN_SIZE + len {
        return Ok(None);
    }
    let req: ControlRequest = serde_json::from_slice(&buf[LEN_SIZE..LEN_SIZE + len])?;
    Ok(Some((req, LEN_SIZE + len)))
}

/// 从缓冲区尝试解码响应
pub fn try_decode_response(buf: &[u8]) -> Result<Option<(ControlResponse, usize)>> {
    if buf.len() < LEN_SIZE {
        return Ok(None);
    }
    let len = u32::from_be_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if buf.len() < LEN_SIZE + len {
        return Ok(None);
    }
    let resp: ControlResponse = serde_json::from_slice(&buf[LEN_SIZE..LEN_SIZE + len])?;
    Ok(Some((resp, LEN_SIZE + len)))
}

/// 读取流中的一帧 (异步)
pub async fn read_frame<T: futures::AsyncRead + Unpin>(
    stream: &mut T,
) -> Result<Vec<u8>> {
    use futures::AsyncReadExt;

    let mut len_buf = [0u8; LEN_SIZE];
    stream.read_exact(&mut len_buf).await?;
    let len = u32::from_be_bytes(len_buf) as usize;
    if len > 1024 * 1024 {
        return Err(anyhow!("control frame too large: {} bytes", len));
    }
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await?;
    Ok(payload)
}

/// 写入一帧到流 (异步)
pub async fn write_frame<T: futures::AsyncWrite + Unpin>(
    stream: &mut T,
    payload: &[u8],
) -> Result<()> {
    use futures::AsyncWriteExt;

    let len = payload.len() as u32;
    stream.write_all(&len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await?;
    Ok(())
}
