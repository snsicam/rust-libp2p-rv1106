#!/bin/bash
# play_viewer.sh — 启动 viewer + SDL 实时播放
#
# 用法:
#   ./play_viewer.sh <relay_addr> <gateway_peer>
#
# 示例:
#   ./play_viewer.sh /ip4/127.0.0.1/tcp/4001/p2p/12D3KooW... 12D3KooW...
#
# 前置条件 (Ubuntu):
#   sudo apt install clang libavcodec-dev libavformat-dev libavutil-dev \
#                    libswscale-dev libswresample-dev libavfilter-dev \
#                    libavdevice-dev libsdl2-dev

set -euo pipefail

if [ $# -lt 2 ]; then
    echo "Usage: $0 <relay_addr> <gateway_peer>"
    echo "Example: $0 /ip4/127.0.0.1/tcp/4001/p2p/12D3KooW... 12D3KooW..."
    exit 1
fi

RELAY_ADDR="$1"
GATEWAY_PEER="$2"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
mkdir -p "$LOG_DIR"

# ---- 修复 ffmpeg-sys-next bindgen 找不到 GCC limits.h 的问题 ----
# libclang 默认不搜索 GCC 内部 include 目录, 需手动指定
if [ -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]; then
    GCC_INC="$(gcc -print-file-name=include 2>/dev/null)"
    if [ -n "$GCC_INC" ] && [ -d "$GCC_INC" ]; then
        export BINDGEN_EXTRA_CLANG_ARGS="-I$GCC_INC"
        echo "[INFO] Set BINDGEN_EXTRA_CLANG_ARGS=-I$GCC_INC"
    fi
fi

# ---- 编译 viewer_cli (with player feature) ----
echo "[INFO] Building viewer_cli (with SDL player)..."
cd "$PROJECT_ROOT"
if ! cargo build --release --example viewer_cli -p mobile-core --features player; then
    echo "[ERROR] Build failed. See errors above."
    exit 1
fi

VIEWER_BIN="$PROJECT_ROOT/target/release/examples/viewer_cli"

if [ ! -f "$VIEWER_BIN" ]; then
    echo "[ERROR] viewer_cli not found at $VIEWER_BIN"
    exit 1
fi

# ---- 启动 ----
echo ""
echo "============================================"
echo "  P2P Camera Viewer (SDL Player)"
echo "============================================"
echo "  Relay:  $RELAY_ADDR"
echo "  Camera: $GATEWAY_PEER"
echo ""
echo "  ESC / Close window to quit"
echo "============================================"
echo ""

export RUST_LOG="${RUST_LOG:-info}"

# 前台运行, Ctrl+C 或关窗退出
"$VIEWER_BIN" \
    --relay "$RELAY_ADDR" \
    --camera "$GATEWAY_PEER" \
    --play \
    2>&1 | tee "$LOG_DIR/viewer.log"
