#!/bin/bash
# run_relay.sh — 启动 Relay Server
set -e

cd "$(dirname "$0")/"
MODE="${1:-debug}"
PORT=4001
KEY_FILE="$(cd "$(dirname "$0")" && pwd)/relay-server.key"

# 根据模式定位二进制
if [ "$MODE" = "release" ]; then
    BIN="./relay-server"
else
    BIN="./relay-server"
fi

if [ ! -f "$BIN" ]; then
    echo "[ERROR] Binary not found. Run ./build_relay.sh $MODE first."
    exit 1
fi

echo "=========================================="
echo "  Starting Relay Server ($MODE)"
echo "=========================================="

# 启动并前台运行 (日志输出到终端，方便查看 PeerId 和连接状态)
export RUST_LOG="${RUST_LOG:-info,}libp2p_dcutr=debug,libp2p_relay=debug"
exec "$BIN" --port $PORT --key-file "$KEY_FILE"
