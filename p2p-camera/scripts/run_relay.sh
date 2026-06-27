#!/bin/bash
# run_relay.sh — 启动 Relay Server
set -e

cd "$(dirname "$0")/../.."
MODE="${1:-debug}"
PORT=4001
SEED=42

# 根据模式定位二进制
if [ "$MODE" = "release" ]; then
    BIN="target/release/relay-server"
else
    BIN="target/debug/relay-server"
fi

if [ ! -f "$BIN" ]; then
    echo "[ERROR] Binary not found. Run ./scripts/build_relay.sh $MODE first."
    exit 1
fi

echo "=========================================="
echo "  Starting Relay Server ($MODE)"
echo "=========================================="

# 启动并前台运行 (日志输出到终端，方便查看 PeerId 和连接状态)
RUST_LOG=info "$BIN" --port $PORT --secret-key-seed $SEED
