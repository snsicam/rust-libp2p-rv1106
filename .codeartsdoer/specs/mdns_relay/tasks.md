# 编码任务清单

基于 spec.md 需求规格和 design.md 技术设计，将实现工作拆解为以下编码任务。

---

## 阶段一：配置系统改造（基础设施）

### 任务 1：DeviceCam 配置系统改造

**文件**: `p2p-camera/device-cam/src/config.rs`

**改动内容**:
1. `Config` 结构体新增 `relays: Vec<String>` 字段（`#[serde(default)]`）
2. `Config` 结构体新增 `enable_mdns: bool` 字段（默认 true，新增 `default_enable_mdns()` 函数）
3. 保留 `relay: String` 字段用于向后兼容
4. 新增 `resolve_relays()` 方法：如果 `relays` 为空且 `relay` 非空，将 `relay` 值加入 `relays`；如果 `relays` 非空，忽略 `relay`
5. `CliOverrides` 结构体：`relay` 字段改为 `relays: Vec<String>`，新增 `enable_mdns: Option<bool>`
6. `apply_cli_overrides()` 方法：处理 `relays` 和 `enable_mdns` 覆盖逻辑
7. `Default` 实现中 `relays` 默认为空 Vec，`enable_mdns` 默认为 true

**验收条件**:
- 旧格式 `relay = "/ip4/..."` 配置文件能正确解析，`resolve_relays()` 返回单元素列表
- 新格式 `relays = ["/ip4/...", "/ip4/..."]` 配置文件能正确解析
- 同时存在 `relay` 和 `relays` 时，`relays` 优先
- `--relay` 命令行参数可多次使用

---

### 任务 2：DeviceCam CLI 参数改造

**文件**: `p2p-camera/device-cam/src/main.rs`

**改动内容**:
1. `Opt` 结构体中 `relay` 字段改为 `relays: Vec<String>`（`#[arg(long = "relay")]`）
2. 新增 `enable_mdns: Option<bool>` 参数（`#[arg(long)]`）
3. 修改 `apply_cli_overrides` 调用，传入新的 `CliOverrides`
4. 修改配置校验逻辑：如果 `relays` 为空且 `enable_mdns` 为 false，报错退出
5. 修改 `validate_device_cam_config()` 函数，遍历 `relays` 列表校验每个地址

**验收条件**:
- `--relay addr1 --relay addr2` 命令行参数正确解析为 `relays` 列表
- `--enable-mdns false` 命令行参数正确覆盖配置文件
- 无 Relay 且 mDNS 禁用时启动报错

---

### 任务 3：Viewer CLI 配置系统改造

**文件**: `p2p-camera/mobile-core/examples/viewer_cli.rs`

**改动内容**:
1. `ViewerConfig` 结构体新增 `relays: Vec<String>` 字段
2. `ViewerConfig` 结构体新增 `enable_mdns: bool` 字段（默认 true）
3. 保留 `relay: String` 字段用于向后兼容
4. 新增 `resolve_relays()` 方法（同 DeviceCam 逻辑）
5. `Opt` 结构体中 `relay` 字段改为 `relays: Vec<String>`
6. 新增 `enable_mdns: Option<bool>` 参数
7. 修改命令行参数覆盖逻辑
8. 修改参数校验逻辑

**验收条件**:
- 旧格式 viewer.toml 能正确解析
- 新格式 viewer.toml 能正确解析
- `--relay` 可多次使用

---

## 阶段二：mDNS 集成

### 任务 4：DeviceCam Cargo.toml 新增 mdns feature

**文件**: `p2p-camera/device-cam/Cargo.toml`

**改动内容**:
1. libp2p features 列表中新增 `"mdns"`

**验收条件**:
- `cargo check` 编译通过

---

### 任务 5：DeviceCam Behaviour 新增 mDNS

**文件**: `p2p-camera/device-cam/src/behaviour.rs`

