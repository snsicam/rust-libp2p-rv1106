//! P2P Camera Relay Server
//!
//! 基于 libp2p relay::Behaviour 的公网中继服务器。
//! 负责:
//! 1. 电路路由 (Circuit Relay v2)
//! 2. 节点身份交换 (Identify)
//! 3. 保活检测 (Ping)
//!
//! 固定身份: 首次运行自动生成 Ed25519 密钥并保存到 key_file，
//!           后续启动从文件读取，保证 PeerId 不变。
//!
//! 注意: 此节点不包含 stream::Behaviour，它只做连接中继，不参与媒体流。

mod behaviour;

use std::{
    error::Error,
    net::{Ipv4Addr, Ipv6Addr},
    path::PathBuf,
};

use behaviour::Behaviour;
use clap::Parser;
use futures::StreamExt;
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    identify, identity, noise,
    swarm::SwarmEvent,
    tcp, yamux,
};
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    let opt = Opt::parse();

    // 从文件加载固定身份密钥, 保证 PeerId 不变 (方便配置)
    let keypair = load_or_create_keypair(&opt.key_file)?;
    let peer_id = keypair.public().to_peer_id();

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        .with_quic()
        .with_behaviour(|key| Behaviour::new(key.public()))?
        .with_swarm_config(|c| c.with_idle_connection_timeout(std::time::Duration::from_secs(120)))
        .build();

    // ---- 监听 ----
    let tcp_addr = Multiaddr::empty()
        .with(match opt.use_ipv6 {
            true => Protocol::from(Ipv6Addr::UNSPECIFIED),
            false => Protocol::from(Ipv4Addr::UNSPECIFIED),
        })
        .with(Protocol::Tcp(opt.port));
    swarm.listen_on(tcp_addr.clone())?;

    let quic_addr = Multiaddr::empty()
        .with(match opt.use_ipv6 {
            true => Protocol::from(Ipv6Addr::UNSPECIFIED),
            false => Protocol::from(Ipv4Addr::UNSPECIFIED),
        })
        .with(Protocol::Udp(opt.port))
        .with(Protocol::QuicV1);
    swarm.listen_on(quic_addr.clone())?;

    // ---- 打印关键信息 (DeviceCam / Viewer 需要) ----
    println!("╔══════════════════════════════════════════╗");
    println!("║     P2P Camera Relay Server              ║");
    println!("╠══════════════════════════════════════════╣");
    println!("║ PeerId: {peer_id}");
    println!("║");
    println!("║ TCP:  /ip4/<PUBLIC_IP>/tcp/{}/p2p/{peer_id}", opt.port);
    println!("║ QUIC: /ip4/<PUBLIC_IP>/udp/{}/quic-v1/p2p/{peer_id}", opt.port);
    println!("║");
    println!("║ Listening TCP:  {tcp_addr}");
    println!("║ Listening QUIC: {quic_addr}");
    println!("╚══════════════════════════════════════════╝");

    // ---- 手动添加公网外部地址（若指定） ----
    if let Some(ref ip_str) = opt.public_ip {
        let ip: std::net::IpAddr = ip_str.parse()
            .map_err(|e| format!("Invalid --public-ip '{}': {e}", ip_str))?;
        if let std::net::IpAddr::V4(v4) = ip {
            if v4.is_private() {
                tracing::error!("[Relay] ERROR: --public-ip {} is a private IP - must be a public IP", ip_str);
                return Err("Public IP must not be a private address".into());
            }
        }
        if ip.is_ipv4() {
            let ext_tcp: Multiaddr = format!("/ip4/{}/tcp/{}", ip_str, opt.port).parse()
                .map_err(|e| format!("Invalid external TCP address: {e}"))?;
            let ext_quic: Multiaddr = format!("/ip4/{}/udp/{}/quic-v1", ip_str, opt.port).parse()
                .map_err(|e| format!("Invalid external QUIC address: {e}"))?;
            swarm.add_external_address(ext_tcp);
            swarm.add_external_address(ext_quic);
            tracing::info!("[Relay] Added external addresses for public IP: {}", ip_str);
        } else {
            let ext_tcp: Multiaddr = format!("/ip6/{}/tcp/{}", ip_str, opt.port).parse()
                .map_err(|e| format!("Invalid external TCP address: {e}"))?;
            let ext_quic: Multiaddr = format!("/ip6/{}/udp/{}/quic-v1", ip_str, opt.port).parse()
                .map_err(|e| format!("Invalid external QUIC address: {e}"))?;
            swarm.add_external_address(ext_tcp);
            swarm.add_external_address(ext_quic);
            tracing::info!("[Relay] Added external addresses for public IPv6: {}", ip_str);
        }
    } else {
        tracing::warn!("[Relay] No --public-ip specified, relay may advertise private IP via hostname -I");
    }

    // ---- 事件循环 ----
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::Behaviour(behaviour::BehaviourEvent::Identify(
                identify::Event::Received {
                    info: identify::Info { observed_addr, listen_addrs, .. },
                    peer_id: client_peer_id,
                    .. // 忽略 connection_id 等额外字段，兼容不同 libp2p 版本
                },
            )) => {
                // ---- 打印 Relay 观察到的地址和客户端公告的监听地址 ----
                tracing::info!("[Relay] ===== Identify from {} =====", client_peer_id);
                tracing::info!("[Relay] Observed address: {}", observed_addr);
                tracing::info!("[Relay] Listen addresses ({} total):", listen_addrs.len());
                for (i, addr) in listen_addrs.iter().enumerate() {
                    tracing::info!("[Relay]   [{}]: {}", i, addr);
                }
                
                // 提取 observed_addr 的 IP 和端口
                let mut ip = String::new();
                let mut port = String::new();
                let mut protocol = String::new();
                for p in observed_addr.iter() {
                    match p {
                        Protocol::Ip4(addr) => {
                            ip = addr.to_string();
                            if addr.is_private() {
                                tracing::warn!("[Relay] WARNING: Observed IP {} is private - DCUtR may fail!", addr);
                            } else {
                                tracing::info!("[Relay] Observed IP {} is public - good for DCUtR", addr);
                            }
                        }
                        Protocol::Ip6(addr) => {
                            ip = addr.to_string();
                            tracing::info!("[Relay] Observed IPv6: {}", addr);
                        }
                        Protocol::Tcp(p) => {
                            port = p.to_string();
                            protocol = "TCP".to_string();
                        }
                        Protocol::Udp(p) => {
                            port = p.to_string();
                            protocol = "UDP".to_string();
                        }
                        Protocol::QuicV1 => {
                            protocol = format!("{} QUIC", protocol);
                        }
                        _ => {}
                    }
                }
                if !ip.is_empty() && !port.is_empty() {
                    tracing::info!("[Relay] Observed: IP={}, Port={}, Protocol={}", ip, port, protocol);
                }
                
                // 将观察到的地址添加到外部地址集（有助于 Relay 自身地址发现，但非必须）
                swarm.add_external_address(observed_addr.clone());
            }

            SwarmEvent::NewListenAddr { address, .. } => {
                tracing::info!("Listening on {}", address);
                if let Some(Protocol::Ip4(ip)) = address.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
                    if ip.is_unspecified() || ip.is_private() {
                        tracing::warn!("[Relay] WARNING: Listening on private/unspecified address ({}) - clients may not be able to connect", ip);
                    }
                }
            }

            SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
                let addr = endpoint.get_remote_address().clone();
                let role = if endpoint.is_dialer() { "outgoing" } else { "incoming" };
                let conn_type = if addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                    "QUIC"
                } else if addr.iter().any(|p| matches!(p, Protocol::Tcp(_))) {
                    "TCP"
                } else {
                    "Other"
                };
                tracing::info!("[Relay] ===== Connection established =====");
                tracing::info!("[Relay] Peer ID: {}", peer_id);
                tracing::info!("[Relay] Role: {}", role);
                tracing::info!("[Relay] Remote address: {}", addr);
                tracing::info!("[Relay] Client connection protocol: {}", conn_type);
                if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                    tracing::info!("[Relay] Type: Relay Circuit connection");
                } else if addr.iter().any(|p| matches!(p, Protocol::QuicV1)) {
                    tracing::info!("[Relay] Type: QUIC direct connection");
                } else if addr.iter().any(|p| matches!(p, Protocol::Tcp(_))) {
                    tracing::info!("[Relay] Type: TCP connection");
                }
            }

            SwarmEvent::ConnectionClosed { peer_id, endpoint, cause, num_established, .. } => {
                let addr = endpoint.get_remote_address().clone();
                let role = if endpoint.is_dialer() { "outgoing" } else { "incoming" };
                tracing::warn!("[Relay] ===== Connection closed =====");
                tracing::warn!("[Relay] Peer ID: {}", peer_id);
                tracing::warn!("[Relay] Role: {}", role);
                tracing::warn!("[Relay] Remote address: {}", addr);
                if let Some(cause) = cause {
                    tracing::warn!("[Relay] Cause: {}", cause);
                }
                tracing::warn!("[Relay] Remaining established connections: {}", num_established);
            }

            SwarmEvent::Behaviour(behaviour::BehaviourEvent::Relay(event)) => {
                // 记录 Relay 事件
                tracing::info!("[Relay] Relay event occurred: {:?}", event);
                // 简单记录事件类型，不深入匹配具体事件，因为 libp2p 版本可能有差异
                {
                    tracing::debug!("[Relay] Relay event: {:?}", event);
                }
            }
            e => {
                tracing::debug!("[Relay] Event: {:?}", e);
            }
        }
    }
}

