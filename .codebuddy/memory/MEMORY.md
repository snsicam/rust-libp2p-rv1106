# Project Memory

## User's Project: P2P Camera Video Monitoring

> 完整文档入口: `p2p-camera/docs/README.md`

- **Camera**: Rockchip RV1106 chip, H.265 hardware encoding + audio (PCM/AAC)
- **Data source**: Rockchip SDK API direct callback (NOT RTSP)
- **Client**: Mobile native APP (Kotlin/Swift), NOT browser WASM
- **Goal**: P2P decentralized audio+video streaming using libp2p, save relay bandwidth
- **Tech choices**:
  - Transport: QUIC (UDP hole punch success rate higher than TCP)
  - Stream protocol: libp2p-stream (full-duplex), 2 separate streams (/video/1.0.0 + /audio/1.0.0)
  - Media encapsulation: Custom MediaPacket (1B track + 8B ts + 1B flags + 4B len + data)
  - Encryption: Noise (libp2p standard)
  - NAT traversal: Relay Circuit + DCUtR + LAN Direct + mDNS
  - TURN: NOT needed — libp2p Relay Circuit = TURN equivalent for QUIC
- **Architecture**: 3 modules - Relay Server (public cloud), DeviceCam (RV1106), Mobile APP (native)

## Key Documents

| Document | Path | Purpose |
|----------|------|---------|
| Project Plan | `p2p-camera/docs/P2P_CAMERA_PLAN.md` | Architecture, protocol design, module details |
| Bug Fix Log | `p2p-camera/docs/bugfix-log.md` | Bug history, design decisions, API caveats |
| NAT Traversal | `p2p-camera/docs/dcutr-vs-stun-turn.md` | DCUtR vs STUN/TURN comparison |
| SDD Specs | `.codeartsdoer/specs/` | Historical spec/design/tasks per feature |

## Key Design Decisions (from bugfix-log.md)

- **DeviceCam needs ping** (5s interval) to keep Relay connection alive
- **Viewer needs ping** (5s interval) to keep Relay alive during DCUtR attempts
- **ConnectionClosed must check num_established** to avoid misjudging DCUtR circuit close as disconnect
- **LAN direct detection** via Identify listen_addrs same-/24 subnet check
- **yamux Config**: Use `libp2p::yamux::Config::default` function pointer, NOT Config instance
- **Multi-relay**: `relays: Vec<String>` with `dial_pending: bool` to avoid duplicate dials
- **Exponential backoff**: 3s → 6s → 12s → 24s → 48s → 60s (max)
- **DCUtR 策略：仅 Symmetric NAT 才禁用（非粗粒度 4G 禁用）**: libp2p `dcutr::Behaviour` 在每次中继连接建立时自动打洞且运行时无法按 peer 关闭，故用 `Toggle<dcutr::Behaviour>`（`Toggle::from(Option)`）在初始化时包裹，事件类型不变。`Toggle` 在连接前就定好，但 NAT 类型需连接后（Identify 观测地址）才知，因此策略为：**默认启用 DCUtR（锥形/EIM NAT 含多数 4G 可打洞，省中继带宽）；连接后 `net_diag` 若确认 Symmetric，则在重连时禁用**。实现：lib 侧 `MediaPlayer` 存 `keypair` + `symmetric_detected`，`reconnect()` 时若确认 Symmetric 则 `build_swarm(keypair, false)` 重建 Swarm（PeerId 不变）；example 侧每次 session 新建 Swarm，重连时把 `enable_dcutr=false` 传入。NAT 类型在 Identify handler 与 summary 中均打印。注意：4G 并非都对称（GSMA 推荐 UDP EIM，WebRTC 通话即证），故**不应以 `network_type=="4g"` 粗粒度禁用**——**例外**：若用户配置**显式 forced `network_type="4g"`**（即已知在 4G/CGNAT 入站 UDP 被屏蔽），则 viewer 端 `enable_dcutr` 直接默认 false（且 NatDiagnosis 不再因非 Symmetric 而重新启用），因为此时打洞必然失败且会饿死中继视频流。这是 2026-07-20 实测 2.3fps 卡顿的根因之一。`net_diag::should_skip_dcutr`/`connection_strategy`/`dcutr_prediction` 均已改为仅 Symmetric 才 skip。
- **设备端写超时绝不能断开连接（关键教训）**: `device-cam/src/main.rs` 视频流写循环原本 `WRITE TIMEOUT(5s) → break` 断开 viewer，触发 viewer 重连风暴（每 16-25s 重连、每次仅传几帧），实测 viewer 平均仅 2.3fps/106kbps。已改为：`WRITE_TIMEOUT` 提到 15s，且超时**只丢帧 `continue` 不 `break`**；真正的断连由 `Ok(Err(e))`（流已死）分支处理。慢 relay viewer 应少收帧而非引发重连风暴。
- **关键帧判定只在 viewer 侧字节扫描, cam 完全不计算/不传 keyframe**: cam 侧 C 回调 `rk_camera.c frame_callback_t` 已**移除** `is_keyframe` 形参 (连同 `is_keyframe_h265/h264` 函数与 `get_stream_thread` 的 `is_kf` 计算一并删除), Rust `rk_video_source.rs on_frame` 签名改为 `fn(chn_id, data, len, pts_us)`, `media_source.rs` 与 `rk_video_source.rs` 均 `MediaPacket::video(ts, data)` (无 is_keyframe 形参)。**`MediaPacket` 的 `is_keyframe()` 方法与 `video()` 的 `is_keyframe` 参数已删除**: 视频包 `flags` 字节保留置 0 (仅音频包用 flags 区分 codec: PCM/AAC/G711A/G711U), 关键帧完全由 viewer 字节扫描 `is_nal_keyframe` (H.265 IRAP 16-21 / H.264 IDR 5) 判定 (receiver 自包含、不信任对端 flag)。**JNI 桥 `nativePollVideoFrame/AudioFrame` 只转发 `[PTS 8B]+[raw data]`, 不转发 flags/is_keyframe 给原生 APP**。注意: `MediaPacket` 的音频 `flags` 字段目前也**未**经 JNI 转发给 Kotlin/Swift, 属潜在 bug (原生 APP 无法区分 PCM/G711/AAC)。