**改动内容**:
1. 新增 `use libp2p::mdns;` 导入
2. `Behaviour` 结构体新增 `pub mdns: mdns::tokio::Behaviour` 字段
3. `new()` 和 `new_with_identify_config()` 构造函数中初始化 mDNS：
   ```rust
   mdns: mdns::tokio::Behaviour::new(
       mdns::Config::default(),
       local_public_key.to_peer_id(),
   )?,
   ```
4. 构造函数返回值从 `Self` 改为 `Result<Self, mdns::tokio::Error>` 或使用 `anyhow::Result`

**验收条件**:
- DeviceCam 启动后 mDNS 自动广播
- 编译通过

---

### 任务 6：mobile-core Cargo.toml 新增 mdns feature

**文件**: `p2p-camera/mobile-core/Cargo.toml`

**改动内容**:
1. libp2p features 列表中新增 `"mdns"`

**验收条件**:
- `cargo check` 编译通过

---

### 任务 7：Viewer Behaviour 新增 mDNS（viewer.rs）

**文件**: `p2p-camera/mobile-core/src/viewer.rs`

**改动内容**:
1. 新增 `use libp2p::mdns;` 导入
2. `ViewerBehaviour` 结构体新增 `pub mdns: mdns::tokio::Behaviour` 字段
3. `new()` 和 `new_with_identify_config()` 构造函数中初始化 mDNS
4. 构造函数返回值类型调整

**验收条件**:
- 编译通过

---

### 任务 8：Viewer Behaviour 新增 mDNS（viewer_cli.rs）

**文件**: `p2p-camera/mobile-core/examples/viewer_cli.rs`

**改动内容**:
1. 新增 `use libp2p::mdns;` 导入
2. `ViewerBehaviour` 结构体新增 `pub mdns: mdns::tokio::Behaviour` 字段
3. 构造函数中初始化 mDNS
4. 构造函数返回值类型调整

**验收条件**:
- 编译通过

---

## 阶段三：DeviceCam 多路预约

### 任务 9：DeviceCam main.rs 多路预约逻辑

**文件**: `p2p-camera/device-cam/src/main.rs`

**改动内容**:
1. 新增 `RelayState` 结构体定义
2. 将原来的单个 `relay_addr`/`relay_peer_id`/`reservation_id`/`relay_connected`/`reconnect_attempt` 替换为 `Vec<RelayState>`
3. 初始化时同时拨号所有 Relay
4. 修改 `ConnectionEstablished` 事件处理：根据 `peer_id` 查找对应的 `RelayState`，更新状态并请求 Reservation
5. 修改 `ReservationReqAccepted` 事件处理：检查至少一个 Relay 预约成功
6. 修改 `ListenerClosed` 事件处理：根据 `listener_id` 查找对应的 `RelayState`，重新预约
7. 修改 `ConnectionClosed` 事件处理：根据 `peer_id` 查找对应的 `RelayState`，更新状态
8. 修改 `OutgoingConnectionError` 事件处理：根据 `peer_id` 查找对应的 `RelayState`，增加重连计数
9. 修改重连逻辑：每个 Relay 独立指数退避重连
10. 新增 mDNS 事件处理（`BehaviourEvent::Mdns`），DeviceCam 侧仅记录日志
11. 修改 `validate_device_cam_config()` 函数，遍历 `relays` 列表校验

**验收条件**:
- 配置多个 Relay 时，DeviceCam 同时连接并预约所有 Relay
- 至少一个 Relay 预约成功即进入可用状态
- 单个 Relay 断开不影响其他 Relay
- 每个 Relay 独立重连
- mDNS 广播正常工作

---

## 阶段四：Viewer mDNS 优先 + 多 Relay 并发拨号

### 任务 10：MediaPlayer::connect() 改造（viewer.rs）

**文件**: `p2p-camera/mobile-core/src/viewer.rs`

