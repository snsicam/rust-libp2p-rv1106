# 实现任务清单

> **状态: 已完成** (2026-07-16)

## 任务1：修改 `create_socket` 方法，添加 SO_REUSEADDR 和 SO_REUSEPORT 设置 ✅

**文件**：`transports/quic/src/transport.rs:183-196`

**操作**：在 `set_only_v6` 之后、`bind` 之前，添加端口复用选项设置

**具体修改**：
- 在 `if socket_addr.is_ipv6() { socket.set_only_v6(true)?; }` 之后添加：
  - `socket.set_reuse_address(true)?;`
  - `#[cfg(all(unix, not(any(target_os = "solaris", target_os = "illumos"))))]` 条件编译块中添加 `socket.set_reuse_port(true)?;`

**验收标准**：
- `set_reuse_address` 和 `set_reuse_port` 在 `bind` 之前调用
- 条件编译策略与TCP transport（`transports/tcp/src/lib.rs:215-222`）完全一致
- 编译通过，无警告

---

## 任务2：修改 `bound_socket` 方法，使用 socket2 创建 socket 并添加 reuse 设置 ✅

**文件**：`transports/quic/src/transport.rs:198-211`

**操作**：将 `UdpSocket::bind(listen_socket_addr)` 替换为使用 `socket2::Socket` 创建socket，设置reuse选项后bind

**具体修改**：
- 使用 `Socket::new(Domain::for_address(listen_socket_addr), Type::DGRAM, Some(socket2::Protocol::UDP))` 创建socket
- 添加 `socket.set_reuse_address(true)?;`
- 添加条件编译的 `socket.set_reuse_port(true)?;`
- 调用 `socket.bind(&listen_socket_addr.into())?;`
- 将socket转为UdpSocket：`let socket: UdpSocket = socket.into();`

**验收标准**：
- `bound_socket` 方法不再直接使用 `UdpSocket::bind`
- reuse选项设置在bind之前
- 拨号socket绑定到通配地址（0.0.0.0:0 或 [::]:0），端口由操作系统分配
- 编译通过，无警告

---

## 任务3：编译验证 ✅

**操作**：运行 `cargo check` 验证修改后的代码编译通过

**验收标准**：
- `transports/quic` 模块编译通过
- 无新增编译警告
