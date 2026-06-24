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

    // ---- 打印关键信息 (Gateway / Viewer 需要) ----
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

    // ---- 事件循环 ----
    loop {
        match swarm.select_next_some().await {
            SwarmEvent::Behaviour(behaviour::BehaviourEvent::Identify(
                identify::Event::Received {
                    info: identify::Info { observed_addr, .. },
                    ..
                },
            )) => {
                tracing::debug!("Observed address: {observed_addr}");
                swarm.add_external_address(observed_addr.clone());
            }

            SwarmEvent::NewListenAddr { address, .. } => {
                tracing::info!("Listening on {address}");
            }

            SwarmEvent::ConnectionEstablished { peer_id, .. } => {
                tracing::info!("Connection established with {peer_id}");
            }

            SwarmEvent::ConnectionClosed { peer_id, .. } => {
                tracing::info!("Connection closed with {peer_id}");
            }

            e => {
                tracing::debug!("{:?}", e);
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
    /// 首次运行自动生成，后续启动从此文件读取以保证 PeerId 不变
    #[arg(long, default_value = "relay-server.key")]
    key_file: PathBuf,

    /// 监听端口
    #[arg(long, default_value_t = 4001)]
    port: u16,
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
