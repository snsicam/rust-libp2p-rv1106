# 1. 实现模型

## 1.1 上下文视图

本设计涉及三个核心组件的改造：DeviceCam、Viewer（mobile-core 库 + viewer_cli 示例）、以及共享的配置系统。

```
┌─────────────────────────────────────────────────────────────────┐
│                        改造范围                                   │
│                                                                   │
│  ┌──────────────┐    ┌──────────────┐    ┌──────────────────┐   │
│  │  DeviceCam   │    │    Viewer    │    │   配置系统        │   │
│  │              │    │              │    │                   │   │
│  │ + mDNS 广播  │    │ + mDNS 发现  │    │ + relays 列表     │   │
│  │ + 多路预约   │    │ + 并发拨号   │    │ + enable_mdns     │   │
│  │ + 多 Relay   │    │ + mDNS 优先  │    │ + 向后兼容 relay  │   │
│  │   重连       │    │   连接策略   │    │                   │   │
│  └──────────────┘    └──────────────┘    └──────────────────┘   │
│                                                                   │
└─────────────────────────────────────────────────────────────────┘
```

## 1.2 服务/组件总体架构

### 1.2.1 Viewer 连接策略状态机

Viewer 启动连接时，采用 mDNS 发现与 Relay 连接并行的策略：

```
                    ┌─────────────┐
                    │  启动连接    │
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐
        │ mDNS 发现 │ │ Relay 1  │ │ Relay N  │
        │ (5s超时)  │ │ 连接+拨号 │ │ 连接+拨号 │
        └─────┬────┘ └─────┬────┘ └─────┬────┘
              │             │             │
              ▼             ▼             ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐
        │ 发现目标? │ │ Circuit  │ │ Circuit  │
        │ PeerId   │ │ 成功?    │ │ 成功?    │
        └─────┬────┘ └─────┬────┘ └─────┬────┘
              │             │             │
     ┌───────┴───────┐     │             │
     ▼               ▼     ▼             ▼
  ┌────────┐  ┌──────────────────────────────┐
  │mDNS直连│  │  使用最先成功的 Relay Circuit  │
  │拨号    │  │  (先到先用)                   │
  └───┬────┘  └──────────────┬───────────────┘
      │                      │
      ▼                      ▼
  ┌──────────────────────────────────┐
  │     打开视频/音频 stream          │
  │     (DCUtR 自动尝试升级)          │
  └──────────────────────────────────┘
```

### 1.2.2 DeviceCam 多路预约状态机

DeviceCam 启动时同时向所有 Relay 请求 Reservation：

```
                    ┌─────────────┐
                    │  DeviceCam   │
                    │   启动       │
                    └──────┬──────┘
                           │
              ┌────────────┼────────────┐
              ▼            ▼            ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐
        │ Relay 1  │ │ Relay 2  │ │ Relay N  │
        │ 连接+预约 │ │ 连接+预约 │ │ 连接+预约 │
        └─────┬────┘ └─────┬────┘ └─────┬────┘
              │             │             │
              ▼             ▼             ▼
        ┌──────────┐ ┌──────────┐ ┌──────────┐
        │Reservation│ │Reservation│ │Reservation│
        │ 成功/失败 │ │ 成功/失败 │ │ 成功/失败 │
        └─────┬────┘ └─────┬────┘ └─────┬────┘
              │             │             │
              └─────────────┼─────────────┘
                            ▼
                  ┌──────────────────┐
                  │ 至少一路成功?     │
                  └────────┬─────────┘
                     ┌─────┴─────┐
                     ▼           ▼
                  ┌──────┐  ┌──────────┐
                  │ 可用  │  │ 继续重连  │
                  │ 状态  │  │ (mDNS仍可)│
                  └──────┘  └──────────┘
```

### 1.2.3 组件依赖关系

```
libp2p (workspace)
  ├── features: tokio, tcp, quic, noise, yamux, relay, dcutr, identify, ping, macros, mdns (新增)
  │
  ├── device-cam
  │   ├── Cargo.toml: 新增 "mdns" feature
  │   ├── config.rs: relay → relays (Vec<String>), 新增 enable_mdns
  │   ├── behaviour.rs: 新增 mdns::Behaviour 字段
  │   └── main.rs: 多路预约逻辑 + mDNS 广播
  │
  ├── mobile-core
  │   ├── Cargo.toml: 新增 "mdns" feature
  │   ├── src/viewer.rs: mDNS 优先 + 多 Relay 并发拨号
  │   └── examples/viewer_cli.rs: 同步改造
  │
  └── relay-server (无变更)
```

