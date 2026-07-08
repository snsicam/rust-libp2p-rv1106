# P2P 摄像头设备授权与用户管理方案

> 版本: v1.0 | 日期: 2026-07-03 | 状态: 待论证
>
> 背景: 当前系统无中心授权机制，任何知道 PeerId 的节点均可连接设备。本方案设计完整的设备注册、用户绑定、局域网/外网双场景授权体系。

---

## 一、需求概述

| 场景 | 需求 | 约束 |
|------|------|------|
| **外网连接** | Viewer 通过 Relay 连接设备，需验证 Viewer 有权限 | 经过 Relay，可依赖业务服务器 |
| **局域网直连** | Viewer 通过 mDNS 发现设备并直连，也需验证权限 | 不经过外网，设备需本地决策 |
| **设备管理** | 客户（二次开发者）能管理设备列表、禁用设备 | 提供 REST API |
| **用户绑定** | 用户购买设备后，能绑定到自己的账号 | 扫码或其他配网方式 |

---

## 二、整体架构

```
┌──────────────────────────────────────────────────────────────────────────────┐
│                          业务服务器 (Auth Server)                           │
│                                                                             │
│  ┌──────────────┐  ┌──────────────┐  ┌────────────────────────────────┐  │
│  │ 设备注册 API  │  │ 用户绑定 API  │  │ Token 颁发与校验               │  │
│  │              │  │              │  │  - device_token (设备→Relay)   │  │
│  │ - 注册设备   │  │ - 绑定设备   │  │  - session_token (Viewer→设备) │  │
│  │ - 禁用设备   │  │ - 解绑设备   │  │  - 授权用户列表下发            │  │
│  │ - 查询设备   │  │ - 查询我的   │  │                              │  │
│  └──────────────┘  │   设备列表   │  └────────────────────────────────┘  │
│                     └──────────────┘                                      │
│  数据库: devices / users / device_bindings / device_tokens                 │
└────────────────────────────────────┬──────────────────────────────────────┘
                                     │ REST API
                ┌────────────────────┼────────────────────┐
                │                    │                    │
         ┌──────▼──────┐    ┌──────▼──────┐    ┌──────▼──────┐
         │    Device    │    │   Viewer    │    │  Relay Server │
         │    (Cam)    │    │    (App)   │    │  (with Auth)  │
         └──────┬──────┘    └──────┬──────┘    └──────┬──────┘
                │                   │                   │
                │   ════════════════════════════════════════
                │              P2P 网络（libp2p）
                │   ════════════════════════════════════════
                │                   │
                └───────── mDNS 局域网直连 ─────────┘
                  (不经过业务服务器，需本地授权)
```

---

## 三、设备身份体系

### 3.1 双身份设计

| 身份类型 | 用途 | 生命周期 | 存储位置 |
|----------|------|----------|----------|
| **设备三元组** | 授权认证 | 与设备硬件绑定，出厂固定 | 工厂烧录到 vendor 分区 |
| **libp2p PeerId** | P2P 网络寻址 | 可变化（重新生成密钥） | `device-cam.key` 文件 |

```
设备三元组（授权身份）:
  ┌─────────────────────────────────────┐
  │ product_key   = "pk_abc123"       │  ← 同一型号产品共用
  │ device_id     = "dev_88008800aabb"│  ← 全局唯一
  │ device_secret = "Ds3k9m..."       │  ← 32字节，不可明文传输
  └─────────────────────────────────────┘

libp2p 身份（网络身份）:
  ┌─────────────────────────────────────┐
  │ Ed25519 密钥对 → PeerId           │
  │ 例: 12D3KooWKNF...               │
  └─────────────────────────────────────┘

关系: 多对一（一个 device_id 可以对应多个 PeerId，换密钥不影响授权）
```

### 3.2 工厂烧录数据

