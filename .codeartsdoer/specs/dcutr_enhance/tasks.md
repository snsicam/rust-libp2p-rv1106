# 编码任务清单

## 任务 1: net_diag.rs — 增加 4G 网络检测和策略建议

**文件**: `p2p-camera/mobile-core/src/net_diag.rs`, `p2p-camera/device-cam/src/net_diag.rs`
**状态**: ✅ 已完成

**实现要点**:

1. 新增 `is_4g_network(ip: std::net::Ipv4Addr) -> bool` 函数，检测 4G 典型网段：
   - `192.168.174.0/24` — Android 热点/USB 共享典型网段
   - `192.168.133.0/24` — iOS/Android 个人热点典型网段
   - `192.168.43.0/24` — Android WiFi 热点 (旧版) 典型网段
   - `100.64.0.0/10` — RFC 6598 CGNAT 保留段
   - 非 RFC 1918 私有地址且非公网 IP — 可能运营商内网

2. `NatDiagnostic` 增加 `local_ips: Vec<std::net::Ipv4Addr>` 字段

3. `NatDiagnostic::new()` 签名变更为 `new(local_quic_port: u16, local_ips: Vec<std::net::Ipv4Addr>)`

4. `NatDiagnosis` 增加字段：
   - `is_4g: bool`
   - `dcutr_suggestion: String`

5. `NatDiagnostic::diagnose()` 中增加 4G 检测逻辑：
   - 遍历 `local_ips`，任一匹配 `is_4g_network()` 则 `is_4g = true`
   - 根据 NAT 类型和 4G 标记生成 `dcutr_suggestion`

6. `dcutr_suggestion` 生成逻辑：
   - Symmetric NAT → "Symmetric NAT detected: DCUtR hole-punching will not succeed, relay circuit is the only option"
   - 4G + Symmetric → "4G/CGNAT with Symmetric NAT: DCUtR will not succeed. Placing device-cam on broadband network increases DCUtR success rate"
   - 4G + Cone → "4G/CGNAT detected: DCUtR may succeed if remote peer is on broadband (Cone NAT). Placing device-cam on broadband increases success rate"
   - Cone NAT → "Cone NAT detected: DCUtR hole-punching should succeed"
   - Unknown → "NAT type unknown: DCUtR will be attempted, success depends on NAT compatibility"

7. 新增 `DcutrPrediction` 结构体和 `dcutr_prediction()` 方法：
   - 在 DCUtR 尝试前输出 NAT 上下文预测
   - 包含 `likely_success: bool`、`is_4g: bool`、`nat_type: NatType`、`reason: String`

8. `is_4g_network` 改为 `pub` 以便其他模块使用

**验收**: 
- `NatDiagnostic::new(34935, vec!["192.168.174.21".parse().unwrap()])` 创建成功
- `diagnose()` 返回 `is_4g = true`，`dcutr_suggestion` 包含 "4G/CGNAT"
- `NatDiagnostic::new(34935, vec!["192.168.133.225".parse().unwrap()])` 创建成功
- `diagnose()` 返回 `is_4g = true`（新增 192.168.133.0/24 网段）
- `dcutr_prediction()` 返回合理的预测结果

---

## 任务 2: viewer_cli.rs — 集成 NatDiagnostic 和日志增强

**文件**: `p2p-camera/mobile-core/examples/viewer_cli.rs`
**状态**: ✅ 已完成

**实现要点**:

1. 在 `run_viewer_session` 中声明 `let mut nat_diagnostic: Option<NatDiagnostic> = None;` 和 `let mut local_nat_type: Option<NatType> = None;`

2. 在 `SwarmEvent::NewListenAddr` 事件处理中：
   - 收集本地 QUIC 端口和 IP
   - 如果 `nat_diagnostic` 为 None 且有 QUIC 端口，创建 `NatDiagnostic::new(port, local_ips)`
   - 保留现有的 `add_external_address` 逻辑不变

3. 在 `SwarmEvent::Behaviour(ViewerBehaviourEvent::Identify(...))` 事件处理中：
   - 调用 `nat_diagnostic.record_observed(&info.observed_addr)`（如果已创建）
   - 调用 `nat_diagnostic.diagnose()` 获取诊断结果
   - 输出 NAT 类型、4G 检测结果、策略建议
   - 保存 `local_nat_type = Some(diag.nat_type)`

4. 在 `SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(Event { result: Err(...), ... }))` 事件处理中：
   - 输出 NAT 类型组合：`local NAT={local_nat_type}, remote NAT={remote_nat_hint}`
   - 输出策略建议：`diag.dcutr_suggestion`
   - remote NAT 推断：通过 Identify 事件中获取的对端 listen_addrs 与 observed_addr 对比

5. 在 viewer 退出 Summary 中增加：
   - `Local NAT: {nat_type}`
   - `Remote NAT: {remote_nat_hint}`

6. 需要增加状态变量来跟踪：
   - `local_nat_type: Option<NatType>` — 本地 NAT 类型
   - `remote_nat_hint: Option<String>` — 对端 NAT 类型推断
   - `local_ips: Vec<std::net::Ipv4Addr>` — 本地 IP 列表
   - `local_quic_port: u16` — 本地 QUIC 端口

7. **新增**：circuit 连接建立后输出 DCUtR 预测（`dcutr_prediction()`）

