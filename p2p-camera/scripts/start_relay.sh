#!/bin/bash
# start_relay.sh — 在 PC/云服务器上启动 relay-server
#
# relay-server 跑在公网或局域网 PC 上, 为 RV1106 gateway 和 viewer 提供中继
#
# 用法:
#   ./start_relay.sh                    # 监听 0.0.0.0:4001
#   ./start_relay.sh --port 5001        # 指定端口
#
# 连接信息会打印出来, 供 RV1106 gateway 和 PC viewer 使用

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
mkdir -p "$LOG_DIR"

PORT=4001
KEY_FILE="$SCRIPT_DIR/relay-server.key"

# 解析参数
while [[ $# -gt 0 ]]; do
    case "$1" in
        --port) PORT="$2"; shift 2 ;;
        --key-file) KEY_FILE="$2"; shift 2 ;;
        *) echo "[ERROR] Unknown arg: $1"; exit 1 ;;
    esac
done

# ---- 编译 relay-server ----
echo "[1/2] Building relay-server (release)..."
cd "$PROJECT_ROOT"
cargo build --release -p relay-server 2>&1 | tail -3

RELAY_BIN="$PROJECT_ROOT/target/release/relay-server"

# ---- 进程管理 ----
RELAY_PID=""
cleanup() {
    echo ""
    echo "[INFO] Stopping relay..."
    [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
    wait 2>/dev/null
    echo "[INFO] Stopped."
}
trap cleanup EXIT INT TERM

# ---- 启动 Relay ----
echo "[2/2] Starting relay-server on port $PORT (key: $KEY_FILE)..."
"$RELAY_BIN" --port "$PORT" --key-file "$KEY_FILE" > "$LOG_DIR/relay.log" 2>&1 &
RELAY_PID=$!

# 等待 relay 就绪, 提取 PeerId
for i in $(seq 1 10); do
    sleep 0.5
    RELAY_PEER=$(grep -oP 'PeerId: \K[^\s]+' "$LOG_DIR/relay.log" 2>/dev/null | head -1)
    [ -n "$RELAY_PEER" ] && break
done

if [ -z "$RELAY_PEER" ]; then
    echo "[ERROR] Failed to get relay PeerId"
    cat "$LOG_DIR/relay.log"
    exit 1
fi

# 获取本机 IP (用于 RV1106 连接)
LOCAL_IP=$(hostname -I 2>/dev/null | awk '{print $1}' || echo "127.0.0.1")
RELAY_ADDR="/ip4/$LOCAL_IP/tcp/$PORT/p2p/$RELAY_PEER"

# ---- 打印连接信息 ----
echo ""
echo "============================================"
echo "  P2P Camera Relay Server Running"
echo "============================================"
echo "  Relay PeerId: $RELAY_PEER"
echo "  Relay Addr:   $RELAY_ADDR"
echo ""
echo "  ---- RV1106 上运行 (gateway) ----"
echo "  gateway --relay $RELAY_ADDR --video-file /tmp/test.h265"
echo ""
echo "  ---- PC 上运行 (viewer, SDL 播放) ----"
echo "  # 先编译 viewer"
echo "  $SCRIPT_DIR/play_viewer.sh build"
echo "  # 再运行 viewer"
echo "  $SCRIPT_DIR/play_viewer.sh run $RELAY_ADDR <GATEWAY_PEER>"
echo "  (GATEWAY_PEER 从 RV1106 gateway 启动日志获取)"
echo ""
echo "  Log: $LOG_DIR/relay.log"
echo "  Ctrl+C to stop"
echo "============================================"
echo ""

# 保持运行
wait
