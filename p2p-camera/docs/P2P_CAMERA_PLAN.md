# P2P 摄像头视频监控 — 完整开发方案

## 硬件与现状

| 项目 | 详情 |
|------|------|
| 摄像头芯片 | Rockchip RV1106 (Cortex-A7, Linux) |
| 视频编码 | H.265 (HEVC) 硬件编码 |
| 音频编码 | SDK 接口读取 PCM/AAC 裸数据 |
| 数据获取 | Rockchip SDK API 直接读取 (不走 RTSP) |
| 手机端 | 原生 APP (非浏览器) |
| 目标 | P2P 去中心化传输，节省中继流量 |

---

## 一、总体架构

```
┌──────────────────────────────────────────────────────────────────┐
│  模块 1: Relay Server (公网云服务器)                              │
│  ┌────────────────────────────────────────────────────────────┐  │
│  │ relay::Behaviour │ identify │ HTTP API                     │  │
│  │                                                                  │
│  │ 职责: 电路路由 + 信令中转(PeerId/地址交换) + DCUtR 协调    │  │
│  │ 注意: 不需要传统 TURN (coturn)，libp2p Relay Circuit 等价  │  │
│  └────────────────────────────────────────────────────────────┘  │
└──────────────────────────┬───────────────────────────────────────┘
                           │
              ┌────────────┴────────────┐
              │ TCP                      │ UDP (QUIC)
              ▼                          ▼
┌─────────────────────────┐  ┌──────────────────────────┐
│ 模块 2: P2P Gateway    │  │ 模块 3: Mobile APP       │
│ (运行在 RV1106 上)      │  │ (Android/iOS 原生)       │
│                         │  │                          │
│ ┌─────────────────────┐ │  │ ┌──────────────────────┐ │
│ │ SDK 回调线程         │ │  │ │ libp2p 节点          │ │
│ │ 视频: H.265 裸数据  │ │  │ │ QUIC + Noise + Yamux │ │
│ │ 音频: PCM/AAC 裸数据 │ │  │ │ Relay Client         │ │
│ │ → MediaPacket 封装  │ │  │ │ DCUtR                │ │
│ └─────────┬───────────┘ │  │ │ libp2p-stream 接收    │ │
│           ▼              │  │ └──────────────────────┘ │
│ ┌─────────────────────┐ │  │                          │
│ │ libp2p 节点          │ │  │ ┌──────────────────────┐ │
│ │ QUIC + Noise + Yamux│ │  │ │ A/V 解复用            │ │
│ │ Relay Client         │ │  │ │ Jitter Buffer (音+视)│ │
│ │ DCUtR                │ │  │ │ H.265 硬解码         │ │
│ │ libp2p-stream 发送   │ │  │ │ 音频硬解码           │ │
│ │   Stream 1: 视频帧   │ │  │ │ A/V 同步播放         │ │
│ │   Stream 2: 音频帧   │ │  │ └──────────────────────┘ │
│ └─────────────────────┘ │  │                          │
└─────────────────────────┘  └──────────────────────────┘
```

### 连接流程

```
阶段 1: 入网 + 信令
  Gateway ──TCP──▶ Relay Server ◀──QUIC── Mobile APP
  1. Gateway 在 Relay 预约 (Reserve)
  2. Gateway PeerId 存入 Redis/HTTP
  3. Mobile 通过 HTTP API 查询 Gateway PeerId

阶段 2: 建立中继连接
  Mobile ──Circuit──▶ Relay ──Circuit──▶ Gateway
  (通过 /p2p-circuit 地址拨号)

阶段 3: DCUtR 协商 + Hole Punch
  双方通过 circuit 交换各自的 QUIC 地址
  QUIC Hole Punch (dial_as_listener)

阶段 4: 直连传输 (成功)
  Mobile ◄══ QUIC Direct ══▶ Gateway
  Stream 1: 视频帧 (H.265 NAL)
  Stream 2: 音频帧 (PCM/AAC)

阶段 5: 直连失败 → 降级 Circuit
  Mobile ◄══ Relay Circuit ══▶ Gateway
  (仍然用 relay, 但比直连带宽贵)
```

---

## 二、模块详细设计

### 模块 1: Relay Server (relay-server/)

**部署**: 公网云服务器 (2核4G, 5Mbps+ 带宽)
**技术栈**: Rust + libp2p (tcp + relay + identify)

