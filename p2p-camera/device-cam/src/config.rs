//! DeviceCam 配置文件支持
//!
//! 配置文件格式: TOML
//! 优先级: 命令行参数 > 配置文件 > 默认值
//! 如果配置文件不存在，自动生成带默认值的配置文件
//!
//! 多 Relay 支持:
//!   新格式: relays = ["/ip4/.../p2p/PeerId1", "/ip4/.../p2p/PeerId2"]
//!   旧格式: relay = "/ip4/.../p2p/PeerId" (向后兼容, 解析时合并到 relays)
//!   同时存在时 relays 优先
//!
//! 三码流配置 (rv1106 模式):
//!   [video.main]    主码流 (高清, 如 2304x1296)
//!   [video.sub]     子码流 (标清, 如 704x576, 低码率)
//!   [video.third]   第三码流 (中清, 如 960x540)
//!
//! 音频采集+编码配置 (rv1106 模式):
//!   [audio]    声卡设备、采样率、通道数、音量、编码(G711A/MP2)

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// 单个码流的编码配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamConfig {
    /// 是否启用该码流
    #[serde(default = "default_stream_enabled")]
    pub enabled: bool,
    /// 编码格式: "H265" | "H264"
    #[serde(default = "default_codec")]
    pub codec: String,
    /// 视频宽度
    #[serde(default = "default_width")]
    pub width: u32,
    /// 视频高度
    #[serde(default = "default_height")]
    pub height: u32,
    /// 源帧率分子
    #[serde(default = "default_fps_num")]
    pub src_frame_rate_num: u32,
    /// 源帧率分母
    #[serde(default = "default_fps_den")]
    pub src_frame_rate_den: u32,
    /// 目标帧率分子
    #[serde(default = "default_fps_num")]
    pub dst_frame_rate_num: u32,
    /// 目标帧率分母
    #[serde(default = "default_fps_den")]
    pub dst_frame_rate_den: u32,
    /// 码率控制模式: "CBR" | "VBR"
    #[serde(default = "default_rc_mode")]
    pub rc_mode: String,
    /// RC 质量: "highest" | "higher" | "high" | "medium" | "low" | "lower" | "lowest"
    #[serde(default = "default_rc_quality")]
    pub rc_quality: String,
    /// 码率 (kbps)
    pub bitrate_kbps: u32,
    /// GOP (I 帧间隔帧数)
    #[serde(default = "default_gop")]
    pub gop: u32,
    /// GOP 模式: "normalP" | "smartP"
    #[serde(default = "default_gop_mode")]
    pub gop_mode: String,
    /// SmartP 虚拟 I 帧间隔 (仅 smartP 时使用)
    #[serde(default = "default_viridrlen")]
    pub smartp_viridrlen: u32,
    /// H.264 Profile: "baseline" | "main" | "high" (仅 codec=H264 时生效)
    #[serde(default = "default_h264_profile")]
    pub h264_profile: String,
    /// 码流缓冲区数
    #[serde(default = "default_buf_cnt")]
    pub stream_buf_cnt: u32,
    /// 画面镜像: "none" | "horizontal" | "vertical" | "both"
    #[serde(default = "default_mirror")]
    pub mirror: String,
}

fn default_stream_enabled() -> bool { true }
fn default_codec() -> String { "H265".to_string() }
fn default_width() -> u32 { 800 }
fn default_height() -> u32 { 600 }
fn default_fps_num() -> u32 { 25 }
fn default_fps_den() -> u32 { 1 }
fn default_rc_mode() -> String { "CBR".to_string() }
fn default_rc_quality() -> String { "high".to_string() }
fn default_gop() -> u32 { 50 }
fn default_gop_mode() -> String { "normalP".to_string() }
fn default_viridrlen() -> u32 { 25 }
fn default_h264_profile() -> String { "high".to_string() }
fn default_buf_cnt() -> u32 { 2 }
fn default_mirror() -> String { "none".to_string() }

/// 视频码流组配置
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VideoConfig {
    #[serde(default)]
    pub main: StreamConfig,
    #[serde(default)]
    pub sub: StreamConfig,
    #[serde(default)]
    pub third: StreamConfig,
}

/// LCD 显示配置 (VO → MIPI 接口)
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LcdConfig {
    /// 是否启用 LCD 显示
    #[serde(default = "default_lcd_enabled")]
    pub enabled: bool,
    /// LCD 宽度 (0 = 自动检测)
    #[serde(default = "default_lcd_width")]
    pub width: u32,
    /// LCD 高度 (0 = 自动检测)
    #[serde(default = "default_lcd_height")]
    pub height: u32,
}