```
设备 Flash 分区:
┌─────────────────────────────────────────────┐
│ vendor 分区 (只读，工厂烧录，OTA 不清除)     │
│   product_key   = "pk_abc123"             │
│   device_id     = "dev_88008800aabb"      │
│   device_secret = "Ds3k9m2x..." (加密存储) │
└─────────────────────────────────────────────┘
┌─────────────────────────────────────────────┐
│ userdata 分区 (可读写)                      │
│   device-cam.key  (Ed25519 密钥)           │
│   auth.db         (本地授权用户列表)         │
│   home_key        (局域网共享密钥，可选)      │
└─────────────────────────────────────────────┘
```

---

## 四、外网场景授权流程

### 4.1 设备注册

设备首次上电（或恢复出厂后首次启动）:

```
Device                           业务服务器
  │                                 │
  │  POST /api/v1/device/register  │
  │  {                              │
  │    "product_key": "pk_abc123", │
  │    "device_id": "dev_8800...", │
  │    "peer_id": "12D3KooW...",  │
  │    "timestamp": 1719999999,    │
  │    "signature": "..."           │
  │  }                              │
  │────────────────────────────────>│
  │                                 │  1. 用 device_secret 验证 signature
  │                                 │  2. 查数据库，若已注册则更新 peer_id
  │                                 │  3. 状态设为 active
  │                                 │  4. 生成 device_token
  │  {                              │
  │    "device_token": "dt_xxx",   │
  │    "expire_at": 1769999999     │
  │  }                              │
  │<────────────────────────────────│
  │                                 │
  │  保存 device_token 到本地         │
  └─────────────────────────────────┘

signature = HMAC-SHA256(device_secret, product_key + device_id + peer_id + timestamp)
```

### 4.2 Relay 连接授权

当前问题: Device 直接向 Relay 请求 reservation，无验证。
改进方案: Device 连接 Relay 前，先完成认证握手。

```
Device                            Relay Server
  │                                 │
  │  1. TCP/QUIC 连接到 Relay       │
  │────────────────────────────────>│
  │                                 │
  │  2. 发起认证协议                 │
  │  /p2p-camera/auth/1.0.0       │
  │  {device_id, device_token}      │
  │────────────────────────────────>│
  │                                 │  校验 token 有效性
  │                                 │  (可缓存到 Redis，避免每次查 DB)
  │  3. 认证结果                     │
  │  {ok: true} / {error: "..."}   │
  │<────────────────────────────────│
  │                                 │
  │  4. 认证通过后，允许 relay       │
  │     reservation 请求             │
  │────────────────────────────────>│
  │                                 │
  └─────────────────────────────────┘
```

### 4.3 Viewer 连接设备

```
Viewer (App)                   业务服务器                   Device
  │                                 │                        │
  │  登录 App                        │                        │
  │  (获取 user_token)               │                        │
  │<────────────────────────────────│                        │
  │                                 │                        │
  │  POST /api/v1/user/devices      │                        │
  │  (获取我的设备列表)               │                        │
  │────────────────────────────────>│                        │
  │  [{device_id, peer_id,          │                        │
  │    online_status}]              │                        │
  │<────────────────────────────────│                        │
  │                                 │                        │
  │  选择设备，请求连接               │                        │
  │  POST /api/v1/device/connect    │                        │
  │  {device_id: "dev_8800..."}    │                        │
  │────────────────────────────────>│                        │
  │                                 │  1. 校验用户是否绑定该设备│
  │                                 │  2. 生成 session_token  │
  │                                 │  3. 记录 session_token  │
  │  {                              │                        │
  │    "relay_addr": "...",         │                        │
  │    "session_token": "st_xxx"    │                        │
  │  }                              │                        │
  │<────────────────────────────────│                        │
  │                                 │                        │
  │  通过 libp2p 连接设备            │                        │
  │  (携带 session_token)           │                        │
  │─────────────────────────────────────────────────────────>│
  │                                 │                        │  验证 session_token
  │                                 │                        │  (向业务服务器校验)
  │                                 │                        │
  │<─────────────────────────────────────────────────────────│
  │  开始接收视频流                   │                        │
  └─────────────────────────────────┘                        │
```

---

## 五、局域网场景授权流程

### 5.1 问题分析