```rust
// 端口规划
TCP 4001 : libp2p 原生节点 (Gateway 用 TCP 连, Mobile 用 QUIC 连)
TCP 8080 : HTTP API (信令查询: PeerId 注册/发现)
```

**核心职责**:
1. `relay::Behaviour` — 电路中继转发
2. HTTP API — 简单信令服务:
   - `POST /register` — Gateway 注册 PeerId + 端口
   - `GET /peers` — Mobile 查询在线 Gateway 列表
3. Identify — 节点身份交换

**关于 TURN：不需要单独搭建**

| 对比维度 | 传统 TURN (coturn) | libp2p Relay Circuit |
|----------|-------------------|---------------------|
| 适用协议 | WebRTC ICE (UDP/TCP) | QUIC / libp2p 连接 |
| 工作层级 | UDP/TCP 包转发 | libp2p 连接级电路转发 |
| 穿透能力 | STUN 打洞失败后中继 | DCUtR 打洞失败后中继 |
| 是否适合本项目 | ❌ 我们不用 WebRTC | ✅ 原生配合 QUIC |

```
传统 WebRTC 方案 (不适合我们):
  App ←→ ICE ←→ coturn ←→ ICE ←→ Camera
  需要: STUN + TURN 两套服务
  
libp2p 方案 (我们采用):
  App ←→ QUIC ←→ Relay Circuit ←→ QUIC ←→ Camera  
  只需: 一个 Relay Server
```

libp2p 的 `relay::Behaviour` 已经实现了 TURN 等价功能——当打洞失败时通过中继转发全连接流量。**不需要额外部署 coturn**。

---

### 模块 2: P2P Gateway (gateway/)

**部署**: RV1106 上直接运行 (Rust cross-compile)
**技术栈**: Rust + Rockchip SDK FFI + libp2p (tcp + quic + relay-client + dcutr + stream)

**RV1106 交叉编译**:
```bash
# 目标: armv7-unknown-linux-gnueabihf (32位) 或 aarch64-unknown-linux-gnu (64位)
# 取决于你的 SDK

# 安装 target
rustup target add armv7-unknown-linux-gnueabihf

# 编译
cargo build --release --target armv7-unknown-linux-gnueabihf
```

**数据获取（修正：不走 RTSP，直接 SDK API）**:

```
Rockchip RV1106 硬件
│
├── VI (Video Input) ──▶ VENC (H.265 HW Encoder)
│                              │
├── AI (Audio Input) ──▶ AENC (Audio HW Encoder)
│                              │
│                    ┌─────────┴──────────┐
│                    │   Rockchip SDK API  │
│                    │   (C 回调/队列)     │
│                    └─────────┬──────────┘
│                              │
│            ┌─────────────────┼─────────────────┐
│            ▼                 ▼                  ▼
│    视频 H.265 NAL      音频 PCM/AAC        事件/控制
│     (已编码裸帧)        (已编码裸帧)        (移动侦测等)
│            │                 │
│            ▼                 ▼
│     [P2P Gateway 进程 (Rust)]
│     ┌──────────────────────────────────────────────┐
│     │                                                │
│     │  1. SDK 回调接入 (通过 FFI 调用 C SDK)        │
│     │     extern "C" fn video_callback(buf, len)    │
│     │     extern "C" fn audio_callback(buf, len)    │
│     │     输出: H.265 NAL units, PCM/AAC raw data   │
│     │                                                │
│     │  2. 媒体包封装                                 │
│     │     MediaPacket { track, timestamp, data }    │
│     │                                                │
│     │  3. libp2p 网络层                              │
│     │     Swarm (TCP+QUIC, Noise, Yamux)            │
│     │     relay::client + dcutr                      │
│     │     stream::Behaviour                          │
│     │                                                │
│     │  4. 每 viewer 两个 stream                      │
│     │     Stream 1: "/video/1.0.0" (视频帧)         │
│     │     Stream 2: "/audio/1.0.0" (音频帧)         │
│     │                                                │
│     └──────────────────────────────────────────────┘
```

