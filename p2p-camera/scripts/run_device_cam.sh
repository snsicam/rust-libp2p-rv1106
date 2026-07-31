#!/bin/bash
# run_device_cam.sh — 启动 device-cam
#
# 用法:
#   方式1 (配置文件): ./run_device_cam.sh
#     → 首次运行自动生成 device-cam.toml，编辑后重启
#
#   方式2 (命令行覆盖): ./run_device_cam.sh <relay_addr> [video_file]
#     → 命令行参数覆盖配置文件中的值
#
# 示例:
#   ./run_device_cam.sh
#   ./run_device_cam.sh /ip4/192.168.1.100/udp/4001/quic-v1/p2p/12D3KooW...
#   ./run_device_cam.sh /ip4/192.168.1.100/udp/4001/quic-v1/p2p/12D3KooW... /tmp/test.h265
#
# 日志: 输出到终端同时写入 scripts/logs/device_cam.log

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR" && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"

# device-cam 二进制位置 (按常见路径查找)
DEVICE_CAM_BIN=""
for p in "$PROJECT_ROOT/target/debug/device-cam" ./device-cam; do
    if [ -x "$p" ]; then
        DEVICE_CAM_BIN="$p"
        break
    fi
done

if [ -z "$DEVICE_CAM_BIN" ]; then
    echo "[ERROR] device-cam binary not found"
    echo "  Searched: $PROJECT_ROOT/target/debug/device-cam ./device-cam"
    exit 1
fi

mkdir -p "$LOG_DIR"

# 构建命令参数
ARGS=()

# 固定 UDP 端口: 稳定 NAT 映射, 避免出口端口随机漂移导致 NAT 类型在
# Full Cone / Symmetric 间跳变 (多网卡/4G 环境下尤为明显, 会让 DCUtR 与直连反复失败)。
# 设备本机已在 48781 上监听 QUIC, 这里显式指定保持一致。
FIXED_UDP_PORT="${FIXED_UDP_PORT:-48781}"

if [ $# -ge 1 ]; then
    # 命令行模式: 传入 relay 地址和可选视频文件
    RELAY_ADDR="$1"
    ARGS+=(--relay "$RELAY_ADDR")

    if [ $# -ge 2 ]; then
        VIDEO_FILE="$2"
        if [ ! -f "$VIDEO_FILE" ]; then
            echo "[ERROR] Video file not found: $VIDEO_FILE"
            exit 1
        fi
        ARGS+=(--video-file "$VIDEO_FILE")
    fi

    ARGS+=(--udp-port "$FIXED_UDP_PORT")

    echo ""
    echo "============================================"
    echo "  P2P Camera DeviceCam"
    echo "============================================"
    echo "  Binary:  $DEVICE_CAM_BIN"
    echo "  Relay:   $RELAY_ADDR"
    echo "  UDP:     $FIXED_UDP_PORT (fixed)"
    if [ $# -ge 2 ]; then
    echo "  Video:   $VIDEO_FILE"
    fi
else
    # 配置文件模式: 直接运行，读取 device-cam.toml
    # 注意: 若 device-cam.toml 未设置 udp_port, 设备将使用随机端口。
    # 如需固定端口, 在 device-cam.toml 中加入 `udp_port = 48781` 或改用命令行模式。
    echo ""
    echo "============================================"
    echo "  P2P Camera DeviceCam"
    echo "============================================"
    echo "  Binary:  $DEVICE_CAM_BIN"
    echo "  Config:  device-cam.toml"
fi

echo ""
echo "  Ctrl+C to stop"
echo "============================================"
echo ""

export RUST_LOG=info

# 前台运行, 输出到终端同时写日志
"$DEVICE_CAM_BIN" "${ARGS[@]}" \
    2>&1 | tee "$LOG_DIR/device_cam.log"
