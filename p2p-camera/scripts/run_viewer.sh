#!/bin/bash
# run_viewer.sh — 启动 viewer_cli 并实时播放
#
# 用法: ./run_viewer.sh <relay_addr> <device_cam_peer> [udp_port]
#
# 示例:
#   ./run_viewer.sh /ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1 12D3KooWCncxppx5oic2SssjWgcgG3e3xkt5P59mPRnPhMDopFHF
#   ./run_viewer.sh /ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1 12D3KooWCncxppx5oic2SssjWgcgG3e3xkt5P59mPRnPhMDopFHF 34501
#
# 必需参数:
#   relay_addr       - Relay 服务器的 Multiaddr
#   device_cam_peer  - DeviceCam 的 PeerId
#
# 可选参数:
#   udp_port         - Viewer 的 QUIC UDP 监听端口（固定，便于端口映射）
#
# 注意: 外部地址由 identify 协议自动发现（公网 IP）和 NewListenAddr 事件自动注入（本地 IP），
#       无需手动指定 --external-ip。
#
# 前置条件: 已运行 build_viewer.sh 编译成功

set -euo pipefail

if [ $# -lt 2 ]; then
    echo "Usage: $0 <relay_addr> <device_cam_peer> [udp_port]"
    echo ""
    echo "Example:"
    echo "  $0 /ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1 12D3KooWCncxppx5oic2SssjWgcgG3e3xkt5P59mPRnPhMDopFHF"
    echo "  $0 /ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1 12D3KooWCncxppx5oic2SssjWgcgG3e3xkt5P59mPRnPhMDopFHF 34501"
    exit 1
fi

RELAY_ADDR="$1"
DEVICE_CAM_PEER="$2"
UDP_PORT="${3:-}"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
VIEWER_BIN="$PROJECT_ROOT/target/debug/examples/viewer_cli"
LOG_DIR="$SCRIPT_DIR/logs"

if [ ! -f "$VIEWER_BIN" ]; then
    echo "[ERROR] viewer_cli not found at $VIEWER_BIN"
    echo "  Please run './build_viewer.sh' first to compile."
    exit 1
fi

mkdir -p "$LOG_DIR"

echo ""
echo "============================================"
echo "  P2P Camera Viewer (SDL Player)"
echo "============================================"
echo "  Relay:      $RELAY_ADDR"
echo "  DeviceCam:  $DEVICE_CAM_PEER"
if [ -n "$UDP_PORT" ]; then
echo "  UDP Port:   $UDP_PORT"
fi
echo ""
echo "  ESC / Close window to quit"
echo "============================================"
echo ""

export RUST_LOG="${RUST_LOG:+$RUST_LOG,}libp2p_dcutr=debug,libp2p_relay=debug"

# 构建命令参数
VIEWER_ARGS=(
    --relay "$RELAY_ADDR"
    --camera "$DEVICE_CAM_PEER"
)
if [ -n "$UDP_PORT" ]; then
    VIEWER_ARGS+=(--udp-port "$UDP_PORT")
fi

# 检测是否支持 --play (需要 --features player 编译)
if "$VIEWER_BIN" --help 2>&1 | grep -q -- '--play'; then
    VIEWER_ARGS+=(--play)
fi

# 前台运行, Ctrl+C 或关窗退出
"$VIEWER_BIN" "${VIEWER_ARGS[@]}" \
    2>&1 | tee "$LOG_DIR/viewer.log"