## 1.3 实现设计文档

### 1.3.1 配置系统改造

**DeviceCam 配置 (`device-cam/src/config.rs`)**

现有结构：
```rust
pub struct Config {
    pub relay: String,  // 单个 Relay 地址
    ...
}
```

改造后：
```rust
pub struct Config {
    /// 多 Relay 地址列表 (新格式优先)
    #[serde(default)]
    pub relays: Vec<String>,
    /// 单 Relay 地址 (旧格式, 向后兼容, 解析时合并到 relays)
    #[serde(default)]
    pub relay: String,
    /// 是否启用 mDNS 局域网发现 (默认 true)
    #[serde(default = "default_enable_mdns")]
    pub enable_mdns: bool,
    ...
}
```

**向后兼容策略**：
- TOML 中如果存在 `relay = "..."` 但没有 `relays`，自动将 `relay` 值加入 `relays` 列表
- TOML 中如果同时存在 `relay` 和 `relays`，`relays` 优先，`relay` 被忽略
- `apply_cli_overrides` 中 `--relay` 参数可多次使用，追加到 `relays` 列表

**Viewer 配置 (`viewer_cli.rs` 中的 `ViewerConfig`)**

改造后：
```rust
struct ViewerConfig {
    /// 多 Relay 地址列表
    #[serde(default)]
    pub relays: Vec<String>,
    /// 单 Relay 地址 (旧格式, 向后兼容)
    #[serde(default)]
    pub relay: String,
    /// 摄像头 PeerId
    pub camera: String,
    /// 是否启用 mDNS (默认 true)
    #[serde(default = "default_enable_mdns")]
    pub enable_mdns: bool,
    ...
}
```

**MediaPlayer 库 (`mobile-core/src/viewer.rs`)**

`connect()` 方法签名改造：
```rust
// 旧签名
pub async fn connect(&mut self, relay_addr: &str, device_cam_peer_id: &str) -> Result<()>

// 新签名
pub async fn connect(
    &mut self,
    relay_addrs: &[String],       // 多 Relay 地址列表
    device_cam_peer_id: &str,
    enable_mdns: bool,            // 是否启用 mDNS
) -> Result<()>
```

### 1.3.2 mDNS 集成设计

**libp2p feature 启用**

在 `device-cam/Cargo.toml` 和 `mobile-core/Cargo.toml` 的 libp2p features 中新增 `mdns`：
```toml
libp2p = { workspace = true, features = [
    "tokio", "tcp", "quic", "noise", "yamux",
    "relay", "dcutr", "identify", "ping", "macros", "mdns",
] }
```

**DeviceCam Behaviour 改造 (`device-cam/src/behaviour.rs`)**

```rust
#[derive(NetworkBehaviour)]
pub struct Behaviour {
    pub relay_client: relay::client::Behaviour,
    pub dcutr: dcutr::Behaviour,
    pub identify: identify::Behaviour,
    pub stream: libp2p_stream::Behaviour,
    pub ping: ping::Behaviour,
    pub mdns: libp2p::mdns::tokio::Behaviour,  // 新增
}
```

构造函数中初始化 mDNS：
```rust
mdns: libp2p::mdns::tokio::Behaviour::new(
    libp2p::mdns::Config::default(),
    local_public_key.to_peer_id(),
)?,
```

**Viewer Behaviour 改造 (viewer_cli.rs 和 viewer.rs)**

同样在 `ViewerBehaviour` 中新增 `mdns` 字段。

**mDNS 事件处理**

DeviceCam 侧：mDNS 自动广播，无需额外处理事件（libp2p-mdns 的 `Behaviour` 会自动响应 mDNS 查询并广播自身）。

Viewer 侧：监听 `mdns::Event::Discovered` 事件，检查发现的 PeerId 是否匹配目标 DeviceCam：
```rust
SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
    libp2p::mdns::Event::Discovered(peers),
)) => {
    for (peer_id, addr) in peers {
        if peer_id == device_cam {
            // 找到目标 DeviceCam，使用局域网地址拨号
            swarm.dial(addr)?;
        }
    }
}
```