**Cargo.toml**:
```toml
[package]
name = "p2p-camera-gateway"
version = "0.1.0"
edition = "2024"

[features]
# RV1106 SDK 支持 (通过 FFI 链接 .so)
rv1106 = []

[dependencies]
libp2p = { version = "0.57", features = [
    "tcp", "quic", "noise", "yamux",
    "relay", "dcutr", "identify", "ping",
    "stream",
] }
tokio = { version = "1", features = ["full"] }
futures = "0.3"
bytes = "1"
anyhow = "1"
tracing = "0.1"
tracing-subscriber = "0.3"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
reqwest = { version = "0.12", features = ["json"] }  # 信令 HTTP 客户端
crossbeam-channel = "0.5"  # C 回调 → Rust 线程安全传递
libc = "0.2"               # FFI

[build-dependencies]
# 链接 RV1106 SDK .so
# 或使用 bindgen 生成 FFI 绑定
```

**关键代码结构**:
```rust
// Gateway 进程: 四个任务并发
#[tokio::main]
async fn main() {
    // 获取 SDK 提供的回调
    // RV1106 SDK 通常在 C 线程中回调
    // 用 crossbeam-channel 把数据传到 Rust 侧

    // 任务1: libp2p Swarm 事件循环 (main thread)
    let swarm = build_swarm(/* config */).await;

    // 任务2: 接收 SDK 视频回调 → 封装 → broadcast
    let (video_tx, _) = broadcast::channel::<MediaPacket>(30);
    tokio::spawn(sdk_video_ingest(video_tx.clone()));

    // 任务3: 接收 SDK 音频回调 → 封装 → broadcast
    let (audio_tx, _) = broadcast::channel::<MediaPacket>(30);
    tokio::spawn(sdk_audio_ingest(audio_tx.clone()));

    // 任务4: 接受 viewer 视频流请求 → 转发帧
    tokio::spawn(video_stream_acceptor(
        stream_control,
        video_tx.subscribe(),
    ));

    // 任务5: 接受 viewer 音频流请求 → 转发帧
    tokio::spawn(audio_stream_acceptor(
        stream_control,
        audio_tx.subscribe(),
    ));
}
```

**SDK FFI 接入示例**:
```rust
// sdk_bridge.rs — 封装 RV1106 SDK C API

use crossbeam_channel::Sender;

/// 视频帧 (从 SDK 回调线程获取)
pub struct RawVideoFrame {
    pub data: Vec<u8>,      // H.265 NAL units
    pub timestamp_ms: u64,
    pub is_keyframe: bool,
    pub width: u32,
    pub height: u32,
}

/// 音频帧 (从 SDK 回调线程获取)
pub struct RawAudioFrame {
    pub data: Vec<u8>,      // PCM 16bit 或 AAC encoded
    pub timestamp_ms: u64,
    pub sample_rate: u32,   // e.g. 16000
    pub channels: u8,       // 1 (mono) or 2 (stereo)
    pub format: AudioFormat,
}

#[derive(Debug, Clone, Copy)]
pub enum AudioFormat {
    Pcm16le,
    Aac,
}

// 外部 C 回调 (由 RV1106 SDK 在独立线程中调用)
//
// Rust 侧通过 crossbeam_channel 或 RingBuf 接收数据:
//
// static VIDEO_SENDER: Lazy<Mutex<Option<Sender<RawVideoFrame>>>> = ...;
//
// extern "C" fn video_frame_callback(
//     data: *const u8, len: u32,
//     timestamp: u64, keyframe: bool,
// ) {
//     let frame = RawVideoFrame { ... };
//     VIDEO_SENDER.lock().as_ref().unwrap().send(frame).ok();
// }
```

**媒体包封装（音视频统一协议）**:

```
通用媒体包格式:

+--------+--------+--------+--------+--------+--------+--------+--------+
|  Track (1B)  |        Timestamp (u64 BE, ms)       |     Flags (1B)    |
+--------+--------+--------+--------+--------+--------+--------+--------+
|              Data Length (u32 BE)                   |  Data ...        |
+--------+--------+--------+--------+--------+--------+--------+--------+

Track:
  0x01 = 视频 (H.265)
  0x02 = 音频 (PCM/AAC)
  0x03-0xFF = 保留 (未来扩展: 字幕、控制等)

Flags (Track=0x01 视频):
  bit 0: 0=IDR关键帧, 1=非关键帧
  bit 1-7: 保留

Flags (Track=0x02 音频):
  bit 0-1: 音频格式 (0=PCM16LE, 1=AAC)
  bit 2-7: 保留
```