fn default_lcd_enabled() -> bool { false }
fn default_lcd_width() -> u32 { 800 }
fn default_lcd_height() -> u32 { 480 }

/// rknn 目标检测配置 (复用 lcd 的 selfpath 帧源, 结果经 bbox_shm 给 LVGL)
///
/// 注意: rknn 不独占 VI 通道, 它复用 lcd_preview 的中性 selfpath 帧源
/// (LCD 或 rknn 任一需要即开启), 见 rknn_infer.c / lcd_preview.c。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RknnConfig {
    /// 是否启用 rknn 目标检测
    #[serde(default = "default_rknn_enabled")]
    pub enabled: bool,
    /// .rknn 模型文件路径 (板端绝对路径)
    #[serde(default = "default_rknn_model_path")]
    pub model_path: String,
}

fn default_rknn_enabled() -> bool { false }
fn default_rknn_model_path() -> String { "/oem/yolov5.rknn".to_string() }

impl Default for RknnConfig {
    fn default() -> Self {
        Self {
            enabled: default_rknn_enabled(),
            model_path: default_rknn_model_path(),
        }
    }
}

impl Default for LcdConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            width: 800,
            height: 480,
        }
    }
}

impl Default for StreamConfig {
    fn default() -> Self {
        Self {
            enabled: default_stream_enabled(),
            codec: default_codec(),
            width: default_width(),
            height: default_height(),
            src_frame_rate_num: default_fps_num(),
            src_frame_rate_den: default_fps_den(),
            dst_frame_rate_num: default_fps_num(),
            dst_frame_rate_den: default_fps_den(),
            rc_mode: default_rc_mode(),
            rc_quality: default_rc_quality(),
            bitrate_kbps: 0, // 由具体配置指定
            gop: default_gop(),
            gop_mode: default_gop_mode(),
            smartp_viridrlen: default_viridrlen(),
            h264_profile: default_h264_profile(),
            stream_buf_cnt: default_buf_cnt(),
            mirror: default_mirror(),
        }
    }
}

impl Default for VideoConfig {
    fn default() -> Self {
        Self {
            main: StreamConfig {
                width: 2304,
                height: 1296,
                bitrate_kbps: 2048,
                ..StreamConfig::default()
            },
            sub: StreamConfig {
                width: 704,
                height: 576,
                src_frame_rate_num: 30,
                dst_frame_rate_num: 30,
                bitrate_kbps: 512,
                ..StreamConfig::default()
            },
            third: StreamConfig {
                width: 960,
                height: 540,
                bitrate_kbps: 1024,
                ..StreamConfig::default()
            },
        }
    }
}

/// 音频采集+编码配置 (rv1106 模式)
///
/// 管线: AI(PCM采集) → [AENC(HW编码)] → MediaPacket → libp2p stream
/// `encode_type = "PCM"` 时跳过编码，直接发送原始 PCM。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AudioConfig {
    /// 是否启用音频采集
    #[serde(default = "default_audio_enabled")]
    pub enabled: bool,
    /// 声卡设备名 (ALSA), 如 "hw:0,0"
    #[serde(default = "default_audio_card_name")]
    pub card_name: String,
    /// 采样率 (Hz): 8000 | 16000 | 24000 | 32000 | 44100 | 48000 等
    #[serde(default = "default_audio_sample_rate")]
    pub sample_rate: u32,
    /// 通道数 (1=mono, 2=stereo, 在采集端强制 mono)
    #[serde(default = "default_audio_channels")]
    pub channels: u32,
    /// 每帧采样点数 (如 1024 @ 16kHz ≈ 64ms/frame)
    #[serde(default = "default_audio_frame_size")]
    pub frame_size: u32,
    /// 声卡格式: "S16" (16-bit signed) | "U8"
    #[serde(default = "default_audio_format")]
    pub format: String,
    /// 硬件音量 (0-100)
    #[serde(default = "default_audio_volume")]
    pub volume: u32,
    /// 编码类型: "PCM" (不编码) | "G711A" | "G711U" | "MP2"
    #[serde(default = "default_audio_encode_type")]
    pub encode_type: String,
    /// 编码码率 (bps, 仅编码模式): 16000 (G711A@8k) | 64000 (MP2)
    #[serde(default = "default_audio_bit_rate")]
    pub bit_rate: u32,
    /// 是否启用 VQE (语音质量增强)
    #[serde(default = "default_audio_enable_vqe")]
    pub enable_vqe: bool,
    /// VQE 配置文件路径
    #[serde(default = "default_audio_vqe_cfg")]
    pub vqe_cfg: String,
}

