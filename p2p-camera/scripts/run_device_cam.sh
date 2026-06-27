#!/bin/bash
# run_device_cam_rv1106.sh — 在 RV1106 上启动 device-cam
#
# 此脚本运行在 RV1106 设备上 (非交叉编译主机)
# 前置: device-cam 二进制已通过 build_rv1106.sh deploy 拷到 RV1106
#
# 用法:
#   ./run_device_cam_rv1106.sh <relay_addr> [video_file]
#
# 示例:
#   ./run_device_cam_rv1106.sh /ip4/192.168.1.100/tcp/4001/p2p/12D3KooW...
#   ./run_device_cam_rv1106.sh /ip4/192.168.1.100/tcp/4001/p2p/12D3KooW... /tmp/test.h265

set -euo pipefail

if [ $# -lt 1 ]; then
    echo "Usage: $0 <relay_addr> [video_file]"
    echo "Example: $0 /ip4/192.168.1.100/tcp/4001/p2p/12D3KooW... /tmp/test.h265"
    exit 1
fi

RELAY_ADDR="$1"
VIDEO_FILE="${2:-/tmp/test.h265}"

# device-cam 二进制位置 (按常见路径查找)
DEVICE_CAM_BIN=""
for p in /usr/bin/device-cam /usr/local/bin/device-cam ./device-cam; do
    if [ -x "$p" ]; then
        DEVICE_CAM_BIN="$p"
        break
    fi
done

if [ -z "$DEVICE_CAM_BIN" ]; then
    echo "[ERROR] device-cam binary not found"
    echo "  Searched: /usr/bin/device-cam /usr/local/bin/device-cam ./device-cam"
    echo "  Deploy with: ./build_rv1106.sh deploy (from cross-compile host)"
    exit 1
fi

if [ ! -f "$VIDEO_FILE" ]; then
    echo "[ERROR] Video file not found: $VIDEO_FILE"
    exit 1
fi

VSIZE=$(stat -c%s "$VIDEO_FILE" 2>/dev/null || stat -f%z "$VIDEO_FILE" 2>/dev/null)
echo "[INFO] DeviceCam: $DEVICE_CAM_BIN"
echo "[INFO] Relay:   $RELAY_ADDR"
echo "[INFO] Video:   $VIDEO_FILE ($VSIZE bytes)"
echo ""

export RUST_LOG="${RUST_LOG:-info}"

# 前台运行, Ctrl+C 退出
exec "$DEVICE_CAM_BIN" \
    --relay "$RELAY_ADDR" \
    --video-file "$VIDEO_FILE"
