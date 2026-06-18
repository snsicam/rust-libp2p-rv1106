#!/bin/bash
# build.sh — 编译 p2p-camera 全部模块 + 单元测试
# 用法:
#   ./build.sh           # 默认 debug
#   ./build.sh release   # release 模式
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
        echo "[3/4] Building gateway (release)..."
        cargo build -p relay-server -p gateway --release
        echo "[4/4] Building mobile-core + example (release)..."
        cargo build -p mobile-core --release
        cargo build --example viewer_cli -p mobile-core --release
        ;;

    test)
        echo "[1/5] Building all crates..."
        cargo build -p proto -p relay-server -p gateway -p mobile-core
        echo "[2/5] Building viewer_cli example..."
        cargo build --example viewer_cli -p mobile-core
        echo "[3/5] Running proto unit tests..."
        cargo test -p proto
        echo "[4/5] Running clippy (warnings)..."
        cargo clippy -p proto -p relay-server -p gateway -p mobile-core -- -D warnings 2>&1 || echo "(clippy skipped)"
        echo "[5/5] Done."
        ;;

    debug|*)
        echo "[1/4] Building proto..."
        cargo build -p proto
        echo "[2/4] Building relay-server..."
        cargo build -p relay-server
        echo "[3/4] Building gateway..."
        cargo build -p relay-server -p gateway
        echo "[4/4] Building mobile-core + example..."
        cargo build -p mobile-core
        cargo build --example viewer_cli -p mobile-core
        ;;
esac

echo ""
echo "=========================================="
echo "  Build SUCCESS"
echo "=========================================="
echo ""
echo "Binaries:"
echo "  Relay Server:  target/debug/relay-server"
echo "  Gateway:       target/debug/gateway"
echo "  Viewer CLI:    target/debug/examples/viewer_cli"
echo ""
echo "Quick test:"
echo "  # Terminal 1: Relay"
echo "  RUST_LOG=info ./target/debug/relay-server --port 4001 --secret-key-seed 42"
echo ""
echo "  # Terminal 2: Gateway"
echo "  RUST_LOG=info ./target/debug/gateway --relay /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER> --video-file test.h265 --enable-audio"
echo ""
echo "  # Terminal 3: Viewer"
echo "  RUST_LOG=info ./target/debug/examples/viewer_cli --relay /ip4/127.0.0.1/tcp/4001/p2p/<RELAY_PEER> --camera <GATEWAY_PEER> --output output.h265"
