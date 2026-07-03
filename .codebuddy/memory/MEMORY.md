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