### 1.3.3 Viewer 连接策略实现

**核心流程：mDNS 优先 + 多 Relay 并发**

```
1. 启动 mDNS 发现（如果 enable_mdns=true）
2. 同时向所有 Relay 发起连接
3. 等待事件：
   a. 如果 mDNS 发现目标 DeviceCam → 立即使用局域网地址拨号
   b. 如果某个 Relay Circuit 连接成功 → 使用该 Circuit 拨号 DeviceCam
   c. 如果 mDNS 5 秒超时 → 降级为 Relay 连接
4. 连接建立后打开 stream
5. DCUtR 自动尝试升级（现有逻辑不变）
```

**并发拨号实现方式**

使用 `tokio::select!` 或 Swarm 事件循环同时等待多个连接结果：

```rust
// 同时向所有 Relay 发起连接
for relay_addr in relay_addrs {
    let relay: Multiaddr = relay_addr.parse()?;
    swarm.dial(relay)?;
}

// 在事件循环中等待第一个成功的 Circuit 连接
loop {
    match swarm.select_next_some().await {
        SwarmEvent::Behaviour(ViewerBehaviourEvent::Mdns(
            mdns::Event::Discovered(peers),
        )) => {
            // mDNS 发现目标，优先使用
            for (peer_id, addr) in peers {
                if peer_id == device_cam {
                    swarm.dial(addr)?;
                    // 等待连接建立...
                    break;
                }
            }
        }
        SwarmEvent::ConnectionEstablished { peer_id, endpoint, .. } => {
            // 检查是否为 Circuit 连接到 DeviceCam
            if peer_id == device_cam {
                let addr = endpoint.get_remote_address();
                if addr.iter().any(|p| matches!(p, Protocol::P2pCircuit)) {
                    // 通过 Relay Circuit 连接成功
                    break;
                }
            }
            // 检查是否为 Relay Server 连接成功
            if relay_peer_ids.contains(&peer_id) {
                // Relay 连接成功，发起 Circuit 拨号
                let circuit_addr = relay_addr_of(peer_id)
                    .with(Protocol::P2pCircuit)
                    .with(Protocol::P2p(device_cam));
                swarm.dial(circuit_addr)?;
            }
        }
        // ...
    }
}
```

**mDNS 超时机制**

使用 `tokio::time::timeout` 或在事件循环中检查时间：
```rust
let mdns_deadline = tokio::time::Instant::now() + Duration::from_secs(5);
let mut mdns_discovered = false;

// 在事件循环中
if !mdns_discovered && tokio::time::Instant::now() > mdns_deadline {
    // mDNS 超时，降级为 Relay
    tracing::info!("[Viewer] mDNS discovery timeout, falling back to relay");
}
```

### 1.3.4 DeviceCam 多路预约实现

**Relay 状态管理**

```rust
/// 单个 Relay 的连接和预约状态
struct RelayState {
    /// Relay 的 Multiaddr
    addr: Multiaddr,
    /// Relay 的 PeerId
    peer_id: PeerId,
    /// 当前 Reservation 的 ListenerId
    reservation_id: Option<libp2p::core::transport::ListenerId>,
    /// 是否已连接
    connected: bool,
    /// 重连尝试次数 (用于指数退避)
    reconnect_attempt: u32,
}
```

**多路预约流程**

```rust
// 初始化：同时连接所有 Relay
let mut relay_states: Vec<RelayState> = relay_addrs.iter().map(|addr| {
    let multiaddr: Multiaddr = addr.parse().unwrap();
    let peer_id = extract_peer_id(&multiaddr).unwrap();
    RelayState {
        addr: multiaddr,
        peer_id,
        reservation_id: None,
        connected: false,
        reconnect_attempt: 0,
    }
}).collect();

// 同时拨号所有 Relay
for state in &relay_states {
    swarm.dial(state.addr.clone())?;
}

// 事件循环中处理
SwarmEvent::ConnectionEstablished { peer_id, .. } => {
    if let Some(state) = relay_states.iter_mut().find(|s| s.peer_id == peer_id) {
        state.connected = true;
        state.reconnect_attempt = 0;
        // 请求 Reservation
        if state.reservation_id.is_none() {
            match swarm.listen_on(state.addr.clone().with(Protocol::P2pCircuit)) {
                Ok(id) => state.reservation_id = Some(id),
                Err(e) => tracing::error!("Reservation failed: {e}"),
            }
        }
    }
}

SwarmEvent::Behaviour(BehaviourEvent::RelayClient(
    relay::client::Event::ReservationReqAccepted { .. },
)) => {
    // 检查至少一个 Relay 预约成功
    let any_reserved = relay_states.iter().any(|s| s.reservation_id.is_some());
    if any_reserved {
        println!("[DeviceCam] At least one relay reservation confirmed!");
    }
}

SwarmEvent::ConnectionClosed { peer_id, num_established, .. } => {
    if let Some(state) = relay_states.iter_mut().find(|s| s.peer_id == peer_id) {
        if num_established == 0 {
            state.connected = false;
            state.reservation_id = None;
            state.reconnect_attempt += 1;
            // 指数退避重连
        }
    }
}
```

