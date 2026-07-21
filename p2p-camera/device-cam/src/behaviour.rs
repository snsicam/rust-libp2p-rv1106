//! DeviceCam NetworkBehaviour
//!
//! 组合 relay client + DCUtR + identify + stream + mDNS 五个行为。

use libp2p::{
    dcutr, identify, mdns, ping, relay,
    swarm::{behaviour::toggle::Toggle, NetworkBehaviour},
};
use tracing::info;

#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub relay_client: relay::client::Behaviour,
    pub dcutr: Toggle<dcutr::Behaviour>,
    pub identify: identify::Behaviour,
    pub stream: libp2p_stream::Behaviour,
    pub ping: ping::Behaviour,
    pub mdns: mdns::tokio::Behaviour,
}

impl Behaviour {
    /// 由 SwarmBuilder 回调构造，relay_client 必须由 builder 传入
    #[allow(dead_code)]
    pub fn new(
        local_public_key: libp2p::identity::PublicKey,
        relay_client: relay::client::Behaviour,
    ) -> Self {
        info!("[DeviceCam] Creating new Behaviour with DCUtR and Relay client");
        let identify_config = identify::Config::new(
            "/p2p-camera-device-cam/1.0.0".to_string(),
            local_public_key.clone(),
        );
        // 默认启用 DCUtR（锥形/EIM NAT 含多数宽带/4G 可打洞，省中继带宽）
        Self::new_with_identify_config(local_public_key, relay_client, identify_config, true)
    }

    /// 允许自定义 identify 配置（例如启用 push_listen_addr_updates）
    /// `enable_dcutr`: 是否启用 DCUtR 直连打洞。4G/CGNAT 等入站 UDP 被屏蔽的网络下
    /// 打洞必然失败，且打洞握手会挤占中继视频流带宽导致卡顿，应设为 false。
    pub fn new_with_identify_config(
        local_public_key: libp2p::identity::PublicKey,
        relay_client: relay::client::Behaviour,
        identify_config: identify::Config,
        enable_dcutr: bool,
    ) -> Self {
        let peer_id = local_public_key.to_peer_id();
        info!("[DeviceCam] Creating new Behaviour for peer_id: {}", peer_id);
        let dcutr_behaviour = if enable_dcutr {
            info!("[DeviceCam] DCUtR enabled for direct connection upgrade");
            Toggle::from(Some(dcutr::Behaviour::new(peer_id)))
        } else {
            info!("[DeviceCam] DCUtR DISABLED (enable_dcutr=false) - relay circuit only");
            Toggle::from(None)
        };
        info!("[DeviceCam] Relay client enabled for circuit fallback");
        info!("[DeviceCam] mDNS enabled for LAN discovery");
        Self {
            relay_client,  // 使用 builder 传入的，不能自己构造
            dcutr: dcutr_behaviour,
            identify: identify::Behaviour::new(identify_config),
            stream: libp2p_stream::Behaviour::new(),
            ping: ping::Behaviour::new(
                ping::Config::default()
                    .with_interval(std::time::Duration::from_secs(5)),
            ),
            mdns: mdns::tokio::Behaviour::new(
                mdns::Config::default(),
                peer_id,
            ).expect("Failed to initialize mDNS"),
        }
    }

    pub fn new_stream_control(&self) -> libp2p_stream::Control {
        self.stream.new_control()
    }
}
