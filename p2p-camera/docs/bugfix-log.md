# P2P-Camera Bug 修复记录

> 本文件记录开发过程中遇到的 bug 及修复方案，便于后续排查和团队共享。
> 新增记录请追加到表格末尾。

| # | 日期 | 模块 | 问题现象 | 根因 | 修复方案 | 涉及文件 |
|---|------|------|---------|------|---------|---------|
| 1 | 2026-06-24 | 构建 (RV1106) | `build_rv1106.sh` 报 "Cross compiler 'armv7l-linux-gnueabihf-gcc' not found" | 系统只有 `arm-linux-gnueabihf-gcc`，无 `armv7l-` 前缀的链接 | 创建符号链接 `/usr/local/bin/armv7l-linux-gnueabihf-gcc` → `/usr/bin/arm-linux-gnueabihf-gcc` | `scripts/build_rv1106.sh` |
| 2 | 2026-06-24 | 构建 (RV1106) | 链接报 "linker `arm-rockchip830-linux-uclibcgnueabihf-gcc` not found" | `.cargo/config.toml` 硬编码了 Rockchip 工具链链接器名，且 TOOLCHAIN_DIR 路径是绝对路径 | (1) TOOLCHAIN_DIR 改为项目相对路径 `$PROJECT_ROOT/toolchain/...`<br>(2) `.cargo/config.toml` 链接器改为通用的 `armv7l-linux-gnueabihf-gcc`<br>(3) rv1106 模式用 `CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER` 环境变量覆盖为 Rockchip 工具链 | `.cargo/config.toml`, `device-cam/.cargo/config.toml`, `examples/ping/.cargo/config.toml`, `scripts/build_rv1106.sh` |
| 3 | 2026-06-24 | DeviceCam/Viewer | Viewer 连接 ~3s 后断开，报 "DeviceCam connection closed" 后重连，出现 HEVC "Duplicate POC" 错误 | yamux 无 keepalive，NAT/路由器丢弃空闲 TCP 连接（尽管 libp2p 层 ping 正常） | 见 #4，yamux keepalive 方案因 API 变更失败，最终由 #6 心跳设计 + #7 DCUtR 误判修复解决 | `device-cam/src/main.rs`, `mobile-core/src/viewer.rs`, `mobile-core/examples/media_viewer.rs` |
| 4 | 2026-06-25 | DeviceCam/Viewer | 编译报 `no method named set_keep_alive_interval / set_max_buffer_size / set_receive_window found for libp2p_yamux::Config` 及 `expected FnOnce() closure, found Config` | libp2p 0.54 的 yamux (v0.14) Config 已移除这些方法；`libp2p_yamux::Config` 是薄封装仅暴露 `set_max_num_streams`；`with_tcp`/`with_relay_client` 需要闭包 `FnOnce() -> Config` 而非实例 | 改用 `libp2p::yamux::Config::default` 函数指针（闭包形式）传给 builder，移除不存在的配置方法调用 | `device-cam/src/main.rs`, `mobile-core/src/viewer.rs`, `mobile-core/examples/media_viewer.rs` |
| 5 | 2026-06-25 | 脚本 | 所有脚本默认 `--release` 编译，调试不便 | 历史默认配置 | `start_relay.sh` 拆分 build/run 子命令；全部脚本 `--release` → debug，路径 `target/release/` → `target/debug/` | `scripts/start_relay.sh`, `scripts/start_server.sh`, `scripts/play_viewer.sh`, `scripts/build_rv1106.sh`, `scripts/build.sh` |
| 6 | 2026-06-25 | DeviceCam/Viewer | Viewer 在 idle_connection_timeout 后断开重连，且重连循环不断 | 心跳配置不合理：Viewer 不应有心跳和 idle timeout（视频流本身维持连接活跃） | **DeviceCam**: 保留 ping (5s) + idle_connection_timeout=120s（与 Relay 维持心跳）<br>**Viewer**: 移除 ping Behaviour，idle_connection_timeout=0（禁用，由 stream 层 read 返回 0/error 检测断开） | `device-cam/src/behaviour.rs`, `mobile-core/src/viewer.rs`, `mobile-core/examples/media_viewer.rs` | ✅ 已验证 |
| 7 | 2026-06-25 | Viewer (CLI) | DCUtR 直连升级后立即报 "DeviceCam connection closed" 触发重连，但直连实际还在 | DCUtR 打洞成功后 libp2p 自动关闭冗余的 circuit relay 连接，触发 `ConnectionClosed`；旧代码不看剩余连接数直接判定断开 | `ConnectionClosed` 分支检查 `num_established` 字段：`==0` 才真正断开触发重连；`>0` 说明只是 circuit 关闭、直连仍在，继续运行 | `mobile-core/examples/media_viewer.rs` | ✅ 已验证 |
| 8 | 2026-06-26 | 构建 (RV1106) | `build_rv1106.sh` 报 "rockit include dir not found"，SDK 路径 `/home/song/samba/work/rv1106/lubancat` 不存在 | SDK_ROOT 默认值是开发者本地绝对路径，不通用 | 改为 `$PROJECT_ROOT/../../rv1106/RV1106_Linux_SDK`（相对项目根目录） | `scripts/build_rv1106.sh` | ✅ 已验证 |
| 9 | 2026-06-26 | 部署 (云服务器) | DeviceCam 连接云服务器 Relay 报 "Handshake timed out" (QUIC) 或 "Timeout has been reached" (TCP) | (1) 云服务器安全组未放行 UDP/TCP 4001 入站；(2) start_relay.sh 用 `hostname -I` 取到内网 IP，打印的连接地址错误 | (1) 腾讯云安全组添加 TCP+UDP 4001 入站规则；(2) start_relay.sh 新增 `--public-ip` 参数指定外网 IP | `scripts/start_relay.sh` | ✅ 已验证 |
| 10 | 2026-07-01 | Viewer/DeviceCam | 同一局域网 (192.168.0.2/0.3) 下 DCUtR 打洞失败，日志显示 "Direct connection (DCUtR): NO (relay circuit)"，但视频流畅低延时 | (1) DCUtR 只尝试公网地址打洞 (183.23.149.209)，NAT hairpin 不允许内部设备通过公网 IP 互访；(2) `wait_for_event` 函数在等待连接期间吞掉了 Identify 和 NewListenAddr 事件，导致局域网检测代码无法执行 | (1) 新增局域网直连检测：Identify 事件中检查对端 listen_addrs 是否有同 /24 子网的 QUIC 私有地址，有则直接 dial；(2) `wait_for_event` 替换为 `wait_for_event_collecting`，在等待期间收集 local_ips 和缓存 Identify 事件；(3) 主循环使用 event_queue 优先处理缓存事件；(4) `ConnectionType` 新增 `LanDirect` 变体区分局域网直连和 DCUtR 打洞 | `mobile-core/examples/media_viewer.rs`, `device-cam/src/main.rs`, `mobile-core/src/viewer.rs`, `mobile-core/src/net_diag.rs` |
| 11 | 2026-07-01 | 全部 | 三个软件参数只能通过命令行传入，RV1106 嵌入式设备上不便操作 | 无配置文件支持 | 三个软件均新增 TOML 配置文件支持：首次运行自动生成默认配置文件，编辑后重启即可；命令行参数可覆盖配置文件值；优先级：命令行 > 配置文件 > 默认值 | `device-cam/src/config.rs`, `relay-server/src/config.rs`, `mobile-core/examples/media_viewer.rs` |
| 12 | 2026-07-01 | Viewer/DeviceCam | 4G CGNAT + 宽带 NAT 场景下 DCUtR 打洞必然失败，但用户无法提前知道原因 | (1) DCUtR 在 circuit 连接建立后自动触发，无法跳过；(2) 4G CGNAT 不允许外部入站 UDP，打洞必然失败；(3) Viewer 端无 ping，DCUtR 尝试期间 Relay 连接可能因空闲断开 | (1) 新增 `DcutrPrediction` 和 `dcutr_prediction()` 方法，在 circuit 连接建立后立即输出 DCUtR 预测（likely SUCCESS/FAIL + 原因）；(2) 4G 检测新增 192.168.133.0/24（iOS/Android 个人热点）、192.168.43.0/24（Android 旧版热点）网段；(3) DCUtR 失败后输出 Relay Circuit 降级确认（"Fallback: Relay circuit is still active"）；(4) Viewer 端 `ViewerBehaviour` 新增 `ping` behaviour（5秒间隔），确保 Relay 连接在 DCUtR 尝试期间和空闲时不会断开 | `mobile-core/src/net_diag.rs`, `device-cam/src/net_diag.rs`, `mobile-core/src/viewer.rs`, `mobile-core/examples/media_viewer.rs`, `device-cam/src/main.rs` |
| 13 | 2026-07-02 | Viewer (CLI+mobile-core) | Viewer 连接循环永不退出，所有 Relay 连接失败且 mDNS 超时后程序卡死 | `all_relay_errors` 变量始终为 `false`，从未被更新，导致退出条件 `all_relay_errors && mdns_expired` 永远不满足 | 将 `all_relay_errors` 替换为 `relay_error_count` 计数器，在 `OutgoingConnectionError` 中对 Relay 连接递增，退出条件改为 `relay_error_count >= total_relays && mdns_expired` | `mobile-core/examples/media_viewer.rs`, `mobile-core/src/viewer.rs` |
| 14 | 2026-07-02 | DeviceCam | Relay 重连时重复拨号，日志中 "Dialing relay (attempt 2)" 重复打印 4 次；且 `reconnect_attempt` 始终为 1，指数退避未生效 | (1) 上一轮修复中 dial 成功后将 `reconnect_attempt` 设为 0，`OutgoingConnectionError` 中 `+=1` 变回 1，导致 `reconnect_attempt` 永远在 0→1 之间循环，指数退避始终用 base delay；(2) 缺少"已发起拨号等待结果"的状态，无法区分"需要重连"和"已拨号等待中" | 新增 `dial_pending: bool` 字段：dial 成功后设为 `true`（而非重置 `reconnect_attempt`），`OutgoingConnectionError`/`ConnectionClosed`/`ConnectionEstablished` 中设为 `false`；重连循环条件增加 `!state.dial_pending`，确保 `reconnect_attempt` 持续递增，指数退避正常生效 | `device-cam/src/main.rs` |
| 15 | 2026-07-02 | Relay Server | Relay 服务器崩溃：`assertion left == right failed (None vs Active)` | Viewer 发起 circuit 请求时，Relay 找到 DeviceCam 连接但 reservation 状态为 `None`（尚未完成或已失效），原代码用 `assert_eq!` 断言状态必须为 `Active`，导致 panic | 将 `assert_eq!` 替换为条件判断：`Active` 则接受 circuit 请求，否则返回 `NoReservation` 拒绝请求，避免 Relay 崩溃 | `protocols/relay/src/behaviour.rs` |
| 16 | 2026-07-03 | 协议清理 | — | `VIDEO_PROTOCOL` (`/p2p-camera/video/1.0.0`) 是为旧版单码流设计的向后兼容协议，现在已有 `VIDEO_MAIN/SUB/THIRD_PROTOCOL` 三个明确协议，旧协议不再需要 | 删除 `stream_protocols.rs` 中 `VIDEO_PROTOCOL` 常量定义；删除 `main.rs` 中 `incoming_legacy_video` accept 及 `select!` 中 `legacy_video` 分支 | `proto/src/stream_protocols.rs`, `device-cam/src/main.rs` |
| 17 | 2026-07-03 | 音频编码 | 音频仅支持原始 PCM 采集，无硬件编码，带宽占用大 | 未使用 RK AENC 硬件编码器 | 新增 G711A/G711U/MP2 HW 编码支持：<br>(1) `config.rs` AudioConfig 新增 `encode_type`/`format`/`bit_rate`/`enable_vqe`/`vqe_cfg` 字段<br>(2) `rk_camera.c` 新增 AENC 管线：AI→AENC bind + `aenc_get_stream_thread`<br>(3) `media_packet.rs` 新增 `audio_g711a()`/`audio_g711u()` 构造函数 (flags=2/3)<br>(4) `device-cam.toml` 新增 `[audio]` 编码配置段 | `device-cam/src/config.rs`, `device-cam/src/rk_camera.c`, `device-cam/src/rk_video_source.rs`, `proto/src/media_packet.rs`, `device-cam.toml` |
| 18 | 2026-07-10 | Viewer (mobile-core) | 连接断开后无任何通知，Android 侧无法感知断连，也不会自动重连 | (1) `receive_frames` EOF 时静默退出，无通知<br>(2) `ConnectionClosed` 只更新状态，不通知上层<br>(3) `ViewerEvent::Disconnected` 枚举已定义但从未发送<br>(4) 无重连机制 | 新增断连检测 + 自动重连：<br>(1) `MediaPlayerEvent` 枚举：`Disconnected`/`DirectUpgraded`<br>(2) `MediaPlayer` 新增 `event_sender`/`event_receiver` 内部事件通道<br>(3) `receive_frames` EOF/错误时发送 `MediaPlayerEvent::Disconnected`<br>(4) `ConnectionClosed (num_established==0)` 时发送 `Disconnected`<br>(5) 新增 `poll_event()` 非阻塞轮询内部事件<br>(6) 新增 `reconnect()` 方法：3s 延迟 → abort 旧任务 → 清空 jitter → 重新 connect<br>(7) `jni_bridge.rs` 主循环检测 `Disconnected` → 发送 `ViewerEvent::Disconnected` → 自动 `reconnect()`<br>(8) `AvJitterBuffer` 新增 `clear()` 方法 | `mobile-core/src/viewer.rs`, `mobile-core/src/jni_bridge.rs`, `mobile-core/src/jitter_buffer.rs`, `mobile-core/src/lib.rs` |