**重连逻辑**

每个 Relay 独立重连，互不影响：
```rust
// 在事件循环末尾检查
for state in &mut relay_states {
    if !state.connected && state.reconnect_attempt > 0 {
        let delay = calculate_backoff(state.reconnect_attempt);
        // 延迟后重连
        tokio::time::sleep(delay).await;
        match swarm.dial(state.addr.clone()) {
            Ok(()) => println!("[DeviceCam] Reconnecting to relay {} (attempt {})",
                state.peer_id, state.reconnect_attempt),
            Err(e) => state.reconnect_attempt += 1,
        }
    }
}
```

### 1.3.5 CLI 参数改造

**DeviceCam CLI (`device-cam/src/main.rs`)**

```rust
#[derive(Debug, Parser)]
struct Opt {
    /// 配置文件路径
    #[arg(long, default_value = "device-cam.toml")]
    config: PathBuf,

    /// Relay Server 地址 (可多次使用, 覆盖配置文件)
    #[arg(long = "relay")]
    relays: Vec<String>,

    /// 是否启用 mDNS (覆盖配置文件)
    #[arg(long)]
    enable_mdns: Option<bool>,

    // ... 其他参数不变
}
```

**Viewer CLI (`viewer_cli.rs`)**

```rust
#[derive(Debug, Parser)]
struct Opt {
    /// 配置文件路径
    #[arg(long, default_value = "viewer.toml")]
    config: PathBuf,

    /// Relay Server 地址 (可多次使用, 覆盖配置文件)
    #[arg(long = "relay")]
    relays: Vec<String>,

    /// 摄像头 PeerId
    #[arg(long)]
    camera: Option<String>,

    /// 是否启用 mDNS (覆盖配置文件)
    #[arg(long)]
    enable_mdns: Option<bool>,

    // ... 其他参数不变
}
```

# 2. 接口设计

## 2.1 总体设计

本次改造主要涉及内部接口变更，不涉及外部 API（MediaPlayer 的 FFI 接口需要同步更新）。

## 2.2 接口清单

### 2.2.1 MediaPlayer::connect() 接口变更

**文件**: `mobile-core/src/viewer.rs`

| 项目 | 旧接口 | 新接口 |
|------|--------|--------|
| 方法签名 | `connect(&mut self, relay_addr: &str, device_cam_peer_id: &str)` | `connect(&mut self, relay_addrs: &[String], device_cam_peer_id: &str, enable_mdns: bool)` |
| relay 参数 | 单个地址字符串 | 地址列表 |
| mDNS 控制 | 无 | 通过 enable_mdns 参数 |

### 2.2.2 DeviceCam Config 接口变更

**文件**: `device-cam/src/config.rs`

| 项目 | 旧接口 | 新接口 |
|------|--------|--------|
| relay 字段 | `pub relay: String` | `pub relay: String` (保留，向后兼容) |
| relays 字段 | 无 | `pub relays: Vec<String>` (新增) |
| enable_mdns 字段 | 无 | `pub enable_mdns: bool` (新增，默认 true) |
| CliOverrides.relay | `pub relay: Option<String>` | `pub relays: Vec<String>` (改为列表) |
| CliOverrides.enable_mdns | 无 | `pub enable_mdns: Option<bool>` (新增) |

### 2.2.3 ViewerConfig 接口变更

**文件**: `mobile-core/examples/viewer_cli.rs`

