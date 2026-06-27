#!/bin/bash
# build_viewer.sh — 仅编译 viewer_cli (with SDL player)
#
# 用法: ./build_viewer.sh
#
# 前置条件 (Ubuntu):
#   sudo apt install clang libavcodec-dev libavformat-dev libavutil-dev \
#                    libswscale-dev libswresample-dev libavfilter-dev \
#                    libavdevice-dev libsdl2-dev

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# ---- 修复 ffmpeg-sys-next bindgen 找不到 GCC limits.h 的问题 ----
setup_bindgen() {
    if [ -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]; then
        GCC_INC="$(gcc -print-file-name=include 2>/dev/null)"
        if [ -n "$GCC_INC" ] && [ -d "$GCC_INC" ]; then
            export BINDGEN_EXTRA_CLANG_ARGS="-I$GCC_INC"
            echo "[INFO] Set BINDGEN_EXTRA_CLANG_ARGS=-I$GCC_INC"
        fi
    fi
}

setup_bindgen

echo "[INFO] Building viewer_cli (with SDL player, debug)..."
cd "$PROJECT_ROOT"
if ! cargo build --example viewer_cli -p mobile-core --features player; then
    echo "[ERROR] Build failed. See errors above."
    exit 1
fi

VIEWER_BIN="$PROJECT_ROOT/target/debug/examples/viewer_cli"
if [ ! -f "$VIEWER_BIN" ]; then
    echo "[ERROR] viewer_cli not found at $VIEWER_BIN"
    exit 1
fi

echo "[INFO] Build SUCCESS → $VIEWER_BIN"
