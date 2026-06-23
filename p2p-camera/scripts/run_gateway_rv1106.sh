#!/bin/bash
# run_gateway_rv1106.sh — 在 RV1106 上启动 gateway
#
# 此脚本运行在 RV1106 设备上 (非交叉编译主机)
# 前置: gateway 二进制已通过 build_rv1106.sh deploy 拷到 RV1106
#
# 用法:
#   ./run_gateway_rv1106.sh <relay_addr> [video_file]
#
# 示例:
#   ./run_gateway_rv1106.sh /ip4/192.168.1.100/tcp/4001/p2p/12D3KooW...
#   ./run_gateway_rv1106.sh /ip4/192.168.1.100/tcp/4001/p2p/12D3KooW... /tmp/test.h265

set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <relay_addr> [video_file]"
    echo "Example: $0 /ip4/192.168.1.100/tcp/4001/p2p/12D3KooW... /tmp/test.h265"
    exit 1
fi

RELAY_ADDR="$1"
VIDEO_FILE="${2:-/tmp/test.h265}"

# gateway 二进制位置 (按常见路径查找)
GATEWAY_BIN=""
for p in /usr/bin/gateway /usr/local/bin/gateway ./gateway; do
    if [ -x "$p" ]; then
        GATEWAY_BIN="$p"
        break
    fi
done

if [ -z "$GATEWAY_BIN" ]; then
    echo "[ERROR] gateway binary not found"
    echo "  Searched: /usr/bin/gateway /usr/local/bin/gateway ./gateway"
    echo "  Deploy with: ./build_rv1106.sh deploy (from cross-compile host)"
    exit 1
fi

if [ ! -f "$VIDEO_FILE" ]; then
    echo "[ERROR] Video file not found: $VIDEO_FILE"
    exit 1
fi

VSIZE=$(stat -c%s "$VIDEO_FILE" 2>/dev/null || stat -f%z "$VIDEO_FILE" 2>/dev/null)
echo "[INFO] Gateway: $GATEWAY_BIN"
echo "[INFO] Relay:   $RELAY_ADDR"
echo "[INFO] Video:   $VIDEO_FILE ($VSIZE bytes)"
echo ""

export RUST_LOG="${RUST_LOG:-info}"

# 前台运行, Ctrl+C 退出
exec "$GATEWAY_BIN" \
    --relay "$RELAY_ADDR" \
    --video-file "$VIDEO_FILE"