**改动内容**:
1. 修改 `connect()` 方法签名：`relay_addr: &str` → `relay_addrs: &[String]`，新增 `enable_mdns: bool` 参数
2. 新增 `ViewerConnectState` 结构体
3. 实现并行连接逻辑：同时向所有 Relay 发起连接
4. 实现 mDNS 优先策略：在事件循环中监听 `mdns::Event::Discovered`，发现目标 DeviceCam 后优先拨号
5. 实现 mDNS 超时降级：5 秒内未发现目标则降级为 Relay
6. 实现先到先用策略：使用最先成功的 Circuit 连接
7. 修改 `poll_swarm()` 方法：处理 mDNS 事件
8. 修改 `new()` 方法：适配 Behaviour 构造函数变更

**验收条件**:
- mDNS 发现目标 DeviceCam 后优先使用局域网直连
- mDNS 超时后降级为 Relay
- 多 Relay 并发拨号，先到先用
- 单 Relay 配置行为与原来一致

---

### 任务 11：viewer_cli.rs 连接逻辑改造

**文件**: `p2p-camera/mobile-core/examples/viewer_cli.rs`

**改动内容**:
1. 修改 `run_viewer_session()` 函数签名：`relay_addr_str: &str` → `relay_addrs: Vec<String>`，新增 `enable_mdns: bool` 参数
2. 修改 `spawn_session()` 函数签名：同步调整
3. 修改 `main()` 中的调用：传入 `relays` 列表和 `enable_mdns`
4. 实现并行连接逻辑：同时向所有 Relay 发起连接
5. 实现 mDNS 优先策略：在 `run_viewer_session()` 事件循环中监听 mDNS 事件
6. 实现 mDNS 超时降级
7. 实现先到先用策略
8. 修改 `wait_for_event_collecting()` 函数：支持多 Relay 连接等待
9. 修改主循环中的 `SessionEvent` 处理

**验收条件**:
- viewer_cli 支持多 Relay 并发拨号
- viewer_cli 支持 mDNS 优先连接
- 重连逻辑正常工作

---

## 阶段五：集成测试与验证

### 任务 12：编译验证与修复

**改动内容**:
1. 执行 `cargo check` 验证所有 crate 编译通过
2. 执行 `cargo build` 验证完整构建
3. 修复编译错误和警告

**验收条件**:
- `cargo check` 无错误
- `cargo build` 无错误

---

### 任务 13：配置文件向后兼容验证

**改动内容**:
1. 使用旧格式 `device-cam.toml`（只有 `relay = "..."`）启动 DeviceCam，验证正常工作
2. 使用旧格式 `viewer.toml`（只有 `relay = "..."`）启动 Viewer，验证正常工作
3. 使用新格式配置文件（`relays = [...]` + `enable_mdns = true`）验证正常工作

**验收条件**:
- 旧格式配置文件完全兼容
- 新格式配置文件正常工作

---

## 任务依赖关系

```
任务 1 (DeviceCam 配置) ──→ 任务 2 (DeviceCam CLI) ──→ 任务 9 (DeviceCam 多路预约)
                                                                    │
任务 3 (Viewer 配置)  ──→ 任务 11 (viewer_cli 连接逻辑)            │
                                                                    │
任务 4 (DeviceCam Cargo.toml) ──→ 任务 5 (DeviceCam Behaviour) ──→ │
                                                                    │
任务 6 (mobile-core Cargo.toml) ──→ 任务 7 (viewer.rs Behaviour) ──→ 任务 10 (MediaPlayer connect)
                  │                                                 │
                  └──→ 任务 8 (viewer_cli Behaviour) ──────────────→│
                                                                    │
任务 9 + 任务 10 + 任务 11 ──→ 任务 12 (编译验证) ──→ 任务 13 (兼容性验证)
```

**建议执行顺序**: 1 → 2 → 4 → 5 → 9 → 3 → 6 → 7 → 8 → 10 → 11 → 12 → 13