```rust
use bytes::{Buf, BufMut, Bytes, BytesMut};

#[derive(Debug, Clone)]
pub struct MediaPacket {
    pub track: MediaTrack,
    pub timestamp_ms: u64,
    pub flags: u8,
    pub data: Bytes,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum MediaTrack {
    Video = 0x01,
    Audio = 0x02,
}

impl MediaPacket {
    const HEADER_SIZE: usize = 1 + 8 + 1 + 4; // track + ts + flags + len

    pub fn video(timestamp_ms: u64, is_keyframe: bool, data: Bytes) -> Self {
        MediaPacket {
            track: MediaTrack::Video,
            timestamp_ms,
            flags: if is_keyframe { 0 } else { 1 },
            data,
        }
    }

    pub fn audio_pcm(timestamp_ms: u64, data: Bytes) -> Self {
        MediaPacket {
            track: MediaTrack::Audio,
            timestamp_ms,
            flags: 0, // PCM16LE
            data,
        }
    }

    pub fn encode(&self) -> Bytes {
        let payload_len = 8 + 1 + 4 + self.data.len();
        let mut buf = BytesMut::with_capacity(1 + payload_len);

        buf.put_u8(self.track as u8);
        buf.put_u64(self.timestamp_ms);
        buf.put_u8(self.flags);
        buf.put_u32(self.data.len() as u32);
        buf.extend_from_slice(&self.data);

        buf.freeze()
    }

    pub fn try_decode(buf: &mut BytesMut) -> Option<Self> {
        if buf.len() < Self::HEADER_SIZE { return None; }

        let track = match buf[0] {
            0x01 => MediaTrack::Video,
            0x02 => MediaTrack::Audio,
            _ => {
                // 跳过未知 track 类型
                if buf.len() >= 4 {  // at minimum need to read data_len
                    // skip entire packet
                    buf.clear(); // 简化处理
                }
                return None;
            }
        };

        let rest = &buf[1..];
        if rest.len() < 8 + 1 + 4 { return None; }

        let timestamp_ms = u64::from_be_bytes(rest[0..8].try_into().ok()?);
        let flags = rest[8];
        let data_len = u32::from_be_bytes(rest[9..13].try_into().ok()?) as usize;

        if rest.len() < 13 + data_len { return None; }

        // 消费整个包
        let total_len = 1 + 13 + data_len;
        let data = buf.copy_to_bytes(total_len);
        // 从 consumed data 中提取 payload
        let payload = data.slice(14..); // skip 1B track + 8B ts + 1B flags + 4B len

        Some(MediaPacket {
            track,
            timestamp_ms,
            flags,
            data: payload,
        })
    }
}
```

---

### 模块 3: Mobile APP (mobile-app/)

**部署**: Android (Kotlin/JNI) 或 iOS (Swift/C FFI)
**技术栈**: Rust 核心库 (FFI) + 平台原生 UI + 硬解码

**架构**:
```
┌─────────────────────────────────────────┐
│  Native UI Layer (Kotlin/Swift)         │
│  SurfaceView / MTKView                  │
├─────────────────────────────────────────┤
│  Platform Media Decoder                 │
│  MediaCodec (Android) / VideoToolbox(iOS)│
├─────────────────────────────────────────┤
│  Rust FFI Layer (.so / .a)              │
│  ┌─────────────────────────────────────┐│
│  │ p2p_viewer_lib (Rust)               ││
│  │                                     ││
│  │ - libp2p Swarm (QUIC + Noise)       ││
│  │ - relay::client + dcutr            ││
│  │ - stream::Behaviour                 ││
│  │ - MediaPacket decoder (音视频解复用)││
│  │ - Jitter Buffer (分离视频+音频队列) ││
│  │                                     ││
│  │ 暴露 C API:                         ││
│  │   init(relay_addr, relay_peer_id)   ││
│  │   connect(peer_id) → conn_id        ││
│  │   disconnect(conn_id)               ││
│  │   next_video_frame() → FrameData    ││
│  │   next_audio_frame() → FrameData    ││
│  │   send_keyframe_request(conn_id)    ││
│  └─────────────────────────────────────┘│
└─────────────────────────────────────────┘
```