fn default_audio_enabled() -> bool { false }
fn default_audio_card_name() -> String { "hw:0,0".to_string() }
fn default_audio_sample_rate() -> u32 { 16000 }
fn default_audio_channels() -> u32 { 1 }
fn default_audio_frame_size() -> u32 { 1024 }
fn default_audio_format() -> String { "S16".to_string() }
fn default_audio_volume() -> u32 { 50 }
fn default_audio_encode_type() -> String { "PCM".to_string() }
fn default_audio_bit_rate() -> u32 { 16000 }
fn default_audio_enable_vqe() -> bool { false }
fn default_audio_vqe_cfg() -> String { "/oem/usr/share/vqefiles/config_aivqe.json".to_string() }

impl Default for AudioConfig {
    fn default() -> Self {
        Self {
            enabled: default_audio_enabled(),
            card_name: default_audio_card_name(),
            sample_rate: default_audio_sample_rate(),
            channels: default_audio_channels(),
            frame_size: default_audio_frame_size(),
            format: default_audio_format(),
            volume: default_audio_volume(),
            encode_type: default_audio_encode_type(),
            bit_rate: default_audio_bit_rate(),
            enable_vqe: default_audio_enable_vqe(),
            vqe_cfg: default_audio_vqe_cfg(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    /// 多 Relay 地址列表 (新格式优先)
    #[serde(default)]
    pub relays: Vec<String>,
    /// 单 Relay 地址 (旧格式, 向后兼容, 解析时合并到 relays)
    #[serde(default)]
    pub relay: String,
    /// 是否启用 mDNS 局域网发现 (默认 true)
    #[serde(default = "default_enable_mdns")]
    pub enable_mdns: bool,

    /// 是否启用 DCUtR 直连打洞 (默认 true)。
    /// 4G/CGNAT 等入站 UDP 被屏蔽的网络下打洞必然失败，且打洞握手会挤占
    /// 中继视频流的写入带宽，导致 SLOW write / 帧丢弃 / 卡顿。此类网络应设为 false，
    /// 仅走中继电路 (relay circuit) 即可稳定传输。
    #[serde(default = "default_enable_dcutr")]
    pub enable_dcutr: bool,

    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_key_file")]
    pub key_file: PathBuf,
    /// 音频采集配置
    #[serde(default)]
    pub audio: AudioConfig,
    #[serde(default)]
    pub udp_port: Option<u16>,

    /// 视频三码流配置 (rv1106 模式)
    #[serde(default)]
    pub video: VideoConfig,

    /// LCD 显示配置 (VO → MIPI 接口)
    #[serde(default)]
    pub lcd: LcdConfig,

    /// rknn 目标检测配置 (复用 lcd selfpath 帧源)
    #[serde(default)]
    pub rknn: RknnConfig,

    /// sensor 原生输出帧率 (摄像头模组实际产出率, 如 30)。
    /// 注意: 这是 VI/编码器的输入源帧率, 与各码流的目标帧率(dst_frame_rate)不同。
    /// 帧率控制逻辑(对标 rkipc isp.0.adjustment:fps): VENC 的 u32SrcFrameRate 必须等于此值,
    /// 编码器才能按目标帧率正确丢帧; 若误用某码流的配置 fps, 会导致 ratio=1 不丢帧、实测跑满原生帧率。
    #[serde(default = "default_sensor_frame_rate")]
    pub sensor_frame_rate: u32,

    // 文件源 (非 rv1106)
    #[serde(default)]
    pub video_file: Option<PathBuf>,
}

fn default_mode() -> String { "listen".to_string() }
fn default_key_file() -> PathBuf { PathBuf::from("device-cam.key") }
fn default_enable_mdns() -> bool { true }
fn default_enable_dcutr() -> bool { true }
fn default_sensor_frame_rate() -> u32 { 30 }

impl Default for Config {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            relay: String::new(),
            enable_mdns: default_enable_mdns(),
            enable_dcutr: default_enable_dcutr(),
            mode: default_mode(),
            key_file: default_key_file(),
            audio: AudioConfig::default(),
            udp_port: None,
            video: VideoConfig::default(),
            lcd: LcdConfig::default(),
            rknn: RknnConfig::default(),
            sensor_frame_rate: default_sensor_frame_rate(),
            video_file: None,
        }
    }
}

impl Config {
    /// 从文件加载配置，文件不存在则生成默认配置文件并返回默认配置
    pub fn load(path: &PathBuf) -> anyhow::Result<Self> {
        if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| anyhow::anyhow!("Failed to read config file {}: {e}", path.display()))?;
            let mut config: Config = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config file {}: {e}", path.display()))?;
            config.resolve_relays();
            println!("[DeviceCam] Loaded config from {}", path.display());
            Ok(config)
        } else {
            let config = Config::default();
            config.save(path)?;
            println!("[DeviceCam] Generated default config file: {}", path.display());
            Ok(config)
        }
    }

    /// 解析 Relay 地址列表: 处理旧格式 relay 与新格式 relays 的兼容
    ///
    /// 规则:
    /// - 如果 relays 非空, 忽略 relay (新格式优先)
    /// - 如果 relays 为空且 relay 非空, 将 relay 加入 relays (旧格式兼容)
    fn resolve_relays(&mut self) {
        if self.relays.is_empty() && !self.relay.is_empty() {
            self.relays.push(self.relay.clone());
        }
    }

    /// 保存配置到文件
    pub fn save(&self, path: &PathBuf) -> anyhow::Result<()> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let content = toml::to_string_pretty(self)
            .map_err(|e| anyhow::anyhow!("Failed to serialize config: {e}"))?;
        std::fs::write(path, content)?;
        Ok(())
    }

    /// 用命令行参数覆盖配置
    pub fn apply_cli_overrides(&mut self, cli: &CliOverrides) {
        if !cli.relays.is_empty() {
            self.relays = cli.relays.clone();
        }
        if let Some(enable_mdns) = cli.enable_mdns {
            self.enable_mdns = enable_mdns;
        }
        if let Some(ref mode) = cli.mode { self.mode = mode.clone(); }
        if let Some(ref key_file) = cli.key_file { self.key_file = key_file.clone(); }
        if cli.enable_audio { self.audio.enabled = true; }
        if let Some(udp_port) = cli.udp_port { self.udp_port = Some(udp_port); }
        if let Some(ref video_file) = cli.video_file { self.video_file = Some(video_file.clone()); }
        #[cfg(feature = "rv1106")]
        if let Some(ref rknn_model) = cli.rknn_model {
            self.rknn.enabled = true;
            self.rknn.model_path = rknn_model.to_string_lossy().to_string();
        }
    }

    /// 返回所有启用码流的配置列表
    #[allow(dead_code)]
    pub fn enabled_streams(&self) -> Vec<(StreamType, &StreamConfig)> {
        let mut streams = Vec::new();
        if self.video.main.enabled {
            streams.push((StreamType::Main, &self.video.main));
        }
        if self.video.sub.enabled {
            streams.push((StreamType::Sub, &self.video.sub));
        }
        if self.video.third.enabled {
            streams.push((StreamType::Third, &self.video.third));
        }
        streams
    }
}

