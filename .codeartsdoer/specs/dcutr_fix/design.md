# 1. 实现模型

## 1.1 上下文视图

本次修改涉及 2 个文件，均位于 `p2p-camera/` 目录下，不修改 libp2p 核心库：

```
p2p-camera/
├── device-cam/src/main.rs              # LAN 地址候选注入 + Identify 诊断增强
└── mobile-core/examples/viewer_cli.rs  # LAN 地址候选注入 + Identify 诊断增强
```

## 1.2 服务/组件总体架构

修改分为 2 个独立模块：

```
┌─────────────────────────────────────────────────┐
│              修改模块总览                          │
├─────────────────────────────────────────────────┤
│                                                 │
│  模块 A: LAN 地址候选注入（核心修复）              │
│  ├── viewer_cli.rs: NewListenAddr 事件处理       │
│  │   → 将 QUIC LAN 地址添加为外部地址候选         │
│  └── device-cam/src/main.rs: NewListenAddr 事件  │
│       → 将 QUIC LAN 地址添加为外部地址候选         │
│                                                 │
│  模块 B: Identify 诊断增强（已有，保留）           │
│  ├── viewer_cli.rs Identify 事件处理             │
│  └── device-cam/src/main.rs Identify 事件处理    │
│                                                 │
└─────────────────────────────────────────────────┘
```

## 1.3 实现设计文档

### 模块 A: LAN 地址候选注入（核心修复）

**问题**：当 cam 和 viewer 在同一 NAT 后面时，DCUtR 握手交换的候选地址只包含公网 IP（通过 identify observed_addr 翻译得到）。双方尝试向对方的公网 IP 发起 QUIC 连接时，需要路由器支持 NAT hairpin，而大多数路由器不支持此功能，导致 hole-punch 失败。

**根因分析**：

1. `swarm.listen_on("/ip4/0.0.0.0/udp/{port}/quic-v1")` 产生 `NewListenAddr` 事件，包含具体的网卡 IP（如 192.168.1.108）
2. 这些 LAN 地址被 identify 的 `listen_addresses` 收集，但**不会**自动成为 `NewExternalAddrCandidate`
3. identify 的 `emit_new_external_addr_candidate_event` 只在收到 observed_addr 时触发，且 `_address_translation` 将 observed IP 替换 listen IP，生成的是公网 IP 地址
4. DCUtR 的 `Candidates` 只收集 `NewExternalAddrCandidate` 事件报告的地址，因此 LAN 地址不在候选列表中
5. DCUtR 握手时双方只交换公网 IP，同 NAT 下无法通过公网 IP 互相到达

**方案**：在 `NewListenAddr` 事件处理中，将符合条件的 LAN QUIC 地址通过 `swarm.add_external_address()` 注入为外部地址候选。

**关键机制**：

- `swarm.add_external_address(addr)` 会触发 `ExternalAddrConfirmed` 事件
- `ExternalAddrConfirmed` 事件会被 DCUtR 的 `on_swarm_event` 处理，将地址加入 `Candidates`
- DCUtR handler 创建时通过 `self.observed_addresses()` 获取候选地址列表
- 候选地址在 DCUtR 握手中发送给对方

**注入条件**（黑名单策略）：

1. 地址必须包含 `/quic-v1` 协议后缀（TCP 地址不参与 QUIC hole-punch）
2. 排除 127.0.0.0/8 回环地址（`ip.is_loopback()`）
3. 排除 0.0.0.0 未指定地址（`ip.is_unspecified()`）
4. 排除包含 `/p2p-circuit` 的 relayed 地址
5. 其余所有本地网卡 IP 均作为候选注入（包括 192.168.x.x、172.32.x.x 等）

**为什么使用黑名单而非白名单**：

`172.32.0.93` 不在 RFC 1918 私有地址范围内（172.16.0.0/12 = 172.16.0.0 - 172.31.255.255），Rust 的 `Ipv4Addr::is_private()` 返回 `false`。但 `172.32.0.93` 实际上是 VPN/Docker 等内网地址，应作为 DCUtR 候选。使用黑名单策略（只排除回环和未指定）可以覆盖所有内网场景。

**修改点**：

1. `device-cam/src/main.rs` — `NewListenAddr` 事件处理（约第 360-362 行）
2. `viewer_cli.rs` — 事件循环中增加 `NewListenAddr` 事件处理

**实现逻辑**：

```
当收到 NewListenAddr 事件:
  1. 检查地址是否包含 /quic-v1
  2. 检查地址不包含 /p2p-circuit
  3. 从地址中提取 IP
  4. 检查 IP 非回环（!is_loopback）且非未指定（!is_unspecified）
  5. 如果满足条件:
     a. 调用 swarm.add_external_address(addr.clone())
     b. 输出 INFO 日志: "Added local address as DCUtR candidate: {addr}"
```

**为什么这个方案有效**：

- 同 NAT 场景：cam 和 viewer 的本地地址在同一子网，可以直接路由，DCUtR 尝试本地地址时能成功建立 QUIC 连接
- 跨 NAT 场景：本地地址不可路由，DCUtR 尝试失败后继续尝试公网地址，不影响最终结果
- 本地地址与公网地址共存于 Candidates 的 LruCache 中，DCUtR 会按频率排序尝试所有候选地址

**风险评估**：

- LAN 地址在跨 NAT 场景下不可达，但 DCUtR 会快速失败（连接拒绝或超时），不会显著增加延迟
- DCUtR Candidates LruCache 最多 20 个地址，LAN 地址不会挤掉公网地址
- `add_external_address` 是幂等操作，重复添加相同地址不会产生副作用

### 模块 B: Identify 诊断增强（已有，保留）

此模块已在之前的迭代中实现，包括：
- idle_connection_timeout 修复（0s → 120s）
- NAT 端口映射检测
- DCUtR 失败诊断建议

本次不需要修改此模块，保留现有实现。

# 2. 接口设计

## 2.1 总体设计

本次修改不涉及新增接口，仅修改现有事件处理逻辑。

## 2.2 接口清单

| 修改项 | 文件 | 类型 | 说明 |
|--------|------|------|------|
| NewListenAddr 事件处理 | device-cam/src/main.rs:360-362 | 逻辑增强 | 将 QUIC LAN 地址添加为外部地址候选 |
| NewListenAddr 事件处理 | viewer_cli.rs | 逻辑新增 | 增加 NewListenAddr 事件处理，将 QUIC LAN 地址添加为外部地址候选 |

# 4. 数据模型

## 4.1 设计目标

本次修改不涉及数据模型变更，仅修改运行时行为。

## 4.2 模型实现

无新增数据模型。LAN 地址候选通过 `swarm.add_external_address()` 注入，由 libp2p 内部的 `ExternalAddresses` 和 DCUtR `Candidates` 结构管理。
