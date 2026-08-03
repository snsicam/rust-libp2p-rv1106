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
mod config;

use std::{
    collections::HashMap,
    error::Error,
    net::{Ipv4Addr, Ipv6Addr},
    path::PathBuf,
    sync::Arc,
};

use behaviour::Behaviour;
use clap::Parser;
use futures::{AsyncReadExt, AsyncWriteExt, StreamExt};
use libp2p::{
    core::multiaddr::{Multiaddr, Protocol},
    identify, identity, noise,
    swarm::SwarmEvent,
    tcp, yamux, PeerId,
};
use libp2p_stream::Control;
use proto::registry::{RegistryMessage, REGISTRY_PROTOCOL};
use tokio::sync::Mutex;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> Result<(), Box<dyn Error>> {
    println!("[Relay] p2p-camera relay-server v{} ({})", env!("CARGO_PKG_VERSION"), env!("BUILD_TIME"));

    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| EnvFilter::new("info")),
        )
        .init();

    let opt = Opt::parse();

    // ---- 加载配置文件 ----
    let mut config = config::Config::load(&opt.config).unwrap_or_else(|e| {
        eprintln!("[Relay] {e}");
        std::process::exit(1);
    });

    // 命令行参数覆盖配置文件
    let cli_overrides = config::CliOverrides {
        use_ipv6: opt.use_ipv6,
        key_file: opt.key_file,
        port: opt.port,
        public_ip: opt.public_ip,
    };
    config.apply_cli_overrides(&cli_overrides);

    // 从文件加载固定身份密钥, 保证 PeerId 不变 (方便配置)
    let keypair = load_or_create_keypair(&config.key_file)?;
    let peer_id = keypair.public().to_peer_id();

    let mut swarm = libp2p::SwarmBuilder::with_existing_identity(keypair)
        .with_tokio()
        .with_tcp(
            tcp::Config::default().nodelay(true),
            noise::Config::new,
            yamux::Config::default,
        )?
        // QUIC max_idle_timeout: 实际生效值 = 两端协商的较小者。
        // relay 端若保持默认 10s, 客户端单方面调大无效, 因此必须同步为 30s。
        .with_quic_config(|mut c| {
            c.max_idle_timeout = 30_000;
            c.keep_alive_interval = std::time::Duration::from_secs(3);
            c
        })
        .with_behaviour(|key| Behaviour::new(key.public()))?
        // swarm idle timeout 调大到 10min: QUIC keep-alive(3s) 负责 NAT 保活; 原 120s 误杀无子协议
        // 活跃的 relay 中转连接, 导致 viewer 每 ~2min 整轮重连。不设 0 防死连接永久挂。
        .with_swarm_config(|c| c.with_idle_connection_timeout(std::time::Duration::from_secs(600)))
        .build();

    // ---- 监听 ----
    let tcp_addr = Multiaddr::empty()
        .with(match config.use_ipv6 {
            true => Protocol::from(Ipv6Addr::UNSPECIFIED),
            false => Protocol::from(Ipv4Addr::UNSPECIFIED),
        })
        .with(Protocol::Tcp(config.port));
    swarm.listen_on(tcp_addr.clone())?;

    let quic_addr = Multiaddr::empty()
        .with(match config.use_ipv6 {
            true => Protocol::from(Ipv6Addr::UNSPECIFIED),
            false => Protocol::from(Ipv4Addr::UNSPECIFIED),
        })
        .with(Protocol::Udp(config.port))
        .with(Protocol::QuicV1);
    swarm.listen_on(quic_addr.clone())?;

    // ---- 注册表 (serial → peer_id 签名绑定) ----
    // 相机用私钥签名 (serial || peer_id) 后 REGISTER；viewer 用 serial QUERY 取回真实 peer_id。
    // 共享表需跨 spawned 任务访问，用 Arc<Mutex<>> 保护。
    let registry: Arc<Mutex<HashMap<String, RegistryEntry>>> =
        Arc::new(Mutex::new(HashMap::new()));

    // 接受相机/viewer 的注册表应用流
    let mut stream_control: Control = swarm.behaviour().stream.new_control();
    let mut incoming_registry = stream_control
        .accept(REGISTRY_PROTOCOL)
        .map_err(|e| format!("Failed to register registry protocol: {e}"))?;
    println!("[Relay] Registry protocol ready: {REGISTRY_PROTOCOL}");

    // ---- 打印关键信息 (DeviceCam / Viewer 需要) ----
    println!("╔══════════════════════════════════════════╗");
    println!("║     P2P Camera Relay Server              ║");
    println!("╠══════════════════════════════════════════╣");
    println!("║ PeerId: {peer_id}");
    println!("║");
    println!("║ TCP:  /ip4/<PUBLIC_IP>/tcp/{}/p2p/{peer_id}", config.port);
    println!("║ QUIC: /ip4/<PUBLIC_IP>/udp/{}/quic-v1/p2p/{peer_id}", config.port);
    println!("║");
    println!("║ Listening TCP:  {tcp_addr}");
    println!("║ Listening QUIC: {quic_addr}");
    println!("╚══════════════════════════════════════════╝");

    // ---- 手动添加公网外部地址（若指定） ----
    if let Some(ref ip_str) = config.public_ip {
        let ip: std::net::IpAddr = ip_str.parse()
            .map_err(|e| format!("Invalid public_ip '{}': {e}", ip_str))?;
        if let std::net::IpAddr::V4(v4) = ip {
            if v4.is_private() {
                tracing::error!("[Relay] ERROR: public_ip {} is a private IP - must be a public IP", ip_str);
                return Err("Public IP must not be a private address".into());
            }
        }
        if ip.is_ipv4() {
            let ext_tcp: Multiaddr = format!("/ip4/{}/tcp/{}", ip_str, config.port).parse()
                .map_err(|e| format!("Invalid external TCP address: {e}"))?;
            let ext_quic: Multiaddr = format!("/ip4/{}/udp/{}/quic-v1", ip_str, config.port).parse()
                .map_err(|e| format!("Invalid external QUIC address: {e}"))?;
            swarm.add_external_address(ext_tcp);
            swarm.add_external_address(ext_quic);
            tracing::info!("[Relay] Added external addresses for public IP: {}", ip_str);
        } else {
            let ext_tcp: Multiaddr = format!("/ip6/{}/tcp/{}", ip_str, config.port).parse()
                .map_err(|e| format!("Invalid external TCP address: {e}"))?;
            let ext_quic: Multiaddr = format!("/ip6/{}/udp/{}/quic-v1", ip_str, config.port).parse()
                .map_err(|e| format!("Invalid external QUIC address: {e}"))?;
            swarm.add_external_address(ext_tcp);
            swarm.add_external_address(ext_quic);
            tracing::info!("[Relay] Added external addresses for public IPv6: {}", ip_str);
        }
    } else {
        tracing::warn!("[Relay] No public_ip specified, relay may advertise private IP via hostname -I");
    }

    // ---- 节点连接地址跟踪 ----
    // 记录每个 peer 连接到 relay 时使用的实际 IP 和端口
    let mut peer_conn_addrs: HashMap<PeerId, Multiaddr> = HashMap::new();

    // ---- 事件循环 ----
    loop {
        tokio::select! {
            // libp2p Swarm 事件
            event = swarm.select_next_some() => {
                match event {
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
                
                // ---- 输出节点连接到 relay 的实际 IP 和端口 ----
                if let Some(conn_addr) = peer_conn_addrs.get(&client_peer_id) {
                    let mut conn_ip = String::new();
                    let mut conn_port = String::new();
                    let mut conn_protocol = String::new();
                    for p in conn_addr.iter() {
                        match p {
                            Protocol::Ip4(addr) => conn_ip = addr.to_string(),
                            Protocol::Ip6(addr) => conn_ip = addr.to_string(),
                            Protocol::Tcp(p) => {
                                conn_port = p.to_string();
                                conn_protocol = "TCP".to_string();
                            }
                            Protocol::Udp(p) => {
                                conn_port = p.to_string();
                                conn_protocol = "UDP".to_string();
                            }
                            Protocol::QuicV1 => {
                                conn_protocol = format!("{} QUIC", conn_protocol);
                            }
                            _ => {}
                        }
                    }
                    tracing::info!("[Relay] Connected from: {} (IP={}, Port={}, Protocol={})", conn_addr, conn_ip, conn_port, conn_protocol);
                } else {
                    tracing::warn!("[Relay] Connected from: unknown (no connection address recorded for this peer)");
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
                // 记录 peer 的连接地址（用于 Identify 事件中输出）
                peer_conn_addrs.insert(peer_id, addr.clone());
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
                // 如果该 peer 没有剩余连接，清除连接地址记录
                if num_established == 0 {
                    peer_conn_addrs.remove(&peer_id);
                }
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
                } // match event
            }, // swarm 分支结束

            // 注册表应用流: 相机注册 / viewer 查询
            reg = incoming_registry.next() => {
                if let Some((peer, stream)) = reg {
                    let reg_table = registry.clone();
                    tokio::spawn(async move {
                        if let Err(e) = handle_registry_connection(peer, stream, reg_table).await {
                            tracing::warn!("[Relay] registry handler error: {e}");
                        }
                    });
                }
            }
        } // select!
    } // loop
}