/// 码流类型枚举
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
#[allow(dead_code)]
pub enum StreamType {
    Main = 0,
    Sub = 1,
    Third = 2,
}

#[allow(dead_code)]
impl StreamType {
    pub fn from_chn_id(chn_id: u8) -> Option<Self> {
        match chn_id {
            0 => Some(StreamType::Main),
            1 => Some(StreamType::Sub),
            2 => Some(StreamType::Third),
            _ => None,
        }
    }

    pub fn name(&self) -> &'static str {
        match self {
            StreamType::Main => "main",
            StreamType::Sub => "sub",
            StreamType::Third => "third",
        }
    }
}

/// 命令行覆盖参数 (所有字段都是 Option，仅覆盖配置文件中的值)
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub relays: Vec<String>,
    pub enable_mdns: Option<bool>,
    pub mode: Option<String>,
    pub key_file: Option<PathBuf>,
    pub enable_audio: bool,
    pub udp_port: Option<u16>,
    pub video_file: Option<PathBuf>,
    /// rknn 模型路径 (指定即启用 rknn 目标检测, 复用 lcd selfpath 帧源)
    /// 仅 rv1106 特性下存在 (对应 Opt 的 --rknn-model 参数)
    #[cfg(feature = "rv1106")]
    pub rknn_model: Option<PathBuf>,
}