## 关键设计决策

### 心跳设计原则（2026-06-25 确定，2026-07-01 修订）
- **DeviceCam 需要 ping**：5s 间隔，与 Relay Server 维持心跳，`idle_connection_timeout=120s` 作为保底
- **Viewer 需要 ping**（2026-07-01 修订）：5s 间隔，确保 Relay 连接在 DCUtR 尝试期间和视频流空闲时不会断开
  - 之前认为 Viewer 不需要 ping（视频流本身维持连接活跃），但实际场景中 DCUtR 尝试期间可能无视频流传输
  - ping 的 `connection_keep_alive()` 返回 true，与 relay client 的 keep_alive 叠加，确保连接不被 idle timeout 关闭
- **Relay Server 需要 ping**：15s 间隔，检测客户端连接存活

### yamux Config API 注意事项（libp2p 0.54 / yamux 0.14）
- `libp2p_yamux::Config` 是 `yamux::Config` 的薄封装，仅暴露 `set_max_num_streams`
- **不存在** `set_keep_alive_interval` / `set_max_buffer_size` / `set_receive_window`（这些是旧版 API）
- `SwarmBuilder::with_tcp` / `with_relay_client` 的 multiplexer 参数需要 `FnOnce() -> Config` 闭包，传 `libp2p::yamux::Config::default` 函数指针即可，**不能传 Config 实例**

