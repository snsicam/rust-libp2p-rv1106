# DCUtR 与 STUN/TURN NAT 穿透方案对比

## 一、协议层级对比

| 维度 | DCUtR (libp2p) | STUN/TURN (WebRTC) |
|------|---------------|-------------------|
| 协议层级 | libp2p 连接层 | IP/UDP 层 (ICE 框架) |
| 地址发现 | Identify 协议观测地址 | STUN Binding Request/Response |
| 中继协商 | Circuit Relay v2 协议 | TURN Allocate + ChannelBind |
| 打洞方式 | QUIC/UDP dial_as_listener | ICE Connectivity Check (STUN Binding) |
| 传输协议 | QUIC (UDP) | UDP/TCP (ICE 候选) |
| 加密 | Noise (libp2p 内置) | DTLS-SRTP (WebRTC 内置) |
| 多路复用 | Yamux/QUIC stream | SCTP DataChannel 或 RTP session |

### DCUtR 工作原理

1. 双方通过 Relay Circuit 建立初始连接
2. Identify 协议交换观测地址（Relay Server 看到的公网地址）
3. DCUtR 协议触发直连升级：双方同时向对方观测地址发起 QUIC 连接（dial_as_listener）
4. 若 UDP 打洞成功 → 直连建立，Circuit 连接自动关闭
5. 若打洞失败 → 保持 Circuit 连接（等价于 TURN 中继）

### 局域网直连检测（同 NAT 场景补充）

DCUtR 的打洞地址来自 Identify 协议观测到的公网地址。当双方在同一 NAT 后面时：

- **NAT hairpin 问题**：NAT 不允许内部设备通过公网 IP 互访（即 hairpin/NAT loopback 不支持或不可靠）
- **DCUtR 打洞失败**：双方尝试向对方的公网地址打洞，但数据包被 NAT 丢弃
- **解决方案**：在 Identify 事件中检测对端 listen_addrs 是否有与本地 IP 同 /24 子网的 QUIC 私有地址，如果有则直接 dial 局域网地址

```
同 NAT 场景下的直连路径:

DCUtR 路径 (失败):
  Viewer (192.168.0.3) → NAT (183.23.149.209) → ✗ hairpin 不支持 → NAT (183.23.149.209) → DeviceCam (192.168.0.2)

LAN 直连检测路径 (成功):
  Viewer (192.168.0.3) ──── 192.168.0.2:45202/quic-v1 ────▶ DeviceCam (192.168.0.2)
  (直接使用对端 listen_addrs 中的局域网地址)
```

### STUN/TURN 工作原理

1. 客户端向 STUN 服务器发送 Binding Request，获取公网映射地址
2. 通过信令服务器交换 ICE 候选地址（host/server-reflexive/relay）
3. ICE 进行连通性检查：双方互相向候选地址发送 STUN Binding Request
4. 若直连检查成功 → 建立 P2P 连接
5. 若直连失败 → 通过 TURN 服务器中继转发

---

## 二、NAT 兼容性矩阵

| NAT 类型组合 | DCUtR (QUIC) | STUN (UDP) | 说明 |
|-------------|-------------|-----------|------|
| 同一 NAT (同子网) | ❌ DCUtR 失败 → ✅ LAN 直连检测 | ❌ STUN 失败 → 需 mDNS/本地候选 | NAT hairpin 不支持，DCUtR/STUN 均走公网地址失败；本项目通过 LAN 直连检测自动解决 |
| 锥型 + 锥型 | ✅ 成功 | ✅ 成功 | 双方映射端口可预测，打洞包可命中 |
| 锥型 + 端口受限锥型 | ✅ 成功 | ✅ 成功 | 映射端口固定，打洞可命中 |
| 锥型 + 对称型 | ⚠️ 条件成功 | ⚠️ 条件成功 | 锥型侧可预测，对称型侧端口不可预测；需锥型侧先发包"打洞" |
| 对称型 + 对称型 | ❌ 失败 | ❌ 失败 | 双方映射端口均不可预测，无法命中 |
| 锥型 + 严格防火墙 | ❌ 失败 | ❌ 失败 | 防火墙拦截入站 UDP，打洞包无法到达 |
| 对称型 + 防火墙 | ❌ 失败 | ❌ 失败 | 双重障碍：端口不可预测 + 入站被阻 |