#[derive(Debug, Parser)]
#[command(name = "p2p-camera relay-server")]
struct Opt {
    /// 配置文件路径 (不存在则自动生成默认配置)
    #[arg(long, default_value = "relay-server.toml")]
    config: PathBuf,

    /// 监听 IPv6 (覆盖配置文件)
    #[arg(long, default_value_t = false)]
    use_ipv6: bool,

    /// 身份密钥文件 (覆盖配置文件)
    #[arg(long)]
    key_file: Option<PathBuf>,

    /// 监听端口 (覆盖配置文件)
    #[arg(long)]
    port: Option<u16>,

    /// 公网 IP 地址 (覆盖配置文件)
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

/// 注册表条目: 序列号绑定的相机身份 (protobuf 编码的二进制)
#[derive(Clone)]
struct RegistryEntry {
    /// PeerId 的 protobuf/multihash 二进制
    peer_id: Vec<u8>,
    /// 相机公钥 (protobuf 编码)
    pubkey: Vec<u8>,
    /// 相机对 (serial || peer_id) 的 ed25519 签名
    signature: Vec<u8>,
}

/// 处理一条注册表应用流 (相机注册 或 viewer 查询)
///
/// 关键安全校验:
/// 1. 连接认证的 peer (`remote_peer`，noise 握手确定) 必须等于消息声明的 `peer_id`，
///    防止第三方用自己合法身份为别人的 serial 抢注假 peer_id。
/// 2. `pubkey` 推导出的 peer_id 必须等于声明的 `peer_id`。
/// 3. 签名 `signature` 必须能用 `pubkey` 验证 `serial || peer_id`。
async fn handle_registry_connection(
    remote_peer: PeerId,
    mut stream: libp2p::swarm::Stream,
    registry: Arc<Mutex<HashMap<String, RegistryEntry>>>,
) -> anyhow::Result<()> {
    // 读取一条完整消息 (注册表消息很小，单次或少量读取即可)
    let mut buf: Vec<u8> = Vec::with_capacity(512);
    let mut tmp = [0u8; 1024];
    loop {
        let n = stream.read(&mut tmp).await?;
        if n == 0 {
            break; // EOF
        }
        buf.extend_from_slice(&tmp[..n]);
        // 尝试解码；成功说明消息已完整
        if RegistryMessage::decode(&buf).is_ok() {
            break;
        }
        if buf.len() >= 8192 {
            break; // 防垃圾数据死循环
        }
    }
    if buf.is_empty() {
        anyhow::bail!("registry stream closed without data");
    }
    let msg = RegistryMessage::decode(&buf)?;

    match msg {
        RegistryMessage::Register {
            serial,
            peer_id,
            pubkey,
            signature,
        } => {
            // 1) 连接认证的 peer 必须等于声明的 peer_id
            let claimed = PeerId::from_bytes(&peer_id)
                .map_err(|e| anyhow::anyhow!("invalid peer_id: {e}"))?;
            if claimed != remote_peer {
                let err = RegistryMessage::Error {
                    message: "peer_id 与连接身份不符 (未认证)".into(),
                };
                stream.write_all(&err.encode()).await?;
                stream.flush().await?;
                anyhow::bail!("register rejected: claimed {claimed} != connection {remote_peer}");
            }

            // 2) pubkey 推导的 peer_id 必须匹配
            let pk = identity::PublicKey::try_decode_protobuf(&pubkey)
                .map_err(|e| anyhow::anyhow!("invalid pubkey: {e}"))?;
            if pk.to_peer_id() != claimed {
                let err = RegistryMessage::Error {
                    message: "pubkey 与 peer_id 不匹配".into(),
                };
                stream.write_all(&err.encode()).await?;
                stream.flush().await?;
                anyhow::bail!("register rejected: pubkey/peer_id mismatch");
            }

            // 3) 验签
            let payload = RegistryMessage::sign_payload(&serial, &peer_id);
            if !pk.verify(&payload, &signature) {
                let err = RegistryMessage::Error {
                    message: "签名无效".into(),
                };
                stream.write_all(&err.encode()).await?;
                stream.flush().await?;
                anyhow::bail!("register rejected: bad signature");
            }

            registry.lock().await.insert(
                serial.clone(),
                RegistryEntry {
                    peer_id: peer_id.clone(),
                    pubkey: pubkey.clone(),
                    signature: signature.clone(),
                },
            );
            tracing::info!("[Relay] Registered serial '{serial}' -> {claimed}");
            let ack = RegistryMessage::Response {
                peer_id,
                pubkey,
                signature,
            };
            stream.write_all(&ack.encode()).await?;
            stream.flush().await?;
        }

        RegistryMessage::Query { serial } => {
            let entry = registry.lock().await.get(&serial).cloned();
            match entry {
                Some(e) => {
                    let resp = RegistryMessage::Response {
                        peer_id: e.peer_id,
                        pubkey: e.pubkey,
                        signature: e.signature,
                    };
                    stream.write_all(&resp.encode()).await?;
                    stream.flush().await?;
                    tracing::info!("[Relay] Query serial '{serial}' -> found");
                }
                None => {
                    let nf = RegistryMessage::NotFound;
                    stream.write_all(&nf.encode()).await?;
                    stream.flush().await?;
                    tracing::info!("[Relay] Query serial '{serial}' -> not found");
                }
            }
        }

        other => {
            let err = RegistryMessage::Error {
                message: format!("unexpected message: {other:?}"),
            };
            stream.write_all(&err.encode()).await?;
            stream.flush().await?;
            anyhow::bail!("unexpected registry message");
        }
    }
    Ok(())
}