局域网内 mDNS 直连:
- Viewer 通过 mDNS 发现设备
- 直接发起 libp2p 连接（Noise 加密，但不验证身份）
- 不经过 Relay 和业务服务器
- **❓ 如何验证 Viewer 有权限？**

### 5.2 方案 A：本地认证协议（推荐）

**设计思路：** 设备本地存储授权用户列表，Viewer 连接时出示签名凭证。

设备本地 `auth.db` 格式:

```toml
# /userdata/auth.toml
# 管理员公钥（拥有所有权限，可添加/删除用户）
admin_public_key = "ed25519:12D3KooW..."

# 授权用户列表
[[authorized_users]]
user_id    = "user_123"
public_key = "ed25519:12D3KooW..."
role       = "owner"       # owner | viewer | guest
expire_at  = 0            # 0 = 永不过期，非零 = Unix 时间戳

[[authorized_users]]
user_id    = "user_456"
public_key = "ed25519:12D3KooW..."
role       = "guest"
expire_at  = 1720000000   # 临时访客，过期自动失效
```

**连接时序：**

```
Viewer (App)                          Device (Cam)
  │                                       │
  │  mDNS 发现设备                        │
  │  (获取 PeerId)                        │
  │                                       │
  │  libp2p Noise 握手 ─────────────────>│  (建立加密信道)
  │<──────────────────────────────────────│
  │                                       │
  │  发起 /local-auth 协议                │
  │  {                                    │
  │    "user_id": "user_123",            │
  │    "device_id": "dev_8800",          │
  │    "timestamp": 1719999999,          │
  │    "signature": "..."                 │
  │  }                                    │
  │──────────────────────────────────────>│
  │                                       │  1. 查 auth.db: user_id 在白名单？
  │                                       │  2. 用 public_key 验证签名
  │                                       │  3. 检查 timestamp (防重放，±5分钟)
  │                                       │  4. 检查 expire_at
  │                                       │
  │  {ok: true, role: "owner"}           │
  │<──────────────────────────────────────│
  │                                       │
  │  发起 /video-main 协议                │
  │  (仅认证通过后才 accept)               │
  │──────────────────────────────────────>│
  │                                       │
  │<──────────────────────────────────────│  开始推流
  │  H.265 NAL Units                     │
  │                                       │
  └───────────────────────────────────────┘

signature = Ed25519_Sign(viewer_private_key, user_id + device_id + timestamp)
```

### 5.3 授权用户列表同步

**方式1: 外网同步（推荐）**

设备定期（或每次上线时）从业务服务器拉取授权用户列表:

```
GET /api/v1/device/auth-users
Headers: { "Authorization": "Bearer <device_token>" }

Response:
{
  "code": 0,
  "data": {
    "users": [
      {"user_id": "user_123", "public_key": "...", "role": "owner", "expire_at": 0}
    ]
  }
}

设备写入本地 auth.db
```

**方式2: 局域网管理员配置**

管理员 App 通过蓝牙/WiFi 直连设备 → 添加/删除授权用户 → 直接写入设备 auth.db

### 5.4 方案 B：Home Key（家庭共享密钥）

适用场景: 家庭用户，用户少，类似 WiFi 密码。

```
Home Key:
  - 设备首次配网时生成，或业务服务器下发
  - 所有家庭成员 App 持有同一个 Home Key
  - 设备本地存储 Home Key 的 HMAC 验证密钥

连接流程:
  Viewer → Device: HMAC-SHA256(home_key, viewer_peer_id + timestamp)
  Device 本地验证 → 通过则接受连接

Home Key 分发:
  - 设备机身二维码: {device_id, home_key}
  - 或业务服务器在用户绑定后下发到 App
```

### 5.5 方案 C：应用层密码（最简单，兜底）

适合场景: 临时访问、演示。类似 RTSP 认证方式。

```
Viewer → 设备: "我要看视频"
设备 → Viewer: "需要密码（401 Unauthorized）"
Viewer → 设备: "密码: 123456"
设备 → Viewer: "密码正确，开始推流"
```

