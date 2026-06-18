#!/bin/bash
# run_test.sh — 端到端测试脚本 (一键启动三个终端)
#
# 前置条件:
#   1. 已运行 ./build.sh 编译成功
#   2. 已安装 ffmpeg (用于生成测试视频)
#
# 用法:
#   ./run_test.sh              # 完整端到端测试
#   ./run_test.sh --no-video   # 跳过视频文件 (仅验证连接)

set -e

cd "$(dirname "$0")/.."

BIN_DIR="target/debug"
VIDEO_FILE="/tmp/test.h265"
OUTPUT_FILE="/tmp/output.h265"
PORT=4001
SEED=42

echo "=========================================="
echo "  P2P-Camera End-to-End Test"
echo "=========================================="

# ---- 0. 检查编译产物 ----
if [ ! -f "$BIN_DIR/relay-server" ] || [ ! -f "$BIN_DIR/gateway" ]; then
    echo "[ERROR] Binaries not found. Run ./scripts/build.sh first."
    exit 1
fi

# ---- 1. 生成测试视频 ----
if [ "$1" != "--no-video" ] && [ ! -f "$VIDEO_FILE" ]; then
    echo "[1/5] Generating test H.265 video..."
    if ! command -v ffmpeg &> /dev/null; then
        echo "[ERROR] ffmpeg not installed. Install with: sudo apt install ffmpeg"
        exit 1
    fi
    ffmpeg -y -f lavfi -i "testsrc=duration=30:size=640x480:rate=25" \
        -c:v libx265 -x265-params "keyint=50:min-keyint=50" \
        -f hevc "$VIDEO_FILE" 2>/dev/null
    echo "      Generated: $VIDEO_FILE"
else
    echo "[1/5] Skipping video generation"
fi

# ---- 2. 启动 Relay Server ----
echo "[2/5] Starting Relay Server..."
RELAY_LOG=$(mktemp)
$BIN_DIR/relay-server --port $PORT --secret-key-seed $SEED > "$RELAY_LOG" 2>&1 &
RELAY_PID=$!
sleep 2

# 提取 Relay PeerId
RELAY_PEER=$(grep -oP 'PeerId: \K[^\s]+' "$RELAY_LOG" | head -1)
if [ -z "$RELAY_PEER" ]; then
    echo "[ERROR] Failed to get Relay PeerId"
    cat "$RELAY_LOG"
    kill $RELAY_PID 2>/dev/null
    exit 1
fi
RELAY_ADDR="/ip4/127.0.0.1/tcp/$PORT/p2p/$RELAY_PEER"
echo "      Relay PeerId: $RELAY_PEER"
echo "      Relay Addr:   $RELAY_ADDR"

# ---- 3. 启动 Gateway ----
echo "[3/5] Starting Gateway..."
GATEWAY_LOG=$(mktemp)
if [ -f "$VIDEO_FILE" ]; then
    $BIN_DIR/gateway --relay "$RELAY_ADDR" --video-file "$VIDEO_FILE" --enable-audio > "$GATEWAY_LOG" 2>&1 &
else
    $BIN_DIR/gateway --relay "$RELAY_ADDR" > "$GATEWAY_LOG" 2>&1 &
fi
GATEWAY_PID=$!
sleep 3

# 提取 Gateway PeerId
GATEWAY_PEER=$(grep -oP 'PeerId: \K[^\s]+' "$GATEWAY_LOG" | head -1)
if [ -z "$GATEWAY_PEER" ]; then
    echo "[ERROR] Failed to get Gateway PeerId"
    cat "$GATEWAY_LOG"
    kill $RELAY_PID $GATEWAY_PID 2>/dev/null
    exit 1
fi
echo "      Gateway PeerId: $GATEWAY_PEER"

# ---- 4. 启动 Viewer (运行10秒后退出) ----
echo "[4/5] Starting Viewer (10 second test)..."
VIEWER_LOG=$(mktemp)

if [ -f "$BIN_DIR/examples/viewer_cli" ]; then
    timeout 10 $BIN_DIR/examples/viewer_cli \
        --relay "$RELAY_ADDR" \
        --camera "$GATEWAY_PEER" \
        --output "$OUTPUT_FILE" > "$VIEWER_LOG" 2>&1 || true
    
    echo "      Viewer output:"
    cat "$VIEWER_LOG"
    
    # ---- 5. 验证结果 ----
    echo "[5/5] Checking results..."
    if [ -f "$OUTPUT_FILE" ]; then
        SIZE=$(stat -c%s "$OUTPUT_FILE" 2>/dev/null || stat -f%z "$OUTPUT_FILE" 2>/dev/null)
        echo "      Output file: $OUTPUT_FILE ($SIZE bytes)"
        if [ "$SIZE" -gt 0 ]; then
            echo ""
            echo "=========================================="
            echo "  ✅ TEST PASSED - Video received!"
            echo "=========================================="
            echo "  Play with: ffplay -f hevc $OUTPUT_FILE"
        else
            echo ""
            echo "=========================================="
            echo "  ❌ TEST FAILED - Empty output"
            echo "=========================================="
        fi
    else
        echo "      No output file generated"
        echo ""
        echo "=========================================="
        echo "  ⚠️  No video file test (--no-video mode)"
        echo "  Check logs for connection status"
        echo "=========================================="
        echo ""
        echo "Relay log:"
        cat "$RELAY_LOG" | tail -20
        echo ""
        echo "Gateway log:"
        cat "$GATEWAY_LOG" | tail -20
        echo ""
        echo "Viewer log:"
        cat "$VIEWER_LOG" | tail -20
    fi
else
    echo "      viewer_cli not found, running manual mode"
    echo "      Press Ctrl+C to stop"
    sleep 30
fi

# ---- 清理 ----
echo ""
echo "Cleaning up..."
kill $RELAY_PID $GATEWAY_PID 2>/dev/null || true
rm -f "$RELAY_LOG" "$GATEWAY_LOG" "$VIEWER_LOG"
echo "Done."
