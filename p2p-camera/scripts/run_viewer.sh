#!/bin/bash
# run_viewer.sh — 启动 viewer_cli 并实时播放
#
# 用法: ./run_viewer.sh <relay_addr> <gateway_peer> <udp_port> <external_ip>
#
# 示例:
#   ./run_viewer.sh /ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1 12D3KooWCncxppx5oic2SssjWgcgG3e3xkt5P59mPRnPhMDopFHF 34501 203.0.113.10
#
# 必需参数:
#   relay_addr    - Relay 服务器的 Multiaddr
#   gateway_peer  - Gateway 的 PeerId
#   udp_port      - Viewer 的 QUIC UDP 监听端口（固定，需在路由器做端口映射）
#   external_ip   - Viewer 所在网络的公网 IP
#
# 前置条件: 已运行 build_viewer.sh 编译成功

set -euo pipefail

if [ $# -lt 4 ]; then
    echo "Usage: $0 <relay_addr> <gateway_peer> <udp_port> <external_ip>"
    echo ""
    echo "Example:"
    echo "  $0 /ip4/101.35.90.171/udp/4001/quic-v1/p2p/12D3KooWDGUejVsts1G4tGyf8ukkr73eWxCr1EUfCQxgcbSDUie1 12D3KooWCncxppx5oic2SssjWgcgG3e3xkt5P59mPRnPhMDopFHF 34501 203.0.113.10"
    exit 1
fi

RELAY_ADDR="$1"
GATEWAY_PEER="$2"
UDP_PORT="$3"
EXTERNAL_IP="$4"

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
echo "  Gateway:    $GATEWAY_PEER"
echo "  UDP Port:   $UDP_PORT"
echo "  External IP: $EXTERNAL_IP"
echo ""
echo "  ESC / Close window to quit"
echo "============================================"
echo ""

export RUST_LOG="${RUST_LOG:-info}"

# 前台运行, Ctrl+C 或关窗退出
"$VIEWER_BIN" \
    --relay "$RELAY_ADDR" \
    --camera "$GATEWAY_PEER" \
    --udp-port "$UDP_PORT" \
    --external-ip "$EXTERNAL_IP" \
    --play \
    2>&1 | tee "$LOG_DIR/viewer.log"