**C FFI 接口定义**:
```rust
// mobile-app/rust-core/src/lib.rs

use std::ffi::{CStr, CString};
use std::os::raw::c_char;

#[repr(C)]
pub struct FrameData {
    pub data: *const u8,
    pub len: u32,
    pub timestamp_ms: u64,
    pub is_keyframe: bool,
}

#[no_mangle]
pub extern "C" fn p2p_init(
    relay_addr: *const c_char,
    relay_peer_id_s: *const c_char,
) -> *mut ViewerHandle {
    // 初始化 Swarm, 连接 Relay
}

#[no_mangle]
pub extern "C" fn p2p_connect(
    handle: *mut ViewerHandle,
    camera_peer_id: *const c_char,
) -> i32 {
    // 通过 Circuit 拨号 + 等待 DCUtR 直连
    // 返回 connection_id
}

#[no_mangle]
pub extern "C" fn p2p_next_frame(
    handle: *mut ViewerHandle,
    conn_id: i32,
) -> FrameData {
    // 从 jitter buffer 取下一帧
    // 阻塞等待直到有新帧或超时
}

#[no_mangle]
pub extern "C" fn p2p_free_frame(handle: *mut ViewerHandle, frame: FrameData) {
    // 释放帧内存
}

#[no_mangle]
pub extern "C" fn p2p_destroy(handle: *mut ViewerHandle) {
    // 清理资源
}
```

### Jitter Buffer 设计 (音视频分离)

```rust
use std::collections::VecDeque;
use std::time::{Duration, Instant};

struct AvJitterBuffer {
    video: TrackJitterBuffer,
    audio: TrackJitterBuffer,
    sync_threshold_ms: i64,  // 音视频同步阈值 (如 50ms)
}

struct TrackJitterBuffer {
    frames: VecDeque<QueuedPacket>,
    target_delay: Duration,   // 视频: 100ms, 音频: 50ms
    max_size: usize,           // 视频: 60帧, 音频: 200帧
    last_played_ts: Option<u64>,
}

struct QueuedPacket {
    packet: MediaPacket,
    arrived_at: Instant,
}

impl AvJitterBuffer {
    /// 获取下一视频帧 (与音频同步)
    fn next_video(&mut self) -> Option<MediaPacket> {
        let video = self.video.pop_next()?;

        // 音视频同步:
        // 如果视频领先音频超过 sync_threshold_ms, 等待音频
        if let Some(last_audio_ts) = self.audio.last_played_ts {
            let diff = video.timestamp_ms as i64 - last_audio_ts as i64;
            if diff > self.sync_threshold_ms {
                // 视频太快, 暂不输出 (下次再来取)
                self.video.push_front(video);
                return None;
            }
        }
        Some(video)
    }

    /// 获取下一音频帧 (与视频同步)
    fn next_audio(&mut self) -> Option<MediaPacket> {
        self.audio.pop_next()
        // 音频一般不做等待，避免卡顿
        // 如果音频落后视频太多, 可以加速播放或丢帧
    }

    fn push_video(&mut self, packet: MediaPacket) {
        self.video.push(packet);
    }

    fn push_audio(&mut self, packet: MediaPacket) {
        self.audio.push(packet);
    }
}

impl TrackJitterBuffer {
    fn pop_next(&mut self) -> Option<MediaPacket> {
        if self.frames.is_empty() { return None; }

        let buffered = self.buffered_duration();

        if self.last_played_ts.is_none() && buffered < self.target_delay {
            return None; // 首帧等待足够缓冲
        }

        let queued = self.frames.pop_front()?;
        self.last_played_ts = Some(queued.packet.timestamp_ms);

        // 追赶: 缓冲 > 2x target, 跳帧
        while self.buffered_duration() > self.target_delay * 2 {
            self.frames.pop_front();
        }

        Some(queued.packet)
    }

    fn push(&mut self, packet: MediaPacket) {
        if self.frames.len() >= self.max_size {
            self.frames.pop_front();
        }
        self.frames.push_back(QueuedPacket {
            packet,
            arrived_at: Instant::now(),
        });
    }

    fn push_front(&mut self, packet: MediaPacket) {
        self.frames.push_front(QueuedPacket {
            packet,
            arrived_at: Instant::now(),
        });
    }

    fn buffered_duration(&self) -> Duration {
        if self.frames.len() < 2 { return Duration::ZERO; }
        let first = self.frames.front().unwrap().packet.timestamp_ms;
        let last = self.frames.back().unwrap().packet.timestamp_ms;
        Duration::from_millis((last - first) as u64)
    }
}
```

---

## 三、媒体协议（音视频统一）

协议定义详见上面 **模块2 Gateway** 中的 `MediaPacket` 实现。核心设计：

