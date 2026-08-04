//! Relay Server 配置文件支持
//!
//! 配置文件格式: TOML
//! 优先级: 命令行参数 > 配置文件 > 默认值
//! 如果配置文件不存在，自动生成带默认值的配置文件

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Config {
    #[serde(default)]
    pub use_ipv6: bool,
    #[serde(default = "default_key_file")]
    pub key_file: PathBuf,
    #[serde(default = "default_port")]
    pub port: u16,
    /// 公网地址列表, 支持同时填 IPv4 和 IPv6, 运行时自动识别族别并
    /// 分别通告 `/ip4/...` 或 `/ip6/...` 外部地址。
    /// 例: public_ips = ["101.35.90.171", "2408:8000::1"]
    #[serde(default)]
    pub public_ips: Vec<String>,
}

fn default_key_file() -> PathBuf { PathBuf::from("relay-server.key") }
fn default_port() -> u16 { 4001 }

impl Default for Config {
    fn default() -> Self {
        Self {
            use_ipv6: false,
            key_file: default_key_file(),
            port: default_port(),
            public_ips: Vec::new(),
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
            println!("[Relay] Loaded config from {}", path.display());
            Ok(config)
        } else {
            let config = Config::default();
            config.save(path)?;
            println!("[Relay] Generated default config file: {}", path.display());
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
        if cli.use_ipv6 { self.use_ipv6 = true; }
        if let Some(ref key_file) = cli.key_file { self.key_file = key_file.clone(); }
        if let Some(port) = cli.port { self.port = port; }
        // 命令行给了 --public-ip 就整体替换配置文件里的列表 (可重复传多个)
        if !cli.public_ips.is_empty() { self.public_ips = cli.public_ips.clone(); }
    }
}

/// 命令行覆盖参数
#[derive(Debug, Default)]
pub struct CliOverrides {
    pub use_ipv6: bool,
    pub key_file: Option<PathBuf>,
    pub port: Option<u16>,
    pub public_ips: Vec<String>,
}