---

## 六、三种局域网方案对比

| 维度 | 方案 A（本地白名单） | 方案 B（Home Key） | 方案 C（应用层密码） |
|------|---------------------|-------------------|---------------------|
| **安全性** | 高（非对称签名，防重放） | 中（共享密钥，泄露即全开） | 低（密码可暴力破解） |
| **用户体验** | 好（一次绑定，后续无感） | 好（输入一次 Home Key） | 差（每次都要输密码） |
| **开发量** | 大（需要密钥管理、白名单同步） | 中 | 小 |
| **适合场景** | 多用户、多权限（主人/访客） | 家庭场景，用户少 | 临时访问、演示 |
| **离线可用** | ✅ | ✅ | ✅ |

**推荐：A + B 混合**
- 优先使用本地白名单（方案 A）
- 白名单验证失败时，尝试 Home Key（方案 B）
- 两者都失败时，要求应用层密码（方案 C 兜底）

---

## 七、数据库设计

```sql
-- 产品表（一个型号一个 product_key）
CREATE TABLE products (
    product_key   VARCHAR(32)  PRIMARY KEY,
    name          VARCHAR(128),
    description   TEXT,
    created_at    BIGINT
);

-- 设备表
CREATE TABLE devices (
    device_id     VARCHAR(64)  PRIMARY KEY,
    product_key   VARCHAR(32),
    device_secret VARCHAR(64),   -- AES 加密存储，不可明文
    peer_id       VARCHAR(64),   -- libp2p PeerId，可更新
    status        INT DEFAULT 1, -- 1=active, 0=disabled
    owner_user_id VARCHAR(64),   -- 当前绑定用户（主人）
    last_online_at BIGINT,
    created_at    BIGINT,
    INDEX(product_key),
    INDEX(owner_user_id)
);

-- 用户表（可对接客户现有用户系统，此处为独立实现）
CREATE TABLE users (
    user_id    VARCHAR(64) PRIMARY KEY,
    username   VARCHAR(128),
    public_key VARCHAR(128),  -- Ed25519 公钥（用于局域网认证）
    created_at BIGINT
);

-- 设备-用户绑定关系（支持分享给多个用户）
CREATE TABLE device_bindings (
    id          BIGINT AUTO_INCREMENT PRIMARY KEY,
    device_id   VARCHAR(64),
    user_id     VARCHAR(64),
    role        INT DEFAULT 1,  -- 1=owner, 2=viewer(只读), 3=guest(限时)
    expire_at   BIGINT DEFAULT 0,  -- 0=永不过期
    created_at  BIGINT,
    UNIQUE KEY(device_id, user_id),
    INDEX(user_id)
);

-- 设备 Token 表（用于设备连接 Relay 时的认证）
CREATE TABLE device_tokens (
    token       VARCHAR(128) PRIMARY KEY,
    device_id   VARCHAR(64),
    expire_at   BIGINT,
    created_at  BIGINT,
    INDEX(device_id)
);

-- Session Token 表（用于 Viewer 连接设备时的临时凭证）
CREATE TABLE session_tokens (
    token       VARCHAR(128) PRIMARY KEY,
    device_id   VARCHAR(64),
    user_id     VARCHAR(64),
    expire_at   BIGINT,
    created_at  BIGINT,
    INDEX(device_id),
    INDEX(user_id)
);
```

---

## 八、业务服务器 API 设计

### 8.1 设备注册

```http
POST /api/v1/device/register
Content-Type: application/json

Request:
{
  "product_key": "pk_abc123",
  "device_id": "dev_88008800aabb",
  "peer_id": "12D3KooW...",      // 可选，libp2p PeerId
  "timestamp": 1719999999,
  "signature": "base64(...)"     // HMAC-SHA256(device_secret, body)
}

Response:
{
  "code": 0,
  "data": {
    "device_token": "dt_xxx...xxx",
    "expire_at": 1769999999       // token 过期时间（Unix 秒）
  }
}
```

### 8.2 用户绑定设备

