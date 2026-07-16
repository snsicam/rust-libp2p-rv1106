//! NAT 类型诊断模块 (device-cam 版本)
//!
//! 通过分析 Relay Server Identify 观测地址与本地监听端口的映射关系，
//! 判断 NAT 类型，评估 DCUtR 穿透可行性。
//! 支持 4G/CGNAT 网络启发式检测和策略建议。

use std::net::Ipv4Addr;

use libp2p::core::multiaddr::{Multiaddr, Protocol};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NatType {
    FullCone,
    RestrictedCone,
    PortRestrictedCone,
    Symmetric,
    Unknown,
}

impl NatType {
    pub fn dcutr_feasible(&self) -> bool {
        matches!(self, Self::FullCone | Self::RestrictedCone | Self::PortRestrictedCone)
    }

    pub fn description(&self) -> &'static str {
        match self {
            Self::FullCone => "Full Cone NAT - DCUtR feasible",
            Self::RestrictedCone => "Restricted Cone NAT - DCUtR feasible",
            Self::PortRestrictedCone => "Port Restricted Cone NAT - DCUtR feasible",
            Self::Symmetric => "Symmetric NAT - DCUtR NOT feasible, will use Relay Circuit",
            Self::Unknown => "NAT type unknown - insufficient observation data",
        }
    }

    pub fn short_name(&self) -> &'static str {
        match self {
            Self::FullCone => "FullCone",
            Self::RestrictedCone => "RestrictedCone",
            Self::PortRestrictedCone => "PortRestrictedCone",
            Self::Symmetric => "Symmetric",
            Self::Unknown => "Unknown",
        }
    }
}

#[derive(Debug, Clone)]
pub struct NatDiagnosis {
    pub nat_type: NatType,
    pub observed_addresses: Vec<Multiaddr>,
    pub local_port: u16,
    pub evidence: String,
    pub dcutr_feasible: bool,
    pub is_4g: bool,
    pub dcutr_suggestion: String,
}

/// DCUtR 尝试前的预测结果
#[derive(Debug, Clone)]
pub struct DcutrPrediction {
    pub likely_success: bool,
    pub is_4g: bool,
    pub nat_type: NatType,
    pub reason: String,
}

/// 连接策略决策
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionStrategy {
    /// 尝试 DCUtR 打洞
    Dcutr,
    /// 跳过 DCUtR，直接使用 Relay Circuit
    SkipDcutr,
}

impl ConnectionStrategy {
    pub fn name(&self) -> &'static str {
        match self {
            Self::Dcutr => "DCUtR",
            Self::SkipDcutr => "SkipDcutr",
        }
    }
}

pub struct NatDiagnostic {
    observed_history: Vec<Multiaddr>,
    local_quic_port: u16,
    local_ips: Vec<Ipv4Addr>,
    /// 用户手动指定为 4G 网络（配置文件或命令行参数）
    /// 4G 模块的 IP 可能是 RFC1918 私有地址（如 10.x.x.x），无法通过 IP 启发式检测
    force_4g: bool,
}

impl NatDiagnostic {
    pub fn new(local_quic_port: u16, local_ips: Vec<Ipv4Addr>) -> Self {
        Self {
            observed_history: Vec::new(),
            local_quic_port,
            local_ips,
            force_4g: false,
        }
    }

    /// 设置用户手动指定的 4G 网络标志
    pub fn set_force_4g(&mut self, force: bool) {
        self.force_4g = force;
    }

    pub fn record_observed(&mut self, addr: &Multiaddr) {
        // 只保留最近一次 Relay 连接的观测地址
        // 当 Relay 重连后，本地 QUIC 端口可能变化，混合不同连接的观测会导致误判 Symmetric NAT
        let observed_port = extract_udp_port(addr);
        let current_port_matches = observed_port
            .map(|p| p == self.local_quic_port || self.observed_history.iter().any(|a| extract_udp_port(a) == Some(p)))
            .unwrap_or(false);

        if !current_port_matches && !self.observed_history.is_empty() {
            // 新观测端口与历史不一致，可能是新连接（端口变了），清除旧观测
            tracing::debug!(
                "[NAT] New observed port {:?} differs from history, clearing old observations (likely relay reconnect with new local port)",
                observed_port
            );
            self.observed_history.clear();
        }
        self.observed_history.push(addr.clone());
    }

