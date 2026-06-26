# P2P-Camera Bug 修复记录

> 本文件记录开发过程中遇到的 bug 及修复方案，便于后续排查和团队共享。
> 新增记录请追加到表格末尾。

| # | 日期 | 模块 | 问题现象 | 根因 | 修复方案 | 涉及文件 |
|---|------|------|---------|------|---------|---------|
| 1 | 2026-06-24 | 构建 (RV1106) | `build_rv1106.sh` 报 "Cross compiler 'armv7l-linux-gnueabihf-gcc' not found" | 系统只有 `arm-linux-gnueabihf-gcc`，无 `armv7l-` 前缀的链接 | 创建符号链接 `/usr/local/bin/armv7l-linux-gnueabihf-gcc` → `/usr/bin/arm-linux-gnueabihf-gcc` | `scripts/build_rv1106.sh` |
| 2 | 2026-06-24 | 构建 (RV1106) | 链接报 "linker `arm-rockchip830-linux-uclibcgnueabihf-gcc` not found" | `.cargo/config.toml` 硬编码了 Rockchip 工具链链接器名，且 TOOLCHAIN_DIR 路径是绝对路径 | (1) TOOLCHAIN_DIR 改为项目相对路径 `$PROJECT_ROOT/toolchain/...`<br>(2) `.cargo/config.toml` 链接器改为通用的 `armv7l-linux-gnueabihf-gcc`<br>(3) rv1106 模式用 `CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER` 环境变量覆盖为 Rockchip 工具链 | `.cargo/config.toml`, `gateway/.cargo/config.toml`, `examples/ping/.cargo/config.toml`, `scripts/build_rv1106.sh` |
| 3 | 2026-06-24 | Gateway/Viewer | Viewer 连接 ~3s 后断开，报 "Gateway connection closed" 后重连，出现 HEVC "Duplicate POC" 错误 | yamux 无 keepalive，NAT/路由器丢弃空闲 TCP 连接（尽管 libp2p 层 ping 正常） | 见 #4，yamux keepalive 方案因 API 变更失败，最终由 #6 心跳设计 + #7 DCUtR 误判修复解决 | `gateway/src/main.rs`, `mobile-core/src/viewer.rs`, `mobile-core/examples/viewer_cli.rs` |
| 4 | 2026-06-25 | Gateway/Viewer | 编译报 `no method named set_keep_alive_interval / set_max_buffer_size / set_receive_window found for libp2p_yamux::Config` 及 `expected FnOnce() closure, found Config` | libp2p 0.54 的 yamux (v0.14) Config 已移除这些方法；`libp2p_yamux::Config` 是薄封装仅暴露 `set_max_num_streams`；`with_tcp`/`with_relay_client` 需要闭包 `FnOnce() -> Config` 而非实例 | 改用 `libp2p::yamux::Config::default` 函数指针（闭包形式）传给 builder，移除不存在的配置方法调用 | `gateway/src/main.rs`, `mobile-core/src/viewer.rs`, `mobile-core/examples/viewer_cli.rs` |
| 5 | 2026-06-25 | 脚本 | 所有脚本默认 `--release` 编译，调试不便 | 历史默认配置 | `start_relay.sh` 拆分 build/run 子命令；全部脚本 `--release` → debug，路径 `target/release/` → `target/debug/` | `scripts/start_relay.sh`, `scripts/start_server.sh`, `scripts/play_viewer.sh`, `scripts/build_rv1106.sh`, `scripts/build.sh` |
| 6 | 2026-06-25 | Gateway/Viewer | Viewer 在 idle_connection_timeout 后断开重连，且重连循环不断 | 心跳配置不合理：Viewer 不应有心跳和 idle timeout（视频流本身维持连接活跃） | **Gateway**: 保留 ping (5s) + idle_connection_timeout=120s（与 Relay 维持心跳）<br>**Viewer**: 移除 ping Behaviour，idle_connection_timeout=0（禁用，由 stream 层 read 返回 0/error 检测断开） | `gateway/src/behaviour.rs`, `mobile-core/src/viewer.rs`, `mobile-core/examples/viewer_cli.rs` | ✅ 已验证 |
| 7 | 2026-06-25 | Viewer (CLI) | DCUtR 直连升级后立即报 "Gateway connection closed" 触发重连，但直连实际还在 | DCUtR 打洞成功后 libp2p 自动关闭冗余的 circuit relay 连接，触发 `ConnectionClosed`；旧代码不看剩余连接数直接判定断开 | `ConnectionClosed` 分支检查 `num_established` 字段：`==0` 才真正断开触发重连；`>0` 说明只是 circuit 关闭、直连仍在，继续运行 | `mobile-core/examples/viewer_cli.rs` | ✅ 已验证 |
| 8 | 2026-06-26 | 构建 (RV1106) | `build_rv1106.sh` 报 "rockit include dir not found"，SDK 路径 `/home/song/samba/work/rv1106/lubancat` 不存在 | SDK_ROOT 默认值是开发者本地绝对路径，不通用 | 改为 `$PROJECT_ROOT/../../rv1106/RV1106_Linux_SDK`（相对项目根目录） | `scripts/build_rv1106.sh` | ✅ 已验证 |
| 9 | 2026-06-26 | 部署 (云服务器) | Gateway 连接云服务器 Relay 报 "Handshake timed out" (QUIC) 或 "Timeout has been reached" (TCP) | (1) 云服务器安全组未放行 UDP/TCP 4001 入站；(2) start_relay.sh 用 `hostname -I` 取到内网 IP，打印的连接地址错误 | (1) 腾讯云安全组添加 TCP+UDP 4001 入站规则；(2) start_relay.sh 新增 `--public-ip` 参数指定外网 IP | `scripts/start_relay.sh` | ✅ 已验证 |

## 关键设计决策

### 心跳设计原则（2026-06-25 确定）
- **只有 Gateway 需要 ping**：5s 间隔，与 Relay Server 维持心跳，`idle_connection_timeout=120s` 作为保底
- **Viewer 不需要 ping 和 idle_connection_timeout**：
  - 视频流持续传输天然保持连接活跃
  - 设为 0 禁用 swarm 层超时，避免画面静止（编码器不产帧）时误触发断开
  - 断开由 stream 层 `read` 返回 0/error 检测
- **不要给 Viewer 加 ping 或 idle_connection_timeout**，否则空闲时会误触发重连

### yamux Config API 注意事项（libp2p 0.54 / yamux 0.14）
- `libp2p_yamux::Config` 是 `yamux::Config` 的薄封装，仅暴露 `set_max_num_streams`
- **不存在** `set_keep_alive_interval` / `set_max_buffer_size` / `set_receive_window`（这些是旧版 API）
- `SwarmBuilder::with_tcp` / `with_relay_client` 的 multiplexer 参数需要 `FnOnce() -> Config` 闭包，传 `libp2p::yamux::Config::default` 函数指针即可，**不能传 Config 实例**

### DCUtR 直连升级注意事项
- DCUtR 打洞成功后，libp2p 会自动关闭冗余的 circuit relay 连接
- 处理 `SwarmEvent::ConnectionClosed` 时**必须检查 `num_established` 字段**：
  - `== 0`：所有连接已断开，触发重连
  - `> 0`：仍有其他连接（如直连），不要误判为断开