```http
POST /api/v1/user/bind_device
Headers: { "Authorization": "Bearer <user_token>" }
Content-Type: application/json

Request:
{
  "device_id": "dev_88008800aabb"
}

Response:
{
  "code": 0,
  "msg": "bound"
}
```

### 8.3 获取我的设备列表

```http
GET /api/v1/user/devices
Headers: { "Authorization": "Bearer <user_token>" }

Response:
{
  "code": 0,
  "data": {
    "devices": [
      {
        "device_id": "dev_88008800aabb",
        "peer_id": "12D3KooW...",
        "product_key": "pk_abc123",
        "online": true,
        "last_online_at": 1719999999
      }
    ]
  }
}
```

### 8.4 请求连接设备

```http
POST /api/v1/device/connect
Headers: { "Authorization": "Bearer <user_token>" }
Content-Type: application/json

Request:
{
  "device_id": "dev_88008800aabb"
}

Response:
{
  "code": 0,
  "data": {
    "relay_addr": "/ip4/1.2.3.4/tcp/443/p2p/12D3KooWRelay.../p2p-circuit/p2p/12D3KooWDevice...",
    "session_token": "st_xxx...xxx",
    "expire_at": 1719999999
  }
}
```

### 8.5 获取设备授权用户列表（设备调用）

```http
GET /api/v1/device/auth-users
Headers: { "Authorization": "Bearer <device_token>" }

Response:
{
  "code": 0,
  "data": {
    "users": [
      {
        "user_id": "user_123",
        "public_key": "ed25519:12D3KooW...",
        "role": "owner",
        "expire_at": 0
      }
    ]
  }
}
```

### 8.6 禁用/启用设备

```http
POST /api/v1/device/set_status
Headers: { "Authorization": "Bearer <user_token>" }
Content-Type: application/json

Request:
{
  "device_id": "dev_88008800aabb",
  "status": 0   // 0=禁用, 1=启用
}

Response:
{
  "code": 0,
  "msg": "status updated"
}
```

---

## 九、与现有代码的集成点

| 模块 | 文件 | 改动内容 |
|------|------|----------|
| **DeviceCam** | `device-cam/src/main.rs` | 启动时向业务服务器注册，获取 `device_token`；新增本地认证协议处理 |
| **DeviceCam** | `device-cam/src/config.rs` | 新增 `[auth]` 配置段：`auth_server_url`、`product_key`、`device_id` 等 |
| **Relay Server** | `relay-server/src/behaviour.rs` | 新增 `/p2p-camera/auth/1.0.0` 协议处理；验证 `device_token` 后才允许 reservation |
| **Relay Server** | `relay-server/src/main.rs` | 新增 Auth Server 地址配置，用于验证 token |
| **Viewer** | `mobile-core/src/viewer.rs` | 新增 `connect_device(device_id)` API；实现本地认证签名 |
| **Proto** | `proto/src/stream_protocols.rs` | 新增 `AUTH_PROTOCOL` 和 `LOCAL_AUTH_PROTOCOL` 常量 |
| **新增模块** | `p2p-camera/auth-server/` | 业务服务器实现（可先用简单 Rust 实现，后续客户自行替换） |

### 9.1 新增配置项

`device-cam.toml`:

```toml
# ============ 设备授权 ============
[auth]
# 业务服务器地址
server_url = "https://auth.example.com"
# 设备身份（从 factory 分区读取，此处为 fallback）
product_key = ""
device_id = ""
# device_secret 不从配置文件读取，从 vendor 分区读取
# 本地认证
enable_local_auth = true
local_auth_db = "/userdata/auth.db"
# Home Key (可选，用于家庭共享)
home_key = ""
```

### 9.2 新增协议常量

`proto/src/stream_protocols.rs`:

```rust
/// 设备向 Relay Server 认证（外网场景）
pub const AUTH_PROTOCOL: StreamProtocol = 
    StreamProtocol::new("/p2p-camera/auth/1.0.0");

/// Viewer 向设备本地认证（局域网场景）
pub const LOCAL_AUTH_PROTOCOL: StreamProtocol = 
    StreamProtocol::new("/p2p-camera/local-auth/1.0.0");
```