```
统一包格式 (MediaPacket):
+------+--------+-------+--------+--------+
|Track |Timestamp|Flags  | DataLen| Data   |
| (1B) | (8B)    | (1B)  | (4B)   | (N B)  |
+------+--------+-------+--------+--------+

Track: 0x01=Video(H.265 NAL), 0x02=Audio(PCM/AAC)

控制通道复用 media stream 自身:
  - 视频 stream 标志位 bit 1=1 → 控制消息(KeyFrameRequest, BitrateRequest)
  - 音频 stream 保持纯数据

传输方式: 两个独立 libp2p-stream
  - Stream 协议 "/video/1.0.0" → 视频 MediaPacket
  - Stream 协议 "/audio/1.0.0" → 音频 MediaPacket
```

---

## 四、开发阶段划分

### Phase 1: Relay Server (3-5天)

```
任务:
├── [Day1] 搭建 relay-server 项目
│   ├── relay::Behaviour + identify
│   ├── TCP listen 4001
│   └── 基础事件循环
│
├── [Day2] HTTP 信令 API
│   ├── POST /register  (Gateway 注册 PeerId)
│   ├── GET /peers       (查询在线列表)
│   └── 简单内存存储
│
├── [Day3] 部署到云服务器
│   ├── Docker 化
│   ├── 开放端口 (4001, 8080)
│   └── 基础监控/日志
│
└── [验证] 用 hole-punching-tests 验证 Relay 工作正常
    目标: 两个测试节点通过 Relay 互相 Ping 通
```

### Phase 2: Gateway (RV1106) (7-10天)

```
任务:
├── [Day1-2] RV1106 Rust 交叉编译环境 + SDK FFI 验证
│   ├── 确认 target: armv7-unknown-linux-gnueabihf
│   ├── 链接 Rockchip SDK .so, 编写 FFI 绑定
│   ├── 验证: SDK 回调 → crossbeam_channel → Rust 收到数据
│   └── 分离视频/音频回调通道
│
├── [Day3-4] 实现 P2P Gateway 核心
│   ├── libp2p Swarm (TCP + QUIC)
│   ├── relay::client (预约 Relay)
│   ├── dcutr (直连升级)
│   └── 连接 Relay + 注册到信令 API
│
├── [Day5-6] 音视频打包 + stream 发送
│   ├── MediaPacket 封装 (音视频统一协议)
│   ├── broadcast channel 分发 (video_tx + audio_tx)
│   └── libp2p-stream: 2 个独立 stream (/video/1.0 + /audio/1.0)
│
├── [Day7-8] 音视频 Pipeline 联调
│   ├── SDK 视频回调 → MediaPacket → stream 发送
│   ├── SDK 音频回调 → MediaPacket → stream 发送
│   └── 本地模拟 viewer 验证音画同步
│
└── [Day9-10] RV1106 实测
    ├── 部署到设备
    ├── 验证 Relay Circuit 连通
    ├── 验证 DCUtR 直连
    ├── 音视频质量评估
    └── 性能测试 (CPU/内存/带宽)
```

### Phase 3: Mobile APP (7-10天)

```
任务:
├── [Day1-2] Rust FFI 核心库
│   ├── p2p_viewer_lib crate
│   ├── C API 导出 (视频+音频分离接口)
│   ├── Android: cargo-ndk 编译 .so
│   └── iOS: cargo-lipo 编译 .a
│
├── [Day3-4] libp2p 集成
│   ├── QUIC transport (移动端)
│   ├── relay::client + dcutr
│   ├── stream::Behaviour: 两个 stream (视频+音频)
│   └── MediaPacket 协议解析 + 音视频解复用
│
├── [Day5] Jitter Buffer 实现
│   ├── 视频帧缓冲队列 (target 100ms)
│   ├── 音频帧缓冲队列 (target 50ms)
│   ├── 音视频同步逻辑
│   └── 跳帧/丢帧策略
│
├── [Day6-7] Android APP (Kotlin)
│   ├── JNI 封装 Rust 库
│   ├── MediaCodec H.265 硬解码
│   ├── AudioTrack 音频播放
│   ├── 音视频同步渲染
│   └── UI (连接/断开/状态)
│
├── [Day8-9] iOS APP (Swift) - 可选
│   ├── C FFI 封装
│   ├── VideoToolbox H.265 硬解码
│   ├── AVAudioEngine 音频播放
│   ├── AVSampleBufferDisplayLayer 渲染
│   └── UI
│
└── [Day10] 端到端测试
    ├── 4G 网络测试
    ├── NAT 穿透成功率
    ├── 音视频同步验证
    └── 延迟/卡顿评估
```

