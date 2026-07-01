#!/bin/bash
# run_relay.sh — 启动 Relay Server
#
# 用法:
#   方式1 (配置文件): ./run_relay.sh
#     → 首次运行自动生成 relay-server.toml，编辑后重启
#
#   方式2 (命令行覆盖): ./run_relay.sh [options]
#     → 命令行参数覆盖配置文件中的值
#
# 示例:
#   ./run_relay.sh
#   ./run_relay.sh --port 5001
#   ./run_relay.sh --public-ip 1.2.3.4
#
# 日志: 输出到终端同时写入 scripts/logs/relay.log

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR" && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
RELAY_BIN="$PROJECT_ROOT/relay-server"

if [ ! -f "$RELAY_BIN" ]; then
    echo "[ERROR] relay-server not found at $RELAY_BIN"
    echo "  Please run './build_relay.sh' first to compile."
    exit 1
fi

mkdir -p "$LOG_DIR"

echo ""
echo "============================================"
echo "  P2P Camera Relay Server"
echo "============================================"
echo "  Binary:  $RELAY_BIN"
if [ $# -gt 0 ]; then
echo "  Args:    $*"
else
echo "  Config:  relay-server.toml"
fi
echo ""
echo "  Ctrl+C to stop"
echo "============================================"
echo ""

export RUST_LOG="${RUST_LOG:-info,}libp2p_dcutr=debug,libp2p_relay=debug"

# 前台运行, 输出到终端同时写日志
# 透传所有命令行参数 (如 --port, --public-ip 等)
"$RELAY_BIN" "$@" \
    2>&1 | tee "$LOG_DIR/relay.log"