---

## 十、安全考虑

| 风险 | 缓解措施 |
|------|----------|
| **device_secret 泄露** | 工厂烧录时加密存储；设备端只用于 HMAC 签名，不明文传输 |
| **token 泄露** | token 设短期过期时间；设备/Relay 定期刷新；支持吊销 |
| **重放攻击** | 签名中包含 timestamp，设备验证时间窗口（±5分钟） |
| **局域网伪造设备** | 依赖 Noise 握手（需要正确的密钥对）；可选：设备出示设备认证证书 |
| **session_token 泄露** | 短期有效（建议 ≤ 5 分钟）；一次使用后失效 |
| **本地 auth.db 篡改** | 文件权限限制；可选：HMAC 完整性校验 |

---

## 十一、实施路线

### Phase 1：基础授权（外网场景）

- [ ] 实现业务服务器基础 API（设备注册、用户绑定、Token 颁发）
- [ ] Relay Server 增加 Token 验证
- [ ] DeviceCam 增加设备注册和 Token 获取逻辑
- [ ] Viewer 增加 `connect_device` API

### Phase 2：局域网授权

- [ ] 实现本地认证协议（`/local-auth`）
- [ ] 设备本地 `auth.db` 读写
- [ ] 设备从业务服务器同步授权用户列表
- [ ] Viewer 实现 Ed25519 签名

### Phase 3：配网和用户体验

- [ ] 实现蓝牙配网或 WiFi AP 配网（首次配网场景）
- [ ] Home Key 机制（家庭共享场景）
- [ ] 设备禁用/启用 API
- [ ] 用户解绑设备 API

### Phase 4：安全加固

- [ ] Token 吊销机制
- [ ] 设备证书（X.509）支持
- [ ] `auth.db` 完整性校验
- [ ] 审计日志

---

## 十二、与业界方案对比

| 维度 | 本方案 | 涂鸦 Tuya | 腾讯云 IoT |
|------|--------|-----------|------------|
| **设备身份** | 三元组 + PeerId 双身份 | uuid + auth_key | ProductID + DeviceName + Secret |
| **局域网授权** | ✅ 本地认证协议 | ❌ 依赖云端 | ❌ 依赖云端 |
| **外网授权** | Relay 侧 Token 验证 | 云端 P2P 服务器 | TRTC/IM 转发 |
| **设备管理** | REST API（客户可自建） | 涂鸦 IoT 平台（封闭） | 腾讯云控制台 |
| **脱离云端** | ✅ 局域网可完全离线工作 | ❌ 不可 | ❌ 不可 |
| **适合场景** | 私有化部署、自研平台 | 白牌摄像头、快速上市 | 腾讯生态整合 |

---

## 十三、待论证问题

1. **业务服务器是否独立部署？** 还是作为 Relay Server 的一个模块？
   - 推荐：独立部署，Relay Server 只做 Token 验证（可对接多个 Auth Server）

2. **设备三元组如何安全存储？**
   - RV1106: 使用 `rkvendor` 接口写入 vendor 分区（只读，OTA 不清除）
   - 其他平台: 使用 Secure Element 或 TEE

3. **Viewer 的 Ed25519 密钥对如何管理？**
   - 每个 Viewer App 实例生成一对密钥
   - 公钥需要在用户绑定设备时上传到业务服务器
   - 私钥存储在 App 的安全区域（iOS Keychain / Android Keystore）

4. **首次配网如何把授权信息写入设备？**
   - 推荐：蓝牙配网（类似智能家居设备）
   - 备选：WiFi AP 模式（设备作为热点，手机连接后配置）
   - 备选：扫码绑定（设备机身二维码包含 `device_id + pairing_token`）

5. **多 Relay 场景下的 Token 验证？**
   - 所有 Relay 共享同一个 Auth Server 或使用共享 Redis 缓存
   - device_token 在所有 Relay 上均有效

---

> 本文档待论证后实施。如有调整，请更新版本号和日期。
