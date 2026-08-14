# 长期记忆 (MEMORY.md)

## 项目结构 / Workspace（关键）
- `p2p-camera/Cargo.toml` 是**独立 workspace**（proto, mobile-core, device-cam, relay-server）；父级 `rust-libp2p/Cargo.toml` 是另一个 workspace（libp2p 官方 crate），不含 p2p-camera。
- Cargo 不支持嵌套 workspace → **所有构建命令必须在 `p2p-camera/` 目录执行**。
- Viewer 不是独立 crate：是 `mobile-core` 的 example → `cargo build --example media_viewer -p mobile-core --features player`。
- path 依赖向上引用父 workspace：`../libp2p`、`../protocols/stream`、`../swarm`。

## Windows 构建环境（已验证可用）
- 依赖：Rust 1.97.1 + VS Build Tools 2022(C++ 桌面 + **ATL 组件**) + vcpkg(`E:\vcpkg`，装 ffmpeg/sdl2/llvm x64-windows) + libclang。
- 一键脚本：`p2p-camera/scripts/setup_env.ps1`；构建 `build_viewer.ps1`；运行 `run_media_viewer.ps1`；打包 `package_viewer.ps1`。这些脚本的 `$ProjectRoot` 必须是 `..`（指向 `p2p-camera/`）。
- **llvm 只为提供 libclang**：`mobile-core/build.rs` 用 bindgen 解析 ffmpeg/sdl2 C 头。仅编译期需要，运行时不依赖。
- **libclang 加载失败(err=126) 修复**：仅设 `LIBCLANG_PATH` 不够，**必须把该目录前置到 `PATH`**（`bin\libclang.dll` 依赖同目录 `LLVM-C.dll`/`z.dll`/`zstd.dll`）。
- FFmpeg 8.0 移除了 `libavcodec/avfft.h`，但 ffmpeg-sys-next 仍引用 → 已创建空占位头文件。
- **ffmpeg-next 版本号必须匹配 vcpkg 的 ffmpeg 主版本**（当前 FFmpeg 8.1.2 → `ffmpeg-next = "8"`）。不匹配会有大量 API 缺失编译错误。
- vcpkg 坑：必须在 `vcvars64.bat` 环境下运行（否则找不到 `atlbase.h`）；vcpkg install 会 re-exec，父进程早退属正常；机器上有 3 个 vcpkg 目录，实际使用 `E:\vcpkg`。

## 文件下行流 (FILE_PROTOCOL = /p2p-camera/file/1.0.0) 约定
- 抓拍 AVI 下载采用「**device-cam 主动推 / viewer 接收**」模式：device-cam `control_handler.rs::download_file` 收到控制请求后 `open_stream(FILE_PROTOCOL)` 出站推数据；viewer 端必须用 `stream_control.accept(FILE_PROTOCOL)` 注册入站监听（先 accept 再发控制请求），再用 `incoming.next()` 取流，**严禁** viewer 端 `open_stream` FILE 协议（否则报 `remote peer does not support`）。
- `Control::accept` 返回 `IncomingStreams`（futures::Stream, yield `(PeerId, Stream)`），见 `protocols/stream/src/control.rs`。

## 网络（国内）
- rustup/cargo 用 `rsproxy.cn` 镜像（`~/.cargo/config.toml` 配 `rsproxy-sparse` + `git-fetch-with-cli = true`）。
- vcpkg 克隆用 gitee 镜像 `https://gitee.com/mirrors/vcpkg`。

## QUIC 连接稳定性（重要踩坑）
- **swarm idle timeout ≠ QUIC idle timeout**，是两层。`with_idle_connection_timeout` 只管「无活跃 stream 关连接」。
- `.with_quic()` 默认 `max_idle_timeout=10s` / `keep_alive_interval=5s` → 丢 2 个探测包即断，症状 `I/O error: timed out` + 断开时长随机。
- 调整需用 `.with_quic_config(|mut c| { c.max_idle_timeout = 30_000; ... c })`，且**三端（device-cam / viewer / relay-server）必须同步改**（生效值取协商较小者），relay 改完要重新部署。
- 排查顺序：先 ping 排除链路 → 再看断开 `Cause:`。`timed out` + 随机时长 = idle timeout 误杀，不是丢包。

