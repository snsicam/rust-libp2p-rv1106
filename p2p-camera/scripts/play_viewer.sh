#!/bin/bash
# play_viewer.sh — 编译或启动 viewer + SDL 实时播放
#
# 用法:
#   编译:   ./play_viewer.sh build
#   运行:   ./play_viewer.sh run <relay_addr> <gateway_peer>
#   一键:   ./play_viewer.sh all <relay_addr> <gateway_peer>
#
# 示例:
#   # 仅编译
#   ./play_viewer.sh build
#
#   # 仅运行 (需先编译)
#   ./play_viewer.sh run /ip4/127.0.0.1/tcp/4001/p2p/12D3KooW... 12D3KooW...
#
#   # 编译 + 运行
#   ./play_viewer.sh all /ip4/127.0.0.1/tcp/4001/p2p/12D3KooW... 12D3KooW...
#
# 前置条件 (Ubuntu):
#   sudo apt install clang libavcodec-dev libavformat-dev libavutil-dev \
#                    libswscale-dev libswresample-dev libavfilter-dev \
#                    libavdevice-dev libsdl2-dev

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
VIEWER_BIN="$PROJECT_ROOT/target/release/examples/viewer_cli"

# ---- 修复 ffmpeg-sys-next bindgen 找不到 GCC limits.h 的问题 ----
# libclang 默认不搜索 GCC 内部 include 目录, 需手动指定
setup_bindgen() {
    if [ -z "${BINDGEN_EXTRA_CLANG_ARGS:-}" ]; then
        GCC_INC="$(gcc -print-file-name=include 2>/dev/null)"
        if [ -n "$GCC_INC" ] && [ -d "$GCC_INC" ]; then
            export BINDGEN_EXTRA_CLANG_ARGS="-I$GCC_INC"
            echo "[INFO] Set BINDGEN_EXTRA_CLANG_ARGS=-I$GCC_INC"
        fi
    fi
}

# ---- 编译 viewer_cli (with player feature) ----
do_build() {
    setup_bindgen

    echo "[INFO] Building viewer_cli (with SDL player)..."
    cd "$PROJECT_ROOT"
    if ! cargo build --release --example viewer_cli -p mobile-core --features player; then
        echo "[ERROR] Build failed. See errors above."
        exit 1
    fi

    if [ ! -f "$VIEWER_BIN" ]; then
        echo "[ERROR] viewer_cli not found at $VIEWER_BIN"
        exit 1
    fi

    echo "[INFO] Build SUCCESS → $VIEWER_BIN"
}

# ---- 运行 viewer ----
do_run() {
    if [ $# -lt 2 ]; then
        echo "Usage: $0 run <relay_addr> <gateway_peer>"
        echo "Example: $0 run /ip4/127.0.0.1/tcp/4001/p2p/12D3KooW... 12D3KooW..."
        exit 1
    fi

    local relay_addr="$1"
    local gateway_peer="$2"

    if [ ! -f "$VIEWER_BIN" ]; then
        echo "[ERROR] viewer_cli not found at $VIEWER_BIN"
        echo "  Run '$0 build' first to compile."
        exit 1
    fi

    mkdir -p "$LOG_DIR"

    echo ""
    echo "============================================"
    echo "  P2P Camera Viewer (SDL Player)"
    echo "============================================"
    echo "  Relay:  $relay_addr"
    echo "  Camera: $gateway_peer"
    echo ""
    echo "  ESC / Close window to quit"
    echo "============================================"
    echo ""

    export RUST_LOG="${RUST_LOG:-info}"

    # 前台运行, Ctrl+C 或关窗退出
    "$VIEWER_BIN" \
        --relay "$relay_addr" \
        --camera "$gateway_peer" \
        --play \
        2>&1 | tee "$LOG_DIR/viewer.log"
}

# ---- 主入口 ----
case "${1:-}" in
    build)
        do_build
        ;;
    run)
        shift
        do_run "$@"
        ;;
    all)
        do_build
        echo ""
        shift
        do_run "$@"
        ;;
    *)
        echo "Usage: $0 {build|run|all} [args...]"
        echo ""
        echo "  build              Build viewer_cli (with SDL player)"
        echo "  run <relay> <peer> Run viewer (requires prior build)"
        echo "  all  <relay> <peer> Build + run in one step"
        echo ""
        echo "Examples:"
        echo "  $0 build"
        echo "  $0 run /ip4/127.0.0.1/tcp/4001/p2p/12D3KooW... 12D3KooW..."
        echo "  $0 all  /ip4/127.0.0.1/tcp/4001/p2p/12D3KooW... 12D3KooW..."
        exit 1
        ;;
esac
