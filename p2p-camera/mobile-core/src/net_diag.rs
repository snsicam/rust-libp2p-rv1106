//! NAT 类型诊断模块
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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
    RelayCircuit,
    QuicDirect,
    LanDirect,
    TcpDirect,
    Disconnected,
}

impl ConnectionType {
    pub fn description(&self) -> &'static str {
        match self {
            Self::RelayCircuit => "Relay Circuit (forwarded via relay server)",
            Self::QuicDirect => "QUIC Direct (hole punched, no relay)",
            Self::LanDirect => "LAN Direct (same subnet, no relay)",
            Self::TcpDirect => "TCP Direct",
            Self::Disconnected => "Disconnected",
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

#[derive(Debug, Clone)]
pub struct ConnectionQuality {
    pub connection_type: ConnectionType,
    pub active_connections: usize,
    pub direct_upgraded: bool,
    pub last_dcutr_result: Option<Result<(), String>>,
}

impl Default for ConnectionQuality {
    fn default() -> Self {
        Self {
            connection_type: ConnectionType::Disconnected,
            active_connections: 0,
            direct_upgraded: false,
            last_dcutr_result: None,
        }
    }
}

pub struct NatDiagnostic {
    observed_history: Vec<Multiaddr>,
    local_quic_port: u16,
    local_ips: Vec<Ipv4Addr>,
}

impl NatDiagnostic {
    pub fn new(local_quic_port: u16, local_ips: Vec<Ipv4Addr>) -> Self {
        Self {
            observed_history: Vec::new(),
            local_quic_port,
            local_ips,
        }
    }

    pub fn record_observed(&mut self, addr: &Multiaddr) {
        self.observed_history.push(addr.clone());
    }

    pub fn observed_history_is_empty(&self) -> bool {
        self.observed_history.is_empty()
    }

    pub fn local_ips(&self) -> &[Ipv4Addr] {
        &self.local_ips
    }

    pub fn diagnose(&self) -> NatDiagnosis {
        let is_4g = self.local_ips.iter().any(|ip| is_4g_network(*ip));

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
                let nat_type = NatType::FullCone;
                return NatDiagnosis {
                    nat_type,
                    observed_addresses: self.observed_history.clone(),
                    local_port: self.local_quic_port,
                    evidence: format!("Observed port {} matches local port {} - no NAT or 1:1 NAT", port, self.local_quic_port),
                    dcutr_feasible: true,
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
            let nat_type = NatType::PortRestrictedCone;
            NatDiagnosis {
                nat_type,
                observed_addresses: self.observed_history.clone(),
                local_port: self.local_quic_port,
                evidence: format!("Observed port {} consistent across {} observations - Cone NAT", valid_ports[0], valid_ports.len()),
                dcutr_feasible: true,
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
/// - 100.64.0.0/10: RFC 6598 CGNAT 保留段
/// - 非 RFC 1918 私有地址且非公网 IP: 可能运营商内网
fn is_4g_network(ip: Ipv4Addr) -> bool {
    // 192.168.174.0/24 — Android WiFi 热点/USB 共享典型网段
    if ip.octets()[0] == 192 && ip.octets()[1] == 168 && ip.octets()[2] == 174 {
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
    // 排除所有保留地址段后即为公网 IP
    // RFC 1918 私有地址
    if ip.is_private() {
        return false;
    }
    // 回环地址
    if ip.is_loopback() {
        return false;
    }
    // 链路本地 169.254.0.0/16
    if ip.is_link_local() {
        return false;
    }
    // RFC 6598 CGNAT 100.64.0.0/10
    if (ip.octets()[0] == 100) && (ip.octets()[1] >= 64 && ip.octets()[1] <= 127) {
        return false;
    }
    // IETF 协议分配 192.0.0.0/24
    if ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 0 {
        return false;
    }
    // TEST-NET-1 192.0.2.0/24
    if ip.octets()[0] == 192 && ip.octets()[1] == 0 && ip.octets()[2] == 2 {
        return false;
    }
    // TEST-NET-2 198.51.100.0/24
    if ip.octets()[0] == 198 && ip.octets()[1] == 51 && ip.octets()[2] == 100 {
        return false;
    }
    // TEST-NET-3 203.0.113.0/24
    if ip.octets()[0] == 203 && ip.octets()[1] == 0 && ip.octets()[2] == 113 {
        return false;
    }
    // 组播 224.0.0.0/4
    if ip.is_multicast() {
        return false;
    }
    // 保留 240.0.0.0/4
    if ip.octets()[0] >= 240 {
        return false;
    }
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