| 项目 | 旧接口 | 新接口 |
|------|--------|--------|
| relay 字段 | `pub relay: String` | `pub relay: String` (保留，向后兼容) |
| relays 字段 | 无 | `pub relays: Vec<String>` (新增) |
| enable_mdns 字段 | 无 | `pub enable_mdns: bool` (新增，默认 true) |

### 2.2.4 ViewerBehaviour 接口变更

**文件**: `mobile-core/src/viewer.rs` 和 `viewer_cli.rs`

| 项目 | 旧接口 | 新接口 |
|------|--------|--------|
| mdns 字段 | 无 | `pub mdns: libp2p::mdns::tokio::Behaviour` (新增) |
| 事件枚举 | `ViewerBehaviourEvent` | `ViewerBehaviourEvent` (自动派生新增 Mdns 变体) |

### 2.2.5 DeviceCam Behaviour 接口变更

**文件**: `device-cam/src/behaviour.rs`

| 项目 | 旧接口 | 新接口 |
|------|--------|--------|
| mdns 字段 | 无 | `pub mdns: libp2p::mdns::tokio::Behaviour` (新增) |
| 事件枚举 | `BehaviourEvent` | `BehaviourEvent` (自动派生新增 Mdns 变体) |

# 4. 数据模型

## 4.1 设计目标

1. 支持多 Relay 地址的配置存储和解析
2. 向后兼容旧的单 Relay 配置格式
3. 跟踪每个 Relay 的独立连接和预约状态
4. 支持 mDNS 开关配置

## 4.2 模型实现

### 4.2.1 DeviceCam 配置文件格式

旧格式（继续支持）：
```toml
relay = "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1"
mode = "listen"
key_file = "device-cam.key"
enable_audio = false
```

新格式：
```toml
# 多 Relay 配置 (新)
relays = [
    "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1",
    "/ip4/203.0.113.5/udp/4001/quic-v1/p2p/12D3KooWAnotherRelayPeerIdHere",
]

# 单 Relay 配置 (旧, 向后兼容, 与 relays 同时存在时 relays 优先)
# relay = "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1"

# mDNS 局域网发现 (默认 true)
enable_mdns = true

mode = "listen"
key_file = "device-cam.key"
enable_audio = false
```

### 4.2.2 Viewer 配置文件格式

旧格式（继续支持）：
```toml
relay = "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1"
camera = "12D3KooWMUJiAVDFby2xwhFQp3wxWeDPw4hfaJLAxbbfUVbQZKEH"
no_audio = false
play = true
```

新格式：
```toml
# 多 Relay 配置 (新)
relays = [
    "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1",
    "/ip4/203.0.113.5/udp/4001/quic-v1/p2p/12D3KooWAnotherRelayPeerIdHere",
]

# 单 Relay 配置 (旧, 向后兼容)
# relay = "/ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1"

# mDNS 局域网发现 (默认 true)
enable_mdns = true

camera = "12D3KooWMUJiAVDFby2xwhFQp3wxWeDPw4hfaJLAxbbfUVbQZKEH"
no_audio = false
play = true
```

### 4.2.3 RelayState 运行时状态

```rust
/// 单个 Relay 的运行时状态 (DeviceCam 侧)
struct RelayState {
    /// Relay 的完整 Multiaddr
    addr: Multiaddr,
    /// Relay 的 PeerId (从 addr 中提取)
    peer_id: PeerId,
    /// 当前 Reservation 的 ListenerId (None 表示未预约)
    reservation_id: Option<ListenerId>,
    /// 是否已连接到该 Relay
    connected: bool,
    /// 重连尝试次数 (0=首次, >0=重连中)
    reconnect_attempt: u32,
}
```

### 4.2.4 Viewer 连接状态

```rust
/// Viewer 连接过程中的状态
struct ViewerConnectState {
    /// 是否已通过 mDNS 发现目标 DeviceCam
    mdns_discovered: bool,
    /// mDNS 发现超时时间点
    mdns_deadline: Instant,
    /// 已成功连接的 Relay PeerId 集合
    connected_relays: HashSet<PeerId>,
    /// 是否已通过 Circuit 连接到 DeviceCam
    circuit_connected: bool,
    /// 是否已通过 mDNS 局域网直连到 DeviceCam
    lan_connected: bool,
}
```
