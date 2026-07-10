# 1. 实现模型

## 1.1 上下文视图

本次修改涉及 4 个文件，均位于 `p2p-camera/` 目录下，不修改 libp2p 核心库：

```
p2p-camera/
├── mobile-core/src/net_diag.rs           # NAT 诊断模块增强（4G 检测、诊断日志增强）
├── mobile-core/src/viewer.rs             # Viewer 核心逻辑（NAT 诊断集成、日志增强）
├── mobile-core/examples/viewer_cli.rs    # Viewer CLI（NAT 诊断集成、日志增强、Summary 增强）
└── device-cam/src/main.rs                # DeviceCam（NAT 诊断集成、日志增强）
```

## 1.2 服务/组件总体架构

修改分为 3 个独立模块：

```
┌──────────────────────────────────────────────────────────────┐
│                    修改模块总览                                 │
├──────────────────────────────────────────────────────────────┤
│                                                              │
│  模块 A: NAT 诊断增强（net_diag.rs）                          │
│  ├── 4G 网络启发式检测                                        │
│  ├── 诊断结果日志增强（evidence 输出）                          │
│  └── DCUtR 策略建议生成                                       │
│                                                              │
│  模块 B: NAT 诊断集成（viewer_cli.rs + device-cam main.rs）   │
│  ├── viewer_cli: 集成 NatDiagnostic，增强 Identify 事件处理    │
│  ├── device-cam: 集成 NatDiagnostic，增强 Identify 事件处理    │
│  ├── DCUtR 失败日志：包含 NAT 类型组合 + 针对性建议            │
│  └── viewer Summary: 包含 NAT 类型和连接类型                   │
│                                                              │
│  模块 C: viewer.rs 同步增强                                   │
│  ├── 集成 NatDiagnostic 的 4G 检测                            │
│  └── DCUtR 失败日志增强                                       │
│                                                              │
└──────────────────────────────────────────────────────────────┘
```

## 1.3 实现设计文档

### 模块 A: NAT 诊断增强（net_diag.rs）

**问题**：当前 `NatDiagnostic` 只通过端口映射一致性判断 NAT 类型，缺少 4G 网络检测和策略建议。

**方案**：

1. **4G 网络启发式检测**：在 `NatDiagnostic` 中增加本地 IP 网段检测。4G 网络通常分配 `192.168.174.x`、`100.64.x.x`（CGNAT 保留段）等网段。当检测到这些网段时，标记为可能的 4G/CGNAT 网络。

2. **NatDiagnosis 增加 4G 标记**：在 `NatDiagnosis` 结构体中增加 `is_4g: bool` 字段。

3. **DCUtR 策略建议**：在 `NatDiagnosis` 中增加 `dcutr_suggestion()` 方法，根据 NAT 类型和 4G 标记生成策略建议字符串。

4. **NatDiagnostic 增加本地 IP 记录**：在 `NatDiagnostic::new()` 时传入本地 IP 列表，或在 `record_observed` 时同时记录本地监听地址。

**关键设计决策**：

- **不实现"跳过 DCUtR"功能**：libp2p DCUtR 没有运行时禁用机制，一旦通过 relay 建立连接就自动触发打洞。无法在应用层跳过。因此 spec 5.2.1 规则 1（Symmetric NAT 跳过 DCUtR）**无法实现**，改为在日志中明确提示 Symmetric NAT 下 DCUtR 不可行，让用户了解预期。
- **4G 检测为启发式**：无法 100% 准确判断是否为 4G 网络，仅作为辅助信息。

**4G 网段检测逻辑**：

```
当本地 IP 满足以下任一条件时，标记为可能的 4G 网络：
1. 192.168.174.0/24 — Android WiFi 热点/USB 共享网络典型网段
2. 192.168.133.0/24 — iOS/Android 个人热点典型网段
3. 192.168.43.0/24 — Android WiFi 热点 (旧版) 典型网段
4. 100.64.0.0/10 — RFC 6598 CGNAT 保留段
5. 本地 IP 不在 RFC 1918 范围且不是公网 IP — 可能是运营商内网
```

**修改点**：

1. `NatDiagnostic` 增加 `local_ips: Vec<std::net::Ipv4Addr>` 字段
2. `NatDiagnostic::new(local_quic_port)` → `NatDiagnostic::new(local_quic_port, local_ips)`
3. `NatDiagnosis` 增加 `is_4g: bool` 和 `dcutr_suggestion: String` 字段
4. `NatDiagnostic::diagnose()` 中增加 4G 检测和策略建议生成
5. 新增 `is_4g_network(ip: Ipv4Addr) -> bool` 辅助函数（`pub` 可见性）
6. 新增 `DcutrPrediction` 结构体和 `dcutr_prediction()` 方法
7. 新增 `is_4g_network` 检测网段：192.168.133.0/24、192.168.43.0/24

### 模块 B: NAT 诊断集成（viewer_cli.rs + device-cam main.rs）

**问题**：viewer_cli 和 device-cam 的 Identify 事件处理中缺少 NAT 类型诊断集成，DCUtR 失败日志缺少 NAT 类型信息。

**方案**：

