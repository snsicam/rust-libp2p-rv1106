# **1. 实现模型**

## **1.1 上下文视图**

本次修改仅涉及QUIC transport层的socket创建逻辑，不涉及relay协议层或其他transport层。

修改前后的上下文对比：

```
修改前：
  swarm.listen_on(quic_addr)
    → GenTransport::listen_on()
      → create_socket()          ← 缺少reuse设置
        → Socket::new() → bind()
      → new_endpoint(socket)

  swarm.dial()
    → GenTransport::dial()
      → bound_socket()           ← 直接UdpSocket::bind，无reuse
        → new_endpoint(socket)

修改后：
  swarm.listen_on(quic_addr)
    → GenTransport::listen_on()
      → create_socket()          ← 新增reuse设置
        → Socket::new() → set_reuse_address(true) → set_reuse_port(true) → bind()
      → new_endpoint(socket)

  swarm.dial()
    → GenTransport::dial()
      → bound_socket()           ← 改用socket2创建，新增reuse设置
        → Socket::new() → set_reuse_address(true) → set_reuse_port(true) → bind()
        → new_endpoint(socket)
```

## **1.2 服务/组件总体架构**

修改集中在 `transports/quic/src/transport.rs` 文件的 `GenTransport` 结构体的两个方法：

1. **`create_socket`**：用于监听（listen_on）场景，创建绑定到指定地址的UDP socket
2. **`bound_socket`**：用于拨号（dial）场景，创建绑定到通配地址的UDP socket

与TCP transport的reuse实现保持一致的架构模式：
- 使用 `socket2` 库创建底层socket
- 在 `bind()` 之前设置 `SO_REUSEADDR`
- 通过条件编译在Unix平台（排除Solaris/Illumos）设置 `SO_REUSEPORT`

## **1.3 实现设计文档**

### 1.3.1 `create_socket` 方法修改

**文件**：`transports/quic/src/transport.rs:183-196`

**当前实现**：
```rust
fn create_socket(&self, socket_addr: SocketAddr) -> io::Result<UdpSocket> {
    let socket = Socket::new(
        Domain::for_address(socket_addr),
        Type::DGRAM,
        Some(socket2::Protocol::UDP),
    )?;
    if socket_addr.is_ipv6() {
        socket.set_only_v6(true)?;
    }
    socket.bind(&socket_addr.into())?;
    Ok(socket.into())
}
```

**修改方案**：在 `set_only_v6` 之后、`bind` 之前，添加 `set_reuse_address` 和条件编译的 `set_reuse_port`，与TCP transport的 `create_socket` 方法保持一致的模式。

修改要点：
- `socket.set_reuse_address(true)?` — 无条件设置，所有平台均支持
- `socket.set_reuse_port(true)?` — 仅在Unix平台（排除Solaris/Illumos）设置，使用与TCP transport相同的 `#[cfg]` 条件编译宏
- 无需引入 `PortUse` 参数，因为QUIC监听socket始终使用reuse策略（与TCP的 `do_listen` 传入 `PortUse::Reuse` 一致）

### 1.3.2 `bound_socket` 方法修改

**文件**：`transports/quic/src/transport.rs:198-211`

**当前实现**：
```rust
fn bound_socket(&mut self, socket_addr: SocketAddr) -> Result<quinn::Endpoint, Error> {
    let socket_family = socket_addr.ip().into();
    if let Some(waker) = self.waker.take() {
        waker.wake();
    }
    let listen_socket_addr = match socket_family {
        SocketFamily::Ipv4 => SocketAddr::new(Ipv4Addr::UNSPECIFIED.into(), 0),
        SocketFamily::Ipv6 => SocketAddr::new(Ipv6Addr::UNSPECIFIED.into(), 0),
    };
    let socket = UdpSocket::bind(listen_socket_addr)?;  // ← 直接使用标准库bind
    let endpoint_config = self.quinn_config.endpoint_config.clone();
    let endpoint = Self::new_endpoint(endpoint_config, None, socket)?;
    Ok(endpoint)
}
```

**修改方案**：将 `UdpSocket::bind` 替换为使用 `socket2::Socket` 创建socket，设置reuse选项后再bind，与 `create_socket` 方法保持一致。

修改要点：
- 使用 `Socket::new()` 创建socket，替代 `UdpSocket::bind()`
- 设置 `set_reuse_address(true)`
- 条件编译设置 `set_reuse_port(true)`
- 调用 `socket.bind()` 后转为 `UdpSocket`
- 拨号socket绑定到通配地址（0.0.0.0:0 或 [::]:0），端口由操作系统分配

# **2. 接口设计**

## **2.1 总体设计**

本次修改不改变任何公共API接口。`create_socket` 和 `bound_socket` 均为 `GenTransport` 的私有方法，修改仅影响内部实现。

## **2.2 接口清单**

无新增或修改的公共接口。修改完全在私有方法内部完成。

# **4. 数据模型**

## **4.1 设计目标**

无需新增或修改数据模型。socket选项的设置是运行时行为，不涉及持久化数据。

## **4.2 模型实现**

不涉及。
