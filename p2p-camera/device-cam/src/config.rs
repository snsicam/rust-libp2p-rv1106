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

    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_key_file")]
    pub key_file: PathBuf,
    #[serde(default)]
    pub enable_audio: bool,
    #[serde(default)]
    pub udp_port: Option<u16>,

    /// 视频三码流配置 (rv1106 模式)
    #[serde(default)]
    pub video: VideoConfig,

    // 文件源 (非 rv1106)
    #[serde(default)]
    pub video_file: Option<PathBuf>,
}

fn default_mode() -> String { "listen".to_string() }
fn default_key_file() -> PathBuf { PathBuf::from("device-cam.key") }
fn default_enable_mdns() -> bool { true }

impl Default for Config {
    fn default() -> Self {
        Self {
            relays: Vec::new(),
            relay: String::new(),
            enable_mdns: default_enable_mdns(),
            mode: default_mode(),
            key_file: default_key_file(),
            enable_audio: false,
            udp_port: None,
            video: VideoConfig::default(),
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
        if cli.enable_audio { self.enable_audio = true; }
        if let Some(udp_port) = cli.udp_port { self.udp_port = Some(udp_port); }
        if let Some(ref video_file) = cli.video_file { self.video_file = Some(video_file.clone()); }
    }

    /// 返回所有启用码流的配置列表
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
pub enum StreamType {
    Main = 0,
    Sub = 1,
    Third = 2,
}

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
}
