#!/bin/bash
# start_relay.sh — 在 PC/云服务器上启动 relay-server
#
# relay-server 跑在公网或局域网 PC 上, 为 RV1106 gateway 和 viewer 提供中继
#
# 用法:
#   ./start_relay.sh build              # 编译 relay-server
#   ./start_relay.sh run                # 运行 relay-server (默认 0.0.0.0:4001)
#   ./start_relay.sh run --port 5001     # 指定端口运行
#   ./start_relay.sh run --public-ip 1.2.3.4  # 指定外网 IP (云服务器场景)
#   ./start_relay.sh                    # 编译 + 运行 (等价于 build + run)
#
# 连接信息会打印出来, 供 RV1106 gateway 和 PC viewer 使用

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"
LOG_DIR="$SCRIPT_DIR/logs"
mkdir -p "$LOG_DIR"

PORT=4001
KEY_FILE="$SCRIPT_DIR/relay-server.key"
RELAY_BIN="$PROJECT_ROOT/target/debug/relay-server"
PUBLIC_IP=""

# 解析子命令
DO_BUILD=false
DO_RUN=false

# 无参数 = build + run
if [ $# -eq 0 ]; then
    DO_BUILD=true
    DO_RUN=true
fi

while [[ $# -gt 0 ]]; do
    case "$1" in
        build) DO_BUILD=true; shift ;;
        run)   DO_RUN=true; shift ;;
        --port) PORT="$2"; shift 2 ;;
        --key-file) KEY_FILE="$2"; shift 2 ;;
        --public-ip) PUBLIC_IP="$2"; shift 2 ;;
        *) echo "[ERROR] Unknown arg: $1"; exit 1 ;;
    esac
done

# ---- 编译 ----
if [ "$DO_BUILD" = true ]; then
    echo "[build] Building relay-server (debug)..."
    cd "$PROJECT_ROOT"
    cargo build -p relay-server
    echo "[build] Done: $RELAY_BIN"
fi

# ---- 运行 ----
if [ "$DO_RUN" = true ]; then
    if [ ! -x "$RELAY_BIN" ]; then
        echo "[ERROR] Binary not found: $RELAY_BIN"
        echo "  Run: $0 build"
        exit 1
    fi

    # 进程管理
    RELAY_PID=""
    cleanup() {
        echo ""
        echo "[INFO] Stopping relay..."
        [ -n "$RELAY_PID" ] && kill "$RELAY_PID" 2>/dev/null
        wait 2>/dev/null
        echo "[INFO] Stopped."
    }
    trap cleanup EXIT INT TERM

    echo "[run] Starting relay-server on port $PORT (key: $KEY_FILE)..."
    "$RELAY_BIN" --port "$PORT" --key-file "$KEY_FILE" > "$LOG_DIR/relay.log" 2>&1 &
    RELAY_PID=$!

    # 等待 relay 就绪, 提取 PeerId
    RELAY_PEER=""
    for i in $(seq 1 10); do
        sleep 0.5
        RELAY_PEER=$(grep -oP 'PeerId: \K[^\s]+' "$LOG_DIR/relay.log" 2>/dev/null | head -1)
        [ -n "$RELAY_PEER" ] && break
    done

    if [ -z "$RELAY_PEER" ]; then
        echo "[ERROR] Failed to get relay PeerId"
        cat "$LOG_DIR/relay.log"
        exit 1
    fi

    # 获取本机 IP (用于 RV1106 连接)
    # 优先用 --public-ip (云服务器场景), 否则取本机 IP
    if [ -n "$PUBLIC_IP" ]; then
        LOCAL_IP="$PUBLIC_IP"
    else
        LOCAL_IP=$(hostname -I 2>/dev/null | awk '{print $1}' || echo "127.0.0.1")
    fi
    RELAY_ADDR="/ip4/$LOCAL_IP/tcp/$PORT/p2p/$RELAY_PEER"

    # ---- 打印连接信息 ----
    echo ""
    echo "============================================"
    echo "  P2P Camera Relay Server Running"
    echo "============================================"
    echo "  Relay PeerId: $RELAY_PEER"
    echo "  Relay Addr:   $RELAY_ADDR"
    echo ""
    echo "  ---- RV1106 上运行 (gateway) ----"
    echo "  gateway --relay $RELAY_ADDR --video-file /tmp/test.h265"
    echo ""
    echo "  ---- PC 上运行 (viewer, SDL 播放) ----"
    echo "  $SCRIPT_DIR/play_viewer.sh run $RELAY_ADDR <GATEWAY_PEER>"
    echo "  (GATEWAY_PEER 从 RV1106 gateway 启动日志获取)"
    echo ""
    echo "  Log: $LOG_DIR/relay.log"
    echo "  Ctrl+C to stop"
    echo "============================================"
    echo ""

    # 保持运行
    wait
fi