8. **新增**：DCUtR 失败后输出 Relay Circuit 降级确认

9. **新增**：`ViewerBehaviour` 添加 `ping` behaviour（5秒间隔），确保 Relay 连接保活

**验收**:
- viewer 启动后日志包含 NAT 类型诊断结果
- DCUtR 失败日志包含 NAT 类型组合和策略建议
- viewer 退出 Summary 包含 NAT 类型信息
- circuit 连接建立后日志包含 DCUtR 预测
- DCUtR 失败后日志包含 Relay Circuit 降级确认

---

## 任务 3: device-cam main.rs — 集成 NatDiagnostic 和日志增强

**文件**: `p2p-camera/device-cam/src/main.rs`
**状态**: ✅ 已完成

**实现要点**:

1. 在 `run_device_cam_session` 中声明 `let mut nat_diagnostic: Option<NatDiagnostic> = None;` 和 `let mut local_nat_type: Option<NatType> = None;`

2. 在 `SwarmEvent::NewListenAddr` 事件处理中：
   - 收集本地 QUIC 端口和 IP
   - 如果 `nat_diagnostic` 为 None 且有 QUIC 端口，创建 `NatDiagnostic::new(port, local_ips)`
   - 保留现有的 `add_external_address` 逻辑不变

3. 在 `SwarmEvent::Behaviour(behaviour::BehaviourEvent::Identify(...))` 事件处理中：
   - 调用 `nat_diagnostic.record_observed(&info.observed_addr)`（如果已创建）
   - 调用 `nat_diagnostic.diagnose()` 获取诊断结果
   - 输出 NAT 类型、4G 检测结果、策略建议
   - 保存 `local_nat_type = Some(diag.nat_type)`

4. 在 `SwarmEvent::Behaviour(behaviour::BehaviourEvent::Dcutr(Event { result: Err(...), ... }))` 事件处理中：
   - 输出 NAT 类型组合和策略建议

5. `net_diag.rs` 复制到 `device-cam/src/net_diag.rs` 并在 `main.rs` 中 `mod net_diag;`

6. **新增**：circuit 连接建立后输出 DCUtR 预测（`dcutr_prediction()`）

7. **新增**：DCUtR 失败后输出 Relay Circuit 降级确认

**验收**:
- device-cam 启动后日志包含 NAT 类型诊断结果
- DCUtR 失败日志包含 NAT 类型和策略建议
- 4G 网络下日志包含 "4G/CGNAT detected" 提示
- circuit 连接建立后日志包含 DCUtR 预测
- DCUtR 失败后日志包含 Relay Circuit 降级确认

---

## 任务 4: viewer.rs — 适配 NatDiagnostic 新签名和日志增强

**文件**: `p2p-camera/mobile-core/src/viewer.rs`
**状态**: ✅ 已完成

**实现要点**:

1. `MediaPlayer::new()` 中 `NatDiagnostic::new(0)` 改为 `NatDiagnostic::new(0, Vec::new())`（初始无 IP 信息，后续通过 Identify 事件补充）

2. 在 `SwarmEvent::Behaviour(ViewerBehaviourEvent::Dcutr(Event { result: Err(...), ... }))` 事件处理中：
   - 获取 `nat_diagnostic.diagnose()` 的策略建议
   - 输出策略建议日志
   - **新增**：DCUtR 失败后输出 Relay Circuit 降级确认

3. 在 Identify 事件处理中：
   - 输出 4G 检测结果（如果 `diag.is_4g`）

4. **新增**：circuit 连接建立后输出 DCUtR 预测（`dcutr_prediction()`）

5. **新增**：`ViewerBehaviour` 添加 `ping` behaviour（5秒间隔），确保 Relay 连接保活

**验收**:
- 编译通过
- DCUtR 失败日志包含策略建议
- circuit 连接建立后日志包含 DCUtR 预测
- DCUtR 失败后日志包含 Relay Circuit 降级确认

---

## 任务 5: 编译验证

**状态**: ✅ 已完成

**操作**:
1. 编译 `mobile-core`：`cargo check -p mobile-core`
2. 编译 `device-cam`：`cargo check -p device-cam`
3. 编译 `viewer_cli`：`cargo check -p mobile-core --example viewer_cli`
4. 编译 `relay-server`：`cargo check -p relay-server`

**验收**: 所有目标编译通过，无 error

---

## 任务 6: 功能验证（手动）

**状态**: 待验证

**操作**: 在 cam=宽带, viewer=4G 环境下运行 device-cam + viewer，验证 NAT 诊断和日志增强

**验证步骤**:
1. 启动 relay server
2. 启动 device-cam（连接宽带，连接 relay）
3. 启动 viewer（连接 4G，连接 relay → circuit 拨号 device-cam）
4. 检查日志：
   - device-cam 日志应包含 NAT 类型诊断结果（Cone NAT）
   - viewer 日志应包含 NAT 类型诊断结果（Symmetric NAT / 4G CGNAT）
   - circuit 连接建立后日志应包含 DCUtR 预测
   - DCUtR 失败日志应包含 NAT 类型组合和策略建议
   - DCUtR 失败后日志应包含 Relay Circuit 降级确认
   - viewer 退出 Summary 应包含 NAT 类型信息
5. 反向测试：cam=4G, viewer=宽带，验证日志输出
