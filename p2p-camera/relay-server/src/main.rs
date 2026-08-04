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
        public_ips: opt.public_ip,
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
    // 注意: rust-libp2p 对 IPv6 监听 socket 强制设置 IPV6_V6ONLY=true
    // (transports/tcp/src/lib.rs 与 transports/quic/src/transport.rs 的 create_socket),
    // 所以 /ip6/:: 不会像普通双栈 socket 那样接受 IPv4-mapped 连接。
    // 官方做法是「同时起两个 listener」而不是依赖 IPv4-mapped IPv6。
    // 因此 use_ipv6 = true 表示「额外启用 IPv6」, IPv4 始终监听, 保证 v4 客户端可连。
    let mut listen_addrs: Vec<Multiaddr> = Vec::new();

    // IPv4 始终监听 (TCP + QUIC)
    listen_addrs.push(
        Multiaddr::empty()
            .with(Protocol::from(Ipv4Addr::UNSPECIFIED))
            .with(Protocol::Tcp(config.port)),
    );
    listen_addrs.push(
        Multiaddr::empty()
            .with(Protocol::from(Ipv4Addr::UNSPECIFIED))
            .with(Protocol::Udp(config.port))
            .with(Protocol::QuicV1),
    );

    // IPv6 按需追加 (TCP + QUIC)
    if config.use_ipv6 {
        listen_addrs.push(
            Multiaddr::empty()
                .with(Protocol::from(Ipv6Addr::UNSPECIFIED))
                .with(Protocol::Tcp(config.port)),
        );
        listen_addrs.push(
            Multiaddr::empty()
                .with(Protocol::from(Ipv6Addr::UNSPECIFIED))
                .with(Protocol::Udp(config.port))
                .with(Protocol::QuicV1),
        );
    }

    for addr in &listen_addrs {
        // IPv6 监听失败不应拖垮 IPv4 (例如宿主机/容器未启用 IPv6)
        if let Err(e) = swarm.listen_on(addr.clone()) {
            let is_v6 = addr.iter().any(|p| matches!(p, Protocol::Ip6(_)));
            if is_v6 {
                tracing::warn!("[Relay] Failed to listen on IPv6 {addr}: {e} (continuing with IPv4)");
            } else {
                return Err(format!("Failed to listen on {addr}: {e}").into());
            }
        }
    }

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
    // 客户端可直接复制的完整拨号地址; 未配 public_ips 时退化为占位符提示
    if config.public_ips.is_empty() {
        println!("║ TCP:  /ip4/<PUBLIC_IP>/tcp/{}/p2p/{peer_id}", config.port);
        println!("║ QUIC: /ip4/<PUBLIC_IP>/udp/{}/quic-v1/p2p/{peer_id}", config.port);
    } else {
        for ip_str in &config.public_ips {
            let proto = if ip_str.trim().parse::<std::net::IpAddr>().map(|i| i.is_ipv6()).unwrap_or(false) {
                "ip6"
            } else {
                "ip4"
            };
            println!("║ TCP:  /{proto}/{}/tcp/{}/p2p/{peer_id}", ip_str.trim(), config.port);
            println!("║ QUIC: /{proto}/{}/udp/{}/quic-v1/p2p/{peer_id}", ip_str.trim(), config.port);
        }
    }
    println!("║");
    for addr in &listen_addrs {
        println!("║ Listening: {addr}");
    }
    println!("╚══════════════════════════════════════════╝");

    // ---- 手动添加公网外部地址（若指定） ----
    // public_ips 支持同时配置 IPv4 和 IPv6, 逐个解析并按地址族生成
    // /ip4/... 或 /ip6/... 的 TCP + QUIC 外部地址。
    if !config.public_ips.is_empty() {
        let mut has_v6 = false;
        for ip_str in &config.public_ips {
            let ip: std::net::IpAddr = ip_str.trim().parse()
                .map_err(|e| format!("Invalid public_ips entry '{}': {e}", ip_str))?;

            // 按地址族分别做私网/非法地址校验, 并选出 multiaddr 的 ip 协议前缀
            let proto = match ip {
                std::net::IpAddr::V4(v4) => {
                    if v4.is_private() || v4.is_loopback() || v4.is_link_local() || v4.is_unspecified() {
                        tracing::error!(
                            "[Relay] ERROR: public_ips entry {} is not a public IPv4 address", ip_str
                        );
                        return Err("public_ips must contain only public addresses".into());
                    }
                    "ip4"
                }
                std::net::IpAddr::V6(v6) => {
                    // ULA(fc00::/7) / 链路本地(fe80::/10) / 回环 / 未指定 均非公网
                    let seg = v6.segments();
                    let is_ula = (seg[0] & 0xfe00) == 0xfc00;
                    let is_link_local = (seg[0] & 0xffc0) == 0xfe80;
                    if is_ula || is_link_local || v6.is_loopback() || v6.is_unspecified() {
                        tracing::error!(
                            "[Relay] ERROR: public_ips entry {} is not a public IPv6 address", ip_str
                        );
                        return Err("public_ips must contain only public addresses".into());
                    }
                    has_v6 = true;
                    "ip6"
                }
            };

            let ext_tcp: Multiaddr = format!("/{}/{}/tcp/{}", proto, ip, config.port).parse()
                .map_err(|e| format!("Invalid external TCP address for {ip_str}: {e}"))?;
            let ext_quic: Multiaddr = format!("/{}/{}/udp/{}/quic-v1", proto, ip, config.port).parse()
                .map_err(|e| format!("Invalid external QUIC address for {ip_str}: {e}"))?;
            swarm.add_external_address(ext_tcp.clone());
            swarm.add_external_address(ext_quic.clone());
            tracing::info!("[Relay] Added external addresses: {ext_tcp} , {ext_quic}");
        }

        // 配了公网 IPv6 却没开 IPv6 监听 → 通告的 v6 地址实际不可达
        if has_v6 && !config.use_ipv6 {
            tracing::warn!(
                "[Relay] public_ips contains IPv6 but use_ipv6 = false; \
                 the advertised IPv6 addresses are NOT listening. Set use_ipv6 = true."
            );
        }
    } else {
        tracing::warn!("[Relay] No public_ips specified, relay may advertise private IP via hostname -I");
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

    /// 公网 IP 地址, 支持 IPv4/IPv6, 可重复传多个 (覆盖配置文件的 public_ips)
    /// 例: --public-ip 1.2.3.4 --public-ip 2408:8000::1
    #[arg(long = "public-ip", value_name = "IP")]
    public_ip: Vec<String>,
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
