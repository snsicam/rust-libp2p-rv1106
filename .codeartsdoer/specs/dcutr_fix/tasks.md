# 编码任务清单

## 任务 1: device-cam main.rs — NewListenAddr 事件中注入本地 QUIC 地址候选

**文件**: `p2p-camera/device-cam/src/main.rs`
**位置**: `NewListenAddr` 事件处理块（约第 360 行）
**状态**: ✅ 已完成

**实现要点**:
1. 检查地址是否包含 `/quic-v1` 协议
2. 排除包含 `/p2p-circuit` 的 relayed 地址
3. 从 `address` 中提取 IP（检查 `Protocol::Ip4`）
4. 使用黑名单策略：排除回环（`ip.is_loopback()`）和未指定地址（`ip.is_unspecified()`），其余均注入
5. 满足条件时调用 `swarm.add_external_address(address.clone())`
6. 输出 INFO 日志

**验收**: device-cam 启动后，本地 QUIC 地址（如 `/ip4/192.168.1.108/udp/34500/quic-v1`、`/ip4/172.32.0.93/udp/34500/quic-v1`）被添加为外部地址候选

**实际代码**:

```rust
SwarmEvent::NewListenAddr { address, .. } => {
    println!("[DeviceCam] Listening on: {address}");

    // 将本地 QUIC 地址注入为 DCUtR 候选地址
    // 同 NAT 场景下，本地地址可直接路由，解决 NAT hairpin 不支持的问题
    // 使用黑名单策略：排除回环和未指定地址，其余本地网卡地址均可作为候选
    // （包括 172.32.x.x 等 is_private() 不覆盖的 VPN/Docker 地址）
    let is_quic = address.iter().any(|p| matches!(p, Protocol::QuicV1));
    let is_relayed = address.iter().any(|p| matches!(p, Protocol::P2pCircuit));
    if is_quic && !is_relayed {
        if let Some(Protocol::Ip4(ip)) = address.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
            if !ip.is_loopback() && !ip.is_unspecified() {
                swarm.add_external_address(address.clone());
                tracing::info!("[DeviceCam] Added local address as DCUtR candidate: {address}");
            }
        }
    }
}
```

---

## 任务 2: viewer_cli.rs — 增加 NewListenAddr 事件处理并注入本地 QUIC 地址候选

**文件**: `p2p-camera/mobile-core/examples/viewer_cli.rs`
**位置**: 事件循环中（在 `SwarmEvent::ConnectionClosed` 之前）
**状态**: ✅ 已完成

**实现要点**:
1. 与任务 1 相同的注入逻辑（黑名单策略）
2. 日志前缀使用 `[Viewer]`
3. viewer_cli 原来没有 `NewListenAddr` 事件处理，新增 match 分支

**验收**: viewer 启动后，本地 QUIC 地址被添加为外部地址候选

**实际代码**:

```rust
SwarmEvent::NewListenAddr { address, .. } => {
    // 将本地 QUIC 地址注入为 DCUtR 候选地址
    // 同 NAT 场景下，本地地址可直接路由，解决 NAT hairpin 不支持的问题
    // 使用黑名单策略：排除回环和未指定地址，其余本地网卡地址均可作为候选
    // （包括 172.32.x.x 等 is_private() 不覆盖的 VPN/Docker 地址）
    let is_quic = address.iter().any(|p| matches!(p, Protocol::QuicV1));
    let is_relayed = address.iter().any(|p| matches!(p, Protocol::P2pCircuit));
    if is_quic && !is_relayed {
        if let Some(Protocol::Ip4(ip)) = address.iter().find(|p| matches!(p, Protocol::Ip4(_))) {
            if !ip.is_loopback() && !ip.is_unspecified() {
                swarm.add_external_address(address.clone());
                tracing::info!("[Viewer] Added local address as DCUtR candidate: {address}");
            }
        }
    }
}
```

---

## 任务 3: 编译验证

**状态**: ✅ 已完成
**结果**: `device-cam` 和 `mobile-core` 均编译通过，无新增 warning

---

## 任务 4: 功能验证（手动）

**操作**: 在同 NAT 环境下运行 device-cam + viewer，验证 DCUtR 直连是否成功

**验证步骤**:
1. 启动 relay server
2. 启动 device-cam（连接 relay）
3. 启动 viewer（连接 relay → circuit 拨号 device-cam）
4. 检查日志：
   - device-cam 日志应包含 "Added local address as DCUtR candidate"（包含 192.168.x.x 和 172.32.x.x 地址）
   - viewer 日志应包含 "Added local address as DCUtR candidate"
   - 如果 DCUtR 成功，日志应包含 "DCUtR direct connection established"
   - viewer 退出时应显示 "Direct connection (DCUtR): YES"
5. 如果 DCUtR 仍然失败，检查 DCUtR 失败日志中的候选地址是否包含本地 IP
