# P2P 摄像头项目文档

> 本目录是项目唯一文档入口，AI 助手和开发者应从此处开始。

## 文档索引

| 文档 | 内容 | 何时阅读 |
|------|------|---------|
| [P2P_CAMERA_PLAN.md](P2P_CAMERA_PLAN.md) | 项目概览、总体架构、模块设计、协议定义、关键技术决策 | 首次了解项目 |
| [bugfix-log.md](bugfix-log.md) | Bug 修复记录、关键设计决策、API 注意事项 | 排查问题或修改代码前 |
| [dcutr-vs-stun-turn.md](dcutr-vs-stun-turn.md) | DCUtR 与 STUN/TURN NAT 穿透方案对比 | 理解 NAT 穿透选型 |

## 项目速览

- **硬件**: Rockchip RV1106 (Cortex-A7, H.265 硬编码)
- **数据源**: Rockchip SDK API 直接回调 (非 RTSP)
- **客户端**: 移动端原生 APP (Kotlin/Swift)
- **目标**: P2P 去中心化音视频传输，节省中继带宽

## 三模块架构

```
Relay Server (公网云) ← TCP/QUIC → DeviceCam (RV1106) ← Circuit/直连 → Viewer (Mobile)
```

- **Relay Server**: 电路中继 + Identify 观测 + DCUtR 协调
- **DeviceCam**: SDK 回调 → MediaPacket → libp2p-stream 发送
- **Viewer**: libp2p-stream 接收 → 解复用 → 硬解码播放

## 关键技术选型

| 决策 | 选择 | 理由 |
|------|------|------|
| 传输 | QUIC (非 TCP) | UDP 打洞成功率更高 |
| 加密 | Noise | libp2p 标准 |
| 媒体封装 | 自定义 MediaPacket | QUIC 已保证可靠有序，无需 RTP |
| NAT 中继 | Relay Circuit (非 TURN) | 原生配合 QUIC，无需 coturn |
| NAT 穿透 | DCUtR + LAN 直连检测 + mDNS | 多策略覆盖 |
| 配置 | TOML 文件 + CLI 覆盖 | 嵌入式设备友好 |

## 源码目录

```
p2p-camera/
├── relay-server/          # 中继服务器
│   └── src/{main.rs, config.rs, behaviour.rs}
├── device-cam/            # RV1106 摄像头端
│   └── src/{main.rs, config.rs, behaviour.rs, net_diag.rs}
├── mobile-core/           # 移动端 Rust 核心库
│   ├── src/{viewer.rs, net_diag.rs, media_packet.rs}
│   └── examples/viewer_cli.rs
└── docs/                  # 本目录
```

## 历史设计文档

SDD (Spec-Driven Development) 过程文档保存在 `.codeartsdoer/specs/` 下，供参考：

| Spec | 功能 | 状态 |
|------|------|------|
| `dcutr_enhance` | DCUtR 调试日志增强、NAT 诊断、连接质量评估 | 已完成 |
| `dcutr_fix` | LAN 直连检测、Identify 事件缓存 | 已完成 |
| `rustlog_dbg` | RUST_LOG 调试配置 | 已完成 |
| `mdns_relay` | mDNS 局域网优先、多 Relay 并发拨号 | 已完成 |