    pub fn observed_history_is_empty(&self) -> bool {
        self.observed_history.is_empty()
    }

    /// 判断是否应跳过 DCUtR 打洞
    ///
    /// 当本端为 Symmetric NAT 或 4G/CGNAT 网络时，DCUtR 打洞必然失败，
    /// 应跳过以节省约 17 秒的超时等待。
    pub fn should_skip_dcutr(&self) -> bool {
        let is_4g = self.force_4g || self.local_ips.iter().any(|ip| is_4g_network(*ip));
        let diag = self.diagnose();
        
        // Symmetric NAT 或 4G/CGNAT → 跳过 DCUtR
        diag.nat_type == NatType::Symmetric || is_4g
    }

    /// 获取连接策略决策及原因
    ///
    /// 返回 (策略, 原因描述)
    pub fn connection_strategy(&self) -> (ConnectionStrategy, String) {
        let is_4g = self.force_4g || self.local_ips.iter().any(|ip| is_4g_network(*ip));
        let diag = self.diagnose();

        if diag.nat_type == NatType::Symmetric {
            return (ConnectionStrategy::SkipDcutr, format!(
                "Symmetric NAT: DCUtR will fail, port mapping is unpredictable. Saved ~17s timeout waiting."
            ));
        }

        if is_4g {
            return (ConnectionStrategy::SkipDcutr, format!(
                "4G/CGNAT: DCUtR will fail, carrier-grade NAT blocks inbound UDP. Saved ~17s timeout waiting."
            ));
        }

        if diag.nat_type.dcutr_feasible() {
            return (ConnectionStrategy::Dcutr, format!(
                "{} NAT: DCUtR should succeed", diag.nat_type.short_name()
            ));
        }

        // Unknown → 保守策略，尝试 DCUtR
        (ConnectionStrategy::Dcutr, "NAT type unknown: DCUtR will be attempted".to_string())
    }

    /// 在 DCUtR 尝试前输出 NAT 上下文预测
    pub fn dcutr_prediction(&self) -> DcutrPrediction {
        let is_4g = self.force_4g || self.local_ips.iter().any(|ip| is_4g_network(*ip));
        let diag = self.diagnose();

        let (likely_success, reason) = if is_4g && !diag.nat_type.dcutr_feasible() {
            (false, format!(
                "4G/CGNAT + {} NAT: DCUtR hole-punching will likely fail. \
                 CGNAT does not allow inbound UDP from external addresses. \
                 Relay circuit will be used.",
                 diag.nat_type.short_name()
            ))
        } else if is_4g {
            (false, format!(
                "4G/CGNAT detected ({}): DCUtR may fail because CGNAT typically blocks inbound UDP. \
                 Success depends on remote peer's NAT type (Cone NAT on broadband may work). \
                 Relay circuit will be used as fallback.",
                 diag.nat_type.short_name()
            ))
        } else if !diag.nat_type.dcutr_feasible() {
            (false, format!(
                "{} NAT: DCUtR hole-punching will not succeed. \
                 Relay circuit will be used.",
                 diag.nat_type.short_name()
            ))
        } else if diag.nat_type == NatType::Unknown {
            (true, "NAT type unknown: DCUtR will be attempted, success depends on NAT compatibility of both peers.".to_string())
        } else {
            (true, format!(
                "{} NAT: DCUtR hole-punching should succeed. \
                 If it fails, check firewall settings or port forwarding.",
                 diag.nat_type.short_name()
            ))
        };

        DcutrPrediction {
            likely_success,
            is_4g,
            nat_type: diag.nat_type,
            reason,
        }
    }