### DCUtR 直连升级注意事项
- DCUtR 打洞成功后，libp2p 会自动关闭冗余的 circuit relay 连接
- 处理 `SwarmEvent::ConnectionClosed` 时**必须检查 `num_established` 字段**：
  - `== 0`：所有连接已断开，触发重连
  - `> 0`：仍有其他连接（如直连），不要误判为断开

### 局域网直连检测注意事项（2026-07-01 新增）
- DCUtR 只能通过公网地址打洞，同一 NAT 下的设备因 hairpin 问题无法通过 DCUtR 直连
- 局域网直连检测通过 Identify 协议交换的 listen_addrs 实现，检查对端是否有同 /24 子网的 QUIC 私有地址
- `wait_for_event` / `wait_for_event_collecting` 在等待连接期间会消费 swarm 事件，**必须**：
  - 收集 `NewListenAddr` 事件中的 `local_ips`，否则子网比较无数据
  - 缓存 `Identify` 事件到 `pending_events`，否则主循环收不到对端地址信息
- 直连升级逻辑统一在 `ConnectionEstablished` 事件中处理（而非 DCUtR 事件），通过远程地址是否为私有 IP 区分 LAN 直连和 DCUtR 打洞

### 配置文件设计原则（2026-07-01 新增）
- 三个软件均使用 TOML 配置文件，首次运行自动生成默认配置
- 优先级：命令行参数 > 配置文件 > 默认值
- 配置文件不存在时不报错退出，而是生成默认配置后返回默认值，允许命令行参数覆盖后继续运行
- 必填项（如 relay 地址）为空时在 main 中检查并报错退出
- device-cam.toml / relay-server.toml / viewer.toml 分别对应三个软件
- `--config` 参数可指定配置文件路径，默认为当前目录

### 断连检测 + 自动重连设计（2026-07-10 新增）
- **事件通道架构**：`MediaPlayer` 内部维护 `event_sender`/`event_receiver` (mpsc channel)，`receive_frames` 和 `poll_swarm` 通过 `event_sender` 发送 `MediaPlayerEvent`，上层通过 `poll_event()` 非阻塞轮询
- **断连触发源**：
  - `receive_frames` EOF 或读错误 → `MediaPlayerEvent::Disconnected`
  - `ConnectionClosed (num_established==0)` → `MediaPlayerEvent::Disconnected`
- **重连流程**：`jni_bridge.rs` 主循环检测到 `Disconnected` → 发送 `ViewerEvent::Disconnected` 给 Android → 发送 `ViewerEvent::Connecting` → 调用 `viewer.reconnect()` → 成功则 `Connected`+`StreamReady`，失败则 `Error` 并退出
- **重连不重建 Swarm**：`reconnect()` 复用现有 Swarm，只重新拨号 Relay + 打开 stream，避免重新生成 PeerId 和重建 transport
- **Jitter Buffer 清空**：重连时调用 `jitter.clear()` 丢弃旧数据，避免旧帧干扰新会话
