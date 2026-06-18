//! Gateway NetworkBehaviour
//!
//! 组合 relay client + DCUtR + identify + stream 四个行为。

use libp2p::{
    dcutr, identify, ping, relay,
    swarm::NetworkBehaviour,
};
use libp2p_stream;

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
        let peer_id = local_public_key.to_peer_id();
        Self {
            relay_client,  // 使用 builder 传入的，不能自己构造
            dcutr: dcutr::Behaviour::new(peer_id),
            identify: identify::Behaviour::new(
                identify::Config::new("/p2p-camera-gateway/1.0.0".to_string(), local_public_key),
            ),
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
