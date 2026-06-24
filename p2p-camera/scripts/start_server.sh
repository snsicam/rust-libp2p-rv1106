#!/bin/bash
# start_server.sh — 启动 relay + gateway (视频从文件读取)
#
# 用法:
#   ./start_server.sh                      # 使用默认测试视频
#   ./start_server.sh /path/to/video.h265  # 指定视频文件
#
# 另一个终端运行 play_viewer.sh 连接:
#   ./play_viewer.sh <relay_addr> <gateway_peer>

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
mkdir -p "$LOG_DIR"

# 视频文件 (参数或默认)
VIDEO_FILE="${1:-$PROJECT_ROOT/p2p-camera/test-data/p2p-camera-test.h265}"

if [ ! -f "$VIDEO_FILE" ]; then
    echo "[ERROR] Video file not found: $VIDEO_FILE"
    echo "Usage: $0 [video_file]"
    echo "Generate test video with: ./scripts/run_test.sh"
    exit 1
fi

VSIZE=$(stat -c%s "$VIDEO_FILE" 2>/dev/null || stat -f%z "$VIDEO_FILE" 2>/dev/null)
echo "[INFO] Video: $VIDEO_FILE ($VSIZE bytes)"

# ---- 编译 ----
echo "[1/3] Building relay-server + gateway (debug)..."
cd "$PROJECT_ROOT"
cargo build -p relay-server -p gateway 2>&1 | tail -3

RELAY_BIN="$PROJECT_ROOT/target/debug/relay-server"
GATEWAY_BIN="$PROJECT_ROOT/target/debug/gateway"

# ---- 进程管理 ----
RELAY_PID=""
GATEWAY_PID=""

cleanup() {
    echo ""
    echo "[INFO] Stopping services..."
    [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
    [ -n "$GATEWAY_PID" ] && kill "$GATEWAY_PID" 2>/dev/null
    wait 2>/dev/null
    echo "[INFO] Stopped."
}
trap cleanup EXIT INT TERM

# ---- 启动 Relay ----
echo "[2/3] Starting relay-server (port 4001)..."
RELAY_KEY="$LOG_DIR/relay-server.key"
"$RELAY_BIN" --port 4001 --key-file "$RELAY_KEY" > "$LOG_DIR/relay.log" 2>&1 &
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
echo "      Relay PeerId: $RELAY_PEER"

RELAY_ADDR="/ip4/127.0.0.1/tcp/4001/p2p/$RELAY_PEER"

# ---- 启动 Gateway ----
echo "[3/3] Starting gateway..."
GATEWAY_KEY="$LOG_DIR/gateway.key"
"$GATEWAY_BIN" --relay "$RELAY_ADDR" --video-file "$VIDEO_FILE" --key-file "$GATEWAY_KEY" > "$LOG_DIR/gateway.log" 2>&1 &
GATEWAY_PID=$!

# 等待 gateway 就绪, 提取 PeerId
for i in $(seq 1 10); do
    sleep 0.5
    GATEWAY_PEER=$(grep -oP 'PeerId: \K[^\s]+' "$LOG_DIR/gateway.log" 2>/dev/null | head -1)
    [ -n "$GATEWAY_PEER" ] && break
done

if [ -z "$GATEWAY_PEER" ]; then
    echo "[ERROR] Failed to get gateway PeerId"
    cat "$LOG_DIR/gateway.log"
    exit 1
fi
echo "      Gateway PeerId: $GATEWAY_PEER"

# ---- 打印连接信息 ----
echo ""
echo "============================================"
echo "  P2P Camera Server Running"
echo "============================================"
echo "  Relay Addr:   $RELAY_ADDR"
echo "  Gateway Peer: $GATEWAY_PEER"
echo ""
echo "  在另一个终端运行:"
echo "    $SCRIPT_DIR/play_viewer.sh run $RELAY_ADDR $GATEWAY_PEER"
echo ""
echo "  Logs: $LOG_DIR/{relay,gateway}.log"
echo "  Ctrl+C to stop"
echo "============================================"
echo ""

# 保持运行
wait