    pub fn diagnose(&self) -> NatDiagnosis {
        let is_4g = self.force_4g || self.local_ips.iter().any(|ip| is_4g_network(*ip));

        if self.observed_history.is_empty() {
            return NatDiagnosis {
                nat_type: NatType::Unknown,
                observed_addresses: Vec::new(),
                local_port: self.local_quic_port,
                evidence: "No Identify observations received yet".to_string(),
                dcutr_feasible: false,
                is_4g,
                dcutr_suggestion: generate_suggestion(NatType::Unknown, is_4g),
            };
        }

        let observed_ports: Vec<Option<u16>> = self.observed_history
            .iter()
            .map(extract_udp_port)
            .collect();

        let valid_ports: Vec<u16> = observed_ports.iter().filter_map(|&p| p).collect();

        if valid_ports.is_empty() {
            return NatDiagnosis {
                nat_type: NatType::Unknown,
                observed_addresses: self.observed_history.clone(),
                local_port: self.local_quic_port,
                evidence: "No UDP ports found in observed addresses (TCP-only observations)".to_string(),
                dcutr_feasible: false,
                is_4g,
                dcutr_suggestion: generate_suggestion(NatType::Unknown, is_4g),
            };
        }

        if valid_ports.len() == 1 {
            let port = valid_ports[0];
            if port == self.local_quic_port && self.local_quic_port != 0 {
                // 4G/CGNAT 场景：单次观测端口一致不能判定为 Full Cone
                // CGNAT 对不同目标可能分配不同端口（Symmetric 行为），
                // 只有通过多个不同 Relay 观测端口一致才能确认是 Cone NAT
                let nat_type = if is_4g {
                    NatType::Unknown
                } else {
                    NatType::FullCone
                };
                let evidence = if is_4g {
                    format!("Observed port {} matches local port {}, but 4G/CGNAT detected - cannot confirm Full Cone NAT from single observation (CGNAT may assign different ports to different destinations)", port, self.local_quic_port)
                } else {
                    format!("Observed port {} matches local port {} - no NAT or 1:1 NAT", port, self.local_quic_port)
                };
                return NatDiagnosis {
                    nat_type,
                    observed_addresses: self.observed_history.clone(),
                    local_port: self.local_quic_port,
                    evidence,
                    dcutr_feasible: !is_4g,
                    is_4g,
                    dcutr_suggestion: generate_suggestion(nat_type, is_4g),
                };
            }
            let nat_type = NatType::Unknown;
            return NatDiagnosis {
                nat_type,
                observed_addresses: self.observed_history.clone(),
                local_port: self.local_quic_port,
                evidence: format!("Only 1 observation with port {} - need more data to determine NAT type", port),
                dcutr_feasible: true,
                is_4g,
                dcutr_suggestion: generate_suggestion(nat_type, is_4g),
            };
        }

        let all_same = valid_ports.iter().all(|&p| p == valid_ports[0]);
        if all_same {
            // 4G/CGNAT 场景：即使对同一 Relay 的端口映射一致，
            // 也不能保证对 DCUtR 对端的映射一致（CGNAT 可能对不同目标分配不同端口）
            let nat_type = if is_4g {
                NatType::Unknown
            } else {
                NatType::PortRestrictedCone
            };
            let evidence = if is_4g {
                format!("Observed port {} consistent across {} observations, but 4G/CGNAT detected - cannot confirm Cone NAT (CGNAT may assign different ports to different destinations)", valid_ports[0], valid_ports.len())
            } else {
                format!("Observed port {} consistent across {} observations - Cone NAT", valid_ports[0], valid_ports.len())
            };
            NatDiagnosis {
                nat_type,
                observed_addresses: self.observed_history.clone(),
                local_port: self.local_quic_port,
                evidence,
                dcutr_feasible: !is_4g,
                is_4g,
                dcutr_suggestion: generate_suggestion(nat_type, is_4g),
            }
        } else {
            let nat_type = NatType::Symmetric;
            let ports_str: Vec<String> = valid_ports.iter().map(|p| p.to_string()).collect();
            NatDiagnosis {
                nat_type,
                observed_addresses: self.observed_history.clone(),
                local_port: self.local_quic_port,
                evidence: format!("Observed ports vary: {} - Symmetric NAT", ports_str.join(", ")),
                dcutr_feasible: false,
                is_4g,
                dcutr_suggestion: generate_suggestion(nat_type, is_4g),
            }
        }
    }
}

