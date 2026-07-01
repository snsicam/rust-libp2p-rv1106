//! DeviceCam 配置文件支持
//!
//! 配置文件格式: TOML
//! 优先级: 命令行参数 > 配置文件 > 默认值
//! 如果配置文件不存在，自动生成带默认值的配置文件

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    pub relay: String,
    #[serde(default = "default_mode")]
    pub mode: String,
    #[serde(default = "default_key_file")]
    pub key_file: PathBuf,
    #[serde(default)]
    pub enable_audio: bool,
    #[serde(default)]
    pub udp_port: Option<u16>,

    // 视频参数 (rv1106 和文件源通用)
    #[serde(default = "default_width")]
    pub width: u32,
    #[serde(default = "default_height")]
    pub height: u32,
    #[serde(default = "default_fps")]
    pub fps: u32,
    #[serde(default = "default_bitrate")]
    pub bitrate: u32,

    // 文件源 (非 rv1106)
    #[serde(default)]
    pub video_file: Option<PathBuf>,
}

fn default_mode() -> String { "listen".to_string() }
fn default_key_file() -> PathBuf { PathBuf::from("device-cam.key") }
fn default_width() -> u32 { 800 }
fn default_height() -> u32 { 600 }
fn default_fps() -> u32 { 25 }
fn default_bitrate() -> u32 { 1024 }

impl Default for Config {
    fn default() -> Self {
        Self {
            relay: String::new(),
            mode: default_mode(),
            key_file: default_key_file(),
            enable_audio: false,
            udp_port: None,
            width: default_width(),
            height: default_height(),
            fps: default_fps(),
            bitrate: default_bitrate(),
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
            let config: Config = toml::from_str(&content)
                .map_err(|e| anyhow::anyhow!("Failed to parse config file {}: {e}", path.display()))?;
            println!("[DeviceCam] Loaded config from {}", path.display());
            Ok(config)
        } else {
            let config = Config::default();
            config.save(path)?;
            println!("[DeviceCam] Generated default config file: {}", path.display());
            Ok(config)
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
        if let Some(ref relay) = cli.relay { self.relay = relay.clone(); }
        if let Some(ref mode) = cli.mode { self.mode = mode.clone(); }
        if let Some(ref key_file) = cli.key_file { self.key_file = key_file.clone(); }
        if cli.enable_audio { self.enable_audio = true; }
        if let Some(udp_port) = cli.udp_port { self.udp_port = Some(udp_port); }
        if let Some(width) = cli.width { self.width = width; }
        if let Some(height) = cli.height { self.height = height; }
        if let Some(fps) = cli.fps { self.fps = fps; }
        if let Some(bitrate) = cli.bitrate { self.bitrate = bitrate; }
        if let Some(ref video_file) = cli.video_file { self.video_file = Some(video_file.clone()); }
    }
}

/// 命令行覆盖参数 (所有字段都是 Option，仅覆盖配置文件中的值)
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub relay: Option<String>,
    pub mode: Option<String>,
    pub key_file: Option<PathBuf>,
    pub enable_audio: bool,
    pub udp_port: Option<u16>,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub fps: Option<u32>,
    pub bitrate: Option<u32>,
    pub video_file: Option<PathBuf>,
}