### Phase 4: 优化 (持续)

```
├── [ ] 自适应码率控制
│   ├── 根据 RTT/丢包动态调整
│   └── 控制消息通知 Gateway 调整编码参数
│
├── [ ] 多路观看支持
│   ├── Gateway 单帧源 → 多 viewer 分发
│   └── QUIC stream 优先级 (关键帧优先)
│
├── [ ] 断线重连
│   ├── 连接断开自动重连
│   └── 重连期间发送 I 帧
│
├── [ ] 录制/回放 (可选)
│   └── Gateway 本地 SD 卡录制
│
└── [ ] 安全性增强
    ├── TLS 证书固定
    └── 信令 API 鉴权
```

---

## 五、项目目录结构

```
p2p-camera/
├── relay-server/              # 模块1: 中继服务器
│   ├── Cargo.toml
│   ├── Dockerfile
│   ├── src/
│   │   ├── main.rs            # 服务入口
│   │   ├── relay.rs           # Relay config
│   │   ├── api.rs             # HTTP 信令 API
│   │   └── behaviour.rs       # NetworkBehaviour 定义
│
├── gateway/                   # 模块2: RV1106 网关
│   ├── Cargo.toml
│   ├── cross.toml             # cross 编译配置
│   ├── build-armv7.sh         # 编译脚本
│   ├── src/
│   │   ├── main.rs            # 入口
│   │   ├── behaviour.rs       # NetworkBehaviour
│   │   ├── sdk_bridge.rs      # RV1106 SDK FFI 绑定
│   │   ├── media_packet.rs    # MediaPacket 协议
│   │   ├── stream_sender.rs   # 音视频发送任务
│   │   ├── signalling.rs      # HTTP 信令客户端
│   │   ├── stream_protocols.rs # 协议常量 (/video/1.0, /audio/1.0)
│   │   └── config.rs          # 配置
│
├── mobile-core/               # 模块3: 移动端 Rust 核心库
│   ├── Cargo.toml
│   ├── src/
│   │   ├── lib.rs             # FFI 导出 (视频+音频接口)
│   │   ├── viewer.rs          # P2pViewer 核心
│   │   ├── behaviour.rs       # NetworkBehaviour
│   │   ├── jitter_buffer.rs   # AvJitterBuffer (音视频分离)
│   │   ├── media_packet.rs    # MediaPacket 解析
│   │   └── stream_protocols.rs # 协议常量
│
├── mobile-android/            # Android APP
│   ├── app/
│   │   ├── build.gradle
│   │   └── src/main/
│   │       ├── java/.../P2pViewer.kt      # JNI 封装
│   │       ├── java/.../VideoDecoder.kt    # MediaCodec
│   │       └── java/.../MainActivity.kt    # UI
│   └── jniLibs/               # .so 文件
│
├── mobile-ios/                # iOS APP (可选)
│   ├── P2PViewer/
│   │   ├── P2pViewer.swift    # C FFI 封装
│   │   ├── VideoDecoder.swift # VideoToolbox
│   │   └── ContentView.swift  # UI
│   └── libs/                  # .a 文件
│
├── proto/                     # 共享协议定义 (所有模块共用)
│   ├── media_packet.rs        # MediaPacket 编码/解码
│   └── stream_protocols.rs    # stream 协议名称常量
│
└── docs/
    └── P2P_CAMERA_PLAN.md     # 本文档
```

---

## 六、关键技术决策

| 决策点 | 选择 | 理由 |
|--------|------|------|
| 传输层 | QUIC (非 TCP) | UDP 打洞成功率更高; 内置拥塞控制; 多路复用 |
| 加密 | Noise | libp2p 标准, 比 TLS 更轻量 |
| 数据获取 | Rockchip SDK FFI (非 RTSP) | 直接从编码器拿裸流, 零开销, 不经过 RTSP 封装 |
| 媒体封装 | 自定义 MediaPacket (非 RTP) | 监控场景单向, QUIC 已保证可靠有序; 音视频统一格式 |
| 音视频传输 | 两个独立 libp2p-stream | 方便移动端独立缓冲/解码; 可单独控制优先级 |
| NAT 中继 | libp2p Relay Circuit (非 TURN) | 原生配合 QUIC; TURN 是为 WebRTC ICE 设计的, 对 QUIC 无优势 |
| 移动解码 | 硬件解码 | MediaCodec/VideoToolbox + AudioTrack/AVAudioEngine |
| 编译 | Rust → ARM cross | RV1106 是 Linux, Rust 可直接交叉编译 |