### NAT 类型说明

| NAT 类型 | 映射行为 | 端口可预测性 | DCUtR 可行性 |
|---------|---------|------------|-------------|
| 全锥型 (Full Cone) | 同一内部 IP:Port → 固定外部映射 | ✅ 高 | ✅ 可行 |
| 受限锥型 (Restricted Cone) | 同一内部 IP:Port → 固定外部映射，限 IP | ✅ 高 | ✅ 可行 |
| 端口受限锥型 (Port Restricted Cone) | 同一内部 IP:Port → 固定外部映射，限 IP:Port | ✅ 高 | ✅ 可行 |
| 对称型 (Symmetric) | 不同目标 → 不同外部映射端口 | ❌ 低 | ❌ 不可行 |

---

## 三、适用场景推荐

### 本项目选择 DCUtR 的原因

1. **原生 libp2p 集成**：DCUtR 是 libp2p 协议栈的一部分，无需额外集成
2. **QUIC 打洞**：基于 QUIC (UDP) 的打洞成功率高于 TCP
3. **无需额外服务部署**：Relay Server 同时承担信令和中继角色
4. **自动降级**：打洞失败自动降级为 Circuit 中继，无需手动切换
5. **连接级转发**：Relay Circuit 转发 libp2p 连接流，比 IP 包转发更高效

### STUN/TURN 不适用于本项目的原因

1. **为 WebRTC 设计**：STUN/TURN 是 ICE 框架的一部分，与 WebRTC 的 RTP/RTCP 协议栈绑定
2. **需额外部署 coturn**：传统 TURN 服务器 (coturn) 需要单独部署和运维
3. **对 QUIC 无优势**：STUN/TURN 工作在 UDP/TCP 层，对 QUIC 连接无原生支持
4. **信令复杂度高**：ICE 候选地址收集和交换需要额外的信令协议

---

## 四、降级策略等价性

| 功能 | libp2p Relay Circuit | 传统 TURN |
|------|---------------------|----------|
| 中继转发 | ✅ 连接级流转发 | ✅ IP 包转发 |
| 触发条件 | DCUtR 打洞失败 | ICE 连通性检查失败 |
| 部署方式 | 单个 Relay Server 进程 | 需要单独部署 coturn |
| 带宽消耗 | 中继服务器带宽 | TURN 服务器带宽 |
| 延迟 | +50ms (额外一跳) | +50ms (额外一跳) |
| 协议支持 | QUIC / TCP | UDP / TCP |

**结论**：libp2p Relay Circuit 在功能上等价于 TURN，本项目无需额外部署 TURN 服务器。

---

## 五、云服务器安全组配置要求

### QUIC 打洞所需端口

| 方向 | 协议 | 端口范围 | 说明 |
|------|------|---------|------|
| 入站 | UDP | 4001 | Relay Server QUIC 监听端口 |
| 入站 | TCP | 4001 | Relay Server TCP 监听端口 |
| 入站 | UDP | 动态 (需开放) | Viewer/DeviceCam QUIC 打洞临时端口 |

### 最佳实践

1. **固定 UDP 端口**：DeviceCam 和 Viewer 使用 `--udp-port` 指定固定端口，便于端口映射
2. **安全组配置**：在云服务器安全组中开放 UDP 4001 入站，以及 DeviceCam/Viewer 指定的 UDP 端口
3. **--external-ip**：在云服务器上必须指定 `--external-ip <公网IP>`，因为 `hostname -I` 可能返回内网 IP
4. **最小化端口开放**：仅开放必要的 UDP 端口，避免全端口开放

### 腾讯云/阿里云配置示例

```bash
# 腾讯云安全组规则
# 入站规则:
#   UDP:4001  允许 0.0.0.0/0  (Relay QUIC)
#   TCP:4001  允许 0.0.0.0/0  (Relay TCP)
#   UDP:<viewer_udp_port>  允许 0.0.0.0/0  (Viewer QUIC 打洞, 可选)
```
