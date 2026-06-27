//! Gateway NetworkBehaviour
//!
//! 组合 relay client + DCUtR + identify + stream 四个行为。

use libp2p::{
    dcutr, identify, ping, relay,
    swarm::NetworkBehaviour,
};
use tracing::info;

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub relay_client: relay::client::Behaviour,
    pub dcutr: dcutr::Behaviour,
    pub identify: identify::Behaviour,
    pub stream: libp2p_stream::Behaviour,
    pub ping: ping::Behaviour,
}

impl Behaviour {
    /// 由 SwarmBuilder 回调构造，relay_client 必须由 builder 传入
    pub fn new(
        local_public_key: libp2p::identity::PublicKey,
        relay_client: relay::client::Behaviour,
    ) -> Self {
        info!("[Gateway] Creating new Behaviour with DCUtR and Relay client");
        let identify_config = identify::Config::new(
            "/p2p-camera-gateway/1.0.0".to_string(),
            local_public_key.clone(),
        );
        Self::new_with_identify_config(local_public_key, relay_client, identify_config)
    }

    /// 允许自定义 identify 配置（例如启用 push_listen_addr_updates）
    pub fn new_with_identify_config(
        local_public_key: libp2p::identity::PublicKey,
        relay_client: relay::client::Behaviour,
        identify_config: identify::Config,
    ) -> Self {
        let peer_id = local_public_key.to_peer_id();
        info!("[Gateway] Creating new Behaviour for peer_id: {}", peer_id);
        info!("[Gateway] DCUtR enabled for direct connection upgrade");
        info!("[Gateway] Relay client enabled for circuit fallback");
        Self {
            relay_client,  // 使用 builder 传入的，不能自己构造
            dcutr: dcutr::Behaviour::new(peer_id),
            identify: identify::Behaviour::new(identify_config),
            stream: libp2p_stream::Behaviour::new(),
            ping: ping::Behaviour::new(
                ping::Config::default()
                    .with_interval(std::time::Duration::from_secs(5)),
            ),
        }
    }

    pub fn new_stream_control(&self) -> libp2p_stream::Control {
        self.stream.new_control()
    }
}