#[derive(Debug, Parser)]
#[command(name = "p2p-camera relay-server")]
struct Opt {
    /// 监听 IPv6 (默认 IPv4)
    #[arg(long, default_value_t = false)]
    use_ipv6: bool,

    /// 身份密钥文件 (protobuf 格式)
    #[arg(long, default_value = "relay-server.key")]
    key_file: PathBuf,

    /// 监听端口
    #[arg(long, default_value_t = 4001)]
    port: u16,

    /// 公网 IP 地址（替代 hostname -I 自动检测）
    /// 在云服务器上 hostname -I 可能返回内网 IP，必须手动指定公网 IP
    #[arg(long)]
    public_ip: Option<String>,
}

/// 从文件加载密钥，不存在则生成新密钥并保存
fn load_or_create_keypair(key_file: &PathBuf) -> Result<identity::Keypair, Box<dyn Error>> {
    if key_file.exists() {
        let data = std::fs::read(key_file)?;
        let keypair = identity::Keypair::from_protobuf_encoding(&data)
            .map_err(|e| format!("Failed to decode key file {}: {e}", key_file.display()))?;
        tracing::info!("Loaded identity from {}", key_file.display());
        Ok(keypair)
    } else {
        let keypair = identity::Keypair::generate_ed25519();
        let data = keypair.to_protobuf_encoding()
            .map_err(|e| format!("Failed to encode keypair: {e}"))?;
        if let Some(parent) = key_file.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(key_file, &data)?;
        tracing::info!("Generated new identity → {}", key_file.display());
        Ok(keypair)
    }
}