1. **viewer_cli.rs**：
   - 在 `run_viewer_session` 中创建 `NatDiagnostic` 实例
   - 在 Identify 事件中调用 `nat_diagnostic.record_observed()` 和 `nat_diagnostic.diagnose()`
   - 在 DCUtR 失败事件中输出 NAT 类型组合和策略建议
   - 在 Summary 中输出 NAT 类型和连接类型

2. **device-cam main.rs**：
   - 在 `run_device_cam_session` 中创建 `NatDiagnostic` 实例
   - 在 Identify 事件中调用 `nat_diagnostic.record_observed()` 和 `nat_diagnostic.diagnose()`
   - 在 DCUtR 失败事件中输出 NAT 类型和策略建议

**NatDiagnostic 创建时机**：

- 在 `NewListenAddr` 事件处理中收集本地 IP 列表
- 在首次 Identify 事件前创建 `NatDiagnostic`（使用收集到的本地 IP）
- 或者简化：在 swarm 构建后立即创建，使用 `0` 作为初始端口，在 Identify 事件后更新

**简化方案**：由于 `NatDiagnostic` 需要 `local_quic_port`，而端口在 `listen_on` 后才确定，采用延迟初始化：

```
1. 声明 let mut nat_diagnostic: Option<NatDiagnostic> = None;
2. 在 NewListenAddr 事件中，如果是 QUIC 地址，提取端口和 IP，创建 NatDiagnostic
3. 在 Identify 事件中，如果 nat_diagnostic 已创建，调用 record_observed 和 diagnose
```

**DCUtR 失败日志增强格式**：

```
[Viewer] DCUtR hole punch FAILED with {peer_id}: {error}
[Viewer] NAT diagnosis: local={nat_type}, remote={remote_nat_hint}
[Viewer] Suggestion: {dcutr_suggestion}
```

**remote NAT 类型推断**：通过 Identify 事件中获取的对端 listen_addrs 与 observed_addr 对比，推断对端 NAT 类型。具体方法：
- 对端 listen_addrs 中的 QUIC 端口与 observed_addr 中的 UDP 端口一致 → Cone NAT
- 不一致 → 可能 Symmetric NAT
- 无法获取 → Unknown

**viewer Summary 增强格式**：

```
[Viewer] === Summary ===
[Viewer] Local NAT: {nat_type}
[Viewer] Remote NAT: {remote_nat_hint}
[Viewer] Direct connection (DCUtR): YES/NO
[Viewer] Total frames: ...
```

### 模块 C: viewer.rs 同步增强

**问题**：`mobile-core/src/viewer.rs` 中的 `MediaPlayer` 已有 `NatDiagnostic` 集成，但缺少 4G 检测和策略建议。

**方案**：

1. 更新 `NatDiagnostic::new()` 调用，传入本地 IP 列表
2. 在 DCUtR 失败事件中输出策略建议
3. 在 Identify 事件中输出 4G 检测结果

# 2. 接口设计

## 2.1 总体设计

本次修改主要涉及内部逻辑增强，不新增公共 API。唯一的接口变更是 `NatDiagnostic::new()` 签名。

## 2.2 接口清单

| 修改项 | 文件 | 类型 | 说明 |
|--------|------|------|------|
| `NatDiagnostic::new()` | net_diag.rs | 签名变更 | 增加 `local_ips` 参数 |
| `NatDiagnosis` | net_diag.rs | 字段增加 | 增加 `is_4g`、`dcutr_suggestion` 字段 |
| `DcutrPrediction` | net_diag.rs | 结构体新增 | DCUtR 尝试前预测结果 |
| `NatDiagnostic::dcutr_prediction()` | net_diag.rs | 方法新增 | 返回 DCUtR 预测 |
| `is_4g_network()` | net_diag.rs | 函数新增 | 4G 网络启发式检测（`pub`） |
| `ViewerBehaviour::ping` | viewer.rs, viewer_cli.rs | 字段新增 | ping behaviour（5秒间隔），确保 Relay 连接保活 |

# 4. 数据模型

## 4.1 设计目标

本次修改不涉及持久化数据模型变更，仅修改运行时诊断数据结构。

## 4.2 模型实现

### NatDiagnosis 增强字段

```
NatDiagnosis {
    nat_type: NatType,              // 已有
    observed_addresses: Vec<Multiaddr>,  // 已有
    local_port: u16,                // 已有
    evidence: String,               // 已有
    dcutr_feasible: bool,           // 已有
    is_4g: bool,                    // 新增：是否为 4G 网络
    dcutr_suggestion: String,       // 新增：DCUtR 策略建议
}
```

### DcutrPrediction 新增结构体

```
DcutrPrediction {
    likely_success: bool,           // DCUtR 是否可能成功
    is_4g: bool,                    // 是否为 4G 网络
    nat_type: NatType,              // NAT 类型
    reason: String,                 // 预测原因说明
}
```

### NatDiagnostic 签名变更

```
// 旧签名
NatDiagnostic::new(local_quic_port: u16)

// 新签名
NatDiagnostic::new(local_quic_port: u16, local_ips: Vec<std::net::Ipv4Addr>)
```
