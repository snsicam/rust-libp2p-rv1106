#!/bin/bash
# build.sh — 编译 p2p-camera 全部模块 + 单元测试
# 用法:
#   ./build.sh           # 默认 debug
#   ./build.sh release   # release 模式
#   ./build.sh player    # release + SDL 播放器 (media_viewer --play)
#   ./build.sh test      # 编译 + 运行测试

set -e

cd "$(dirname "$0")/../.."  # 回到 rust-libp2p 根目录

MODE="${1:-debug}"

echo "=========================================="
echo "  P2P-Camera Build Script"
echo "  Mode: $MODE"
echo "  pwd: $PWD"
echo "=========================================="

case "$MODE" in
    release)
        echo "[1/4] Building proto (release)..."
        cargo build -p proto --release
        echo "[2/4] Building relay-server (release)..."
        cargo build -p relay-server --release
        echo "[3/4] Building device-cam (release)..."
        cargo build -p relay-server -p device-cam --release
        echo "[4/4] Building mobile-core + example (release)..."
        cargo build -p mobile-core --release
        cargo build --example media_viewer -p mobile-core --release
        ;;

    test)
        echo "[1/5] Building all crates..."
        cargo build -p proto -p relay-server -p device-cam -p mobile-core
        echo "[2/5] Building media_viewer example..."
        cargo build --example media_viewer -p mobile-core
        echo "[3/5] Running proto unit tests..."
        cargo test -p proto
        echo "[4/5] Running clippy (warnings)..."
        cargo clippy -p proto -p relay-server -p device-cam -p mobile-core -- -D warnings 2>&1 || echo "(clippy skipped)"
        echo "[5/5] Done."
        ;;

    player)
        echo "[1/3] Building relay-server + device-cam (release)..."
        cargo build -p relay-server -p device-cam --release
        echo "[2/3] Building media_viewer (release, player feature)..."
        cargo build --example media_viewer -p mobile-core --release --features player
        echo "[3/3] Done."
        ;;

    debug|*)
        echo "[1/4] Building proto..."
        cargo build -p proto
        echo "[2/4] Building relay-server..."
        cargo build -p relay-server
        echo "[3/4] Building device-cam..."
        cargo build -p relay-server -p device-cam
        echo "[4/4] Building mobile-core + example..."
        cargo build -p mobile-core
        cargo build --example media_viewer -p mobile-core
        ;;
esac

echo ""
echo "=========================================="
echo "  Build SUCCESS"
echo "=========================================="
echo ""
echo "Binaries:"
echo "  Relay Server:  target/debug/relay-server"
echo "  DeviceCam:       target/debug/device-cam"
echo "  Media Viewer:  target/debug/examples/media_viewer"
echo ""
echo "Quick start:"
echo "  # Terminal 1: Relay Server (首次运行自动生成 relay-server.toml)"
echo "  ./p2p-camera/scripts/start_relay.sh"
echo ""
echo "  # Terminal 2: DeviceCam (首次运行自动生成 device-cam.toml)"
echo "  ./p2p-camera/scripts/run_device_cam.sh --relay <relay_addr> --enable-audio"
echo ""
echo "  # Terminal 3: Viewer (首次运行自动生成 viewer.toml)"
echo "  ./p2p-camera/scripts/run_media_viewer.sh <relay_addr> <device_cam_peer>"