/// 4G/CGNAT 网络启发式检测
///
/// 通过本地 IP 网段判断是否可能为 4G 网络：
/// - 192.168.174.0/24: Android WiFi 热点/USB 共享网络典型网段
/// - 192.168.133.0/24: iOS/Android 个人热点典型网段
/// - 192.168.43.0/24: Android WiFi 热点 (旧版) 典型网段
/// - 100.64.0.0/10: RFC 6598 CGNAT 保留段
/// - 非 RFC 1918 且非公网 IP: 运营商内网
pub fn is_4g_network(ip: Ipv4Addr) -> bool {
    // 192.168.174.0/24 — Android WiFi 热点/USB 共享典型网段
    if ip.octets()[0] == 192 && ip.octets()[1] == 168 && ip.octets()[2] == 174 {
        return true;
    }

    // 192.168.133.0/24 — iOS/Android 个人热点典型网段
    if ip.octets()[0] == 192 && ip.octets()[1] == 168 && ip.octets()[2] == 133 {
        return true;
    }

    // 192.168.43.0/24 — Android WiFi 热点 (旧版) 典型网段
    if ip.octets()[0] == 192 && ip.octets()[1] == 168 && ip.octets()[2] == 43 {
        return true;
    }

    // 100.64.0.0/10 — RFC 6598 CGNAT 保留段 (100.64.0.0 - 100.127.255.255)
    if (ip.octets()[0] == 100) && (ip.octets()[1] >= 64 && ip.octets()[1] <= 127) {
        return true;
    }

    // 非 RFC 1918 私有地址、非回环、非未指定、非公网 IP
    // 这类地址通常是运营商内网或 VPN 分配的地址
    // RFC 1918: 10.0.0.0/8, 172.16.0.0/12, 192.168.0.0/16
    if !ip.is_private() && !ip.is_loopback() && !ip.is_unspecified() && !is_public_ip(ip) {
        return true;
    }

    false
}

/// 判断 IP 是否为公网可达地址
fn is_public_ip(ip: Ipv4Addr) -> bool {
    if ip.is_private() { return false; }
    if ip.is_loopback() { return false; }
    if ip.is_link_local() { return false; }
    if (ip.octets()[0] == 100) && (ip.octets()[1] >= 64 && ip.octets()[1] <= 127) { return false; }
    if ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 0 { return false; }
    if ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 2 { return false; }
    if ip.octets()[0] == 198 && ip.octets()[1] == 51 && ip.octets()[2] == 100 { return false; }
    if ip.octets()[0] == 203 && ip.octets()[1] == 0 && ip.octets()[2] == 113 { return false; }
    if ip.is_multicast() { return false; }
    if ip.octets()[0] >= 240 { return false; }
    true
}

/// 根据 NAT 类型和 4G 标记生成 DCUtR 策略建议
fn generate_suggestion(nat_type: NatType, is_4g: bool) -> String {
    match (nat_type, is_4g) {
        (NatType::Symmetric, true) => {
            "4G/CGNAT with Symmetric NAT: DCUtR hole-punching will not succeed. \
             Placing device-cam on broadband (Cone NAT) network increases DCUtR success rate. \
             Relay circuit will be used as fallback.".to_string()
        }
        (NatType::Symmetric, false) => {
            "Symmetric NAT: DCUtR hole-punching will not succeed, relay circuit is the only option. \
             Consider: 1) Configure port forwarding on router, 2) Use --udp-port to fix local port".to_string()
        }
        (NatType::FullCone | NatType::RestrictedCone | NatType::PortRestrictedCone, true) => {
            "4G/CGNAT detected: DCUtR may succeed if remote peer is on broadband (Cone NAT). \
             Placing device-cam on broadband network increases DCUtR success rate.".to_string()
        }
        (NatType::FullCone | NatType::RestrictedCone | NatType::PortRestrictedCone, false) => {
            "Cone NAT: DCUtR hole-punching should succeed. \
             If it fails, check firewall settings or port forwarding configuration.".to_string()
        }
        (NatType::Unknown, true) => {
            "4G/CGNAT detected with unknown NAT type: DCUtR will be attempted but success rate is uncertain. \
             Placing device-cam on broadband network increases DCUtR success rate.".to_string()
        }
        (NatType::Unknown, false) => {
            "NAT type unknown: DCUtR will be attempted, success depends on NAT compatibility of both peers.".to_string()
        }
    }
}

fn extract_udp_port(addr: &Multiaddr) -> Option<u16> {
    for p in addr.iter() {
        if let Protocol::Udp(port) = p {
            return Some(port);
        }
    }
    None
}
