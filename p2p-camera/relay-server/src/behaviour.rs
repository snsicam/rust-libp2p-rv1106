use libp2p::{
    identify, noise, ping, relay,
    swarm::{NetworkBehaviour, SwarmEvent},
    tcp, yamux,
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
        Self {
            relay: relay::Behaviour::new(peer_id, Default::default()),
            identify: identify::Behaviour::new(
                identify::Config::new("/p2p-camera-relay/1.0.0".to_string(), local_public_key),
            ),
            ping: ping::Behaviour::new(
                ping::Config::default().with_interval(Duration::from_secs(15)),
            ),
        }
    }
}
