use libp2p::{
    identify, ping, relay,
    swarm::NetworkBehaviour,
};
use std::time::Duration;

/// 组合所有 Relay Server 需要的行为
#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub relay: relay::Behaviour,
    pub identify: identify::Behaviour,
    pub ping: ping::Behaviour,
}

impl Behaviour {
    pub fn new(local_public_key: libp2p::identity::PublicKey) -> Self {
        let peer_id = local_public_key.to_peer_id();

        // 专用 relay server 配置 — 放宽 circuit 限制以支持视频流
        // 默认 max_circuit_bytes=128KiB / max_circuit_duration=2min 远不够视频用
        // max_circuit_bytes=0 表示无限 (copy_future.rs: "if max_circuit_bytes > 0")
        let config = relay::Config {
            max_circuit_duration: Duration::from_secs(60 * 60), // 1 小时
            max_circuit_bytes: 0, // 0 = 无限
            ..Default::default()
        };

        let mut relay = relay::Behaviour::new(peer_id, config);
        // 显式启用 relay — 专用 relay server 不依赖 external address 自动检测。
        // 否则 relay 默认 Status::Disable，在 identify 完成(获得 observed_addr)前
        // 所有 reservation 请求都会因 hop 协议协商失败被拒。
        relay.set_status(Some(relay::Status::Enable));

        Self {
            relay,
            identify: identify::Behaviour::new(
                identify::Config::new("/p2p-camera-relay/1.0.0".to_string(), local_public_key),
            ),
            ping: ping::Behaviour::new(
                ping::Config::default().with_interval(Duration::from_secs(15)),
            ),
        }
    }
}
