//! NAT 类型诊断模块
//!
//! 通过分析 Relay Server Identify 观测地址与本地监听端口的映射关系，
//! 判断 NAT 类型，评估 DCUtR 穿透可行性。

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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionType {
    RelayCircuit,
    QuicDirect,
    TcpDirect,
    Disconnected,
}

impl ConnectionType {
    pub fn description(&self) -> &'static str {
        match self {
            Self::RelayCircuit => "Relay Circuit (forwarded via relay server)",
            Self::QuicDirect => "QUIC Direct (hole punched, no relay)",
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
}

impl NatDiagnostic {
    pub fn new(local_quic_port: u16) -> Self {
        Self {
            observed_history: Vec::new(),
            local_quic_port,
        }
    }

    pub fn record_observed(&mut self, addr: &Multiaddr) {
        self.observed_history.push(addr.clone());
    }

    pub fn observed_history_is_empty(&self) -> bool {
        self.observed_history.is_empty()
    }

    pub fn diagnose(&self) -> NatDiagnosis {
        if self.observed_history.is_empty() {
            return NatDiagnosis {
                nat_type: NatType::Unknown,
                observed_addresses: Vec::new(),
                local_port: self.local_quic_port,
                evidence: "No Identify observations received yet".to_string(),
                dcutr_feasible: false,
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
            };
        }

        if valid_ports.len() == 1 {
            let port = valid_ports[0];
            if port == self.local_quic_port && self.local_quic_port != 0 {
                return NatDiagnosis {
                    nat_type: NatType::FullCone,
                    observed_addresses: self.observed_history.clone(),
                    local_port: self.local_quic_port,
                    evidence: format!("Observed port {} matches local port {} - no NAT or 1:1 NAT", port, self.local_quic_port),
                    dcutr_feasible: true,
                };
            }
            return NatDiagnosis {
                nat_type: NatType::Unknown,
                observed_addresses: self.observed_history.clone(),
                local_port: self.local_quic_port,
                evidence: format!("Only 1 observation with port {} - need more data to determine NAT type", port),
                dcutr_feasible: true,
            };
        }

        let all_same = valid_ports.iter().all(|&p| p == valid_ports[0]);
        if all_same {
            NatDiagnosis {
                nat_type: NatType::PortRestrictedCone,
                observed_addresses: self.observed_history.clone(),
                local_port: self.local_quic_port,
                evidence: format!("Observed port {} consistent across {} observations - Cone NAT", valid_ports[0], valid_ports.len()),
                dcutr_feasible: true,
            }
        } else {
            let ports_str: Vec<String> = valid_ports.iter().map(|p| p.to_string()).collect();
            NatDiagnosis {
                nat_type: NatType::Symmetric,
                observed_addresses: self.observed_history.clone(),
                local_port: self.local_quic_port,
                evidence: format!("Observed ports vary: {} - Symmetric NAT", ports_str.join(", ")),
                dcutr_feasible: false,
            }
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