## LAN 直连不稳 → 保留 relay 电路秒切（方案 A，已实现）
- 多网卡/对称 NAT 下 LAN 直连约 50s 一断，但 relay 电路很稳。**不要**在 LAN 升级成功后关掉 relay 连接。
- `mobile-core/src/viewer.rs`：LAN 升级后置 `relay_circuit_available = true`；`ConnectionClosed`(num_established==0) 时走 `fallback_to_relay()` —— 经现有 relay 重拨 circuit，不重建 Swarm、不重 mDNS，复用 swarm 级 `stream_control` 重开三路 stream，`connected` 保持 true。`reconnect()` 重建 Swarm 后须置 `relay_circuit_available = false`。
- 代码细节：`poll_swarm` 是 async fn 被循环驱动，内部**用 `return` 不能用 `continue`**；`StreamProtocol` 非 Copy，被 `open_stream` 消费后不可复用。
- 固定设备 UDP 端口稳定 NAT：`device-cam.toml` `udp_port = 48781`，`run_device_cam.sh` 传 `--udp-port`（`FIXED_UDP_PORT` 可覆盖）。

## libp2p IPv6 不双栈（重要踩坑）
- rust-libp2p 对 IPv6 监听 socket **强制 `set_only_v6(true)`**（`transports/tcp/src/lib.rs` + `transports/quic/src/transport.rs` 的 `create_socket`）。`/ip6/::` **不接受 IPv4-mapped 连接**，「IPv6 兼容 IPv4」在 libp2p **不成立**。
- 官方设计（CHANGELOG PR 1555）：IPv4-mapped 地址无法干净表达为 multiaddr，会污染 identify/地址簿/relay 电路地址 → 要求「同时起两个 listener」。
- 因此凡是要同时服务 v4/v6，必须**分别 listen_on `/ip4/0.0.0.0` 和 `/ip6/::`**，不能二选一。
- `relay-server` 已按此修复：IPv4 恒定监听，`use_ipv6` 语义 = 「是否**额外**加 IPv6」；IPv6 监听失败只 warn 不退出。
- relay 配置字段是 **`public_ips` 列表**（无旧 `public_ip` 单值兼容），自动按族生成 `/ip4` 或 `/ip6` 的 TCP+QUIC 外部地址；CLI `--public-ip` 可重复传。配 v6 必须同时 `use_ipv6 = true`，否则只通告不监听（已加 warn）。

## 设备交互模型（app 与 viewer 对齐）
- Android `MainActivity.kt`：点击=选中；长按=菜单(连接/断开/配置)；「删除」按钮删选中设备(带确认)。
- Rust viewer `media_viewer.rs`：单击=选中（无双击连接）；右键菜单=连接/断开/配置（顺序须与 app 一致，`CtxMenuAct` 与 `draw_context_menu` items 顺序对应）。
- 字段解耦：app `selectedDeviceId`(选中) / `currentDeviceId`(播放)；viewer `selected`(索引) / session(播放)。

## CodeBuddy 执行器注意事项
- `execute_command` 会自动跳过「可能耗时久」的命令；长时构建要用**独立会话**跑。
- Windows 长时构建可靠方式：`schtasks /Create /TN xxx /TR "xxx.bat" /SC ONCE /ST 23:59 /IT /F` + `schtasks /Run`。**必须加 `/IT`**（否则无 tty 直接死）。
- 反例：`cmd /c start "" xxx.bat` 会在下一条 `execute_command` 时被进程树回收杀掉。
- 批处理里 `call vcvars64.bat` 的输出要 `>nul 2>&1`，否则弄乱后续日志重定向句柄。