---

## 七、NAT 穿透方案矩阵

| 场景 | 方案 | 延迟 | 带宽消耗 |
|------|------|------|----------|
| 同一局域网 | 直连 (QUIC) | <5ms | 无额外 |
| Camera 公网 IP | 直连 (QUIC) | <50ms | 无额外 |
| 双方不同 NAT | QUIC Hole Punch → 直连 | <100ms | 无额外 |
| Hole Punch 失败 | Relay Circuit 降级 | +50ms | Relay 服务器带宽 |
| 企业级对称 NAT | Relay Circuit (唯一方案) | +50ms+ | Relay 带宽 |

**Hole Punch 成功率参考**: 在家庭路由器场景下 ~80-90%, 企业防火墙 ~50-60%。

### 为什么不需要 TURN

```
传统 WebRTC 的 NAT 穿透链路:
  Peer A → STUN(获取公网地址) → ICE 打洞
       ↓ 失败
  Peer A → TURN(中继转发) → Peer B
  需要部署: STUN + TURN 两套服务

libp2p 的 NAT 穿透链路:
  Peer A → AutoNAT(检测NAT类型) → Relay Circuit → DCUtR → Hole Punch
       ↓ 失败(等价于 TURN)
  Peer A → Relay Circuit → Peer B
  需要部署: 只需要 Relay Server (一个进程搞定)
```

**libp2p Relay Circuit 实际就等价于 TURN 功能** — 当双方无法直连时，通过中继服务器转发全部流量 (包括 QUIC 连接)。区别在于:
- TURN 转发的是 IP 包，Relay Circuit 转发的是 libp2p 连接流
- Relay Circuit 自动由 DCUtR 触发, 不需要手动配置 ICE candidate
- Relay Server 同时承担信令和中继角色，减少服务数量

---

## 八、RV1106 资源评估

| 资源 | 需求 | 是否满足 |
|------|------|----------|
| CPU (Cortex-A7) | SDK 回调 + libp2p node | ✅ H.265 硬件编码, Gateway 只转发不解码 |
| RAM (建议 128MB+) | Rust binary ~10-20MB + libp2p ~5MB | ✅ 足够 |
| 存储 | Rust binary ~5-8MB (striped) | ✅ |
| 网络 | 视频 1-4Mbps + 音频 16-128kbps 上行 | ✅ 取决于编码参数 |

---

## 九、信令协议简化版

不需要复杂的 Redis/Kademlia, 用一个简单的 HTTP API:

```rust
// Relay Server HTTP API

// Gateway 启动时注册
POST /api/register
Body: { "peer_id": "12D3...", "device_id": "camera-01" }
Response: { "ok": true }

// Mobile APP 查询摄像头列表
GET /api/peers
Response: {
  "peers": [
    {
      "peer_id": "12D3...",
      "device_id": "camera-01", 
      "last_seen": 1234567890
    }
  ]
}

// Gateway 定时心跳
POST /api/heartbeat
Body: { "peer_id": "12D3..." }
Response: { "ok": true }
```

这样 Gateway 和 Mobile 只需要知道 Relay Server 的 IP 和 PeerId, 其余通过 HTTP API 自动发现。

---

## 十、快速验证原型 (2天内跑通)

```bash
# Day 1: 本地验证 Relay + Gateway 直连音视频传输

# 终端1: Relay Server
cd relay-server && cargo run
# 输出: Relay PeerId = 12D3KooW...

# 终端2: Gateway (本地模拟, 用 H.265 + PCM 文件代替 SDK 回调)
cd gateway && cargo run -- \
    --relay /ip4/127.0.0.1/tcp/4001/p2p/<relay_peer> \
    --video test.h265 \
    --audio test.pcm
# 输出: Gateway PeerId = 12D3KooX...

# 终端3: Viewer (本地模拟手机)
cd viewer-cli && cargo run -- \
    --relay /ip4/127.0.0.1/tcp/4001/p2p/<relay_peer> \
    --camera 12D3KooX...
# 输出: 保存视频到 output.h265 + audio.pcm, 播放验证
```

验证通过后:
1. 交叉编译到 RV1106 (armv7)
2. 替换文件输入为 Rockchip SDK FFI 回调
3. 实际设备测试
