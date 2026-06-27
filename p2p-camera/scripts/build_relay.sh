#!/bin/bash
# build_relay.sh — 编译 Relay Server
set -e

cd "$(dirname "$0")/../.."
MODE="${1:-debug}"
LOG_FILE="log/relay_build.log"

echo "=========================================="
echo "  Building Relay Server ($MODE)"
echo "  Start Time: $(date '+%Y-%m-%d %H:%M:%S')"
echo "=========================================="

# 确保target目录存在
mkdir -p log

# 清空旧的构建日志
> "$LOG_FILE"

if [ "$MODE" = "release" ]; then
    echo "[INFO] Compiling release version... (Detailed log: $LOG_FILE)"
    if cargo build -p relay-server --release > "$LOG_FILE" 2>&1; then
        echo "✅ [$(date '+%H:%M:%S')] Build complete. Binary: target/release/relay-server"
    else
        echo "❌ [$(date '+%H:%M:%S')] Build failed! See log for details:"
        tail -n 20 "$LOG_FILE"
        exit 1
    fi
else
    echo "[INFO] Compiling debug version... (Detailed log: $LOG_FILE)"
    if cargo build -p relay-server > "$LOG_FILE" 2>&1; then
        echo "✅ [$(date '+%H:%M:%S')] Build complete. Binary: target/debug/relay-server"
    else
        echo "❌ [$(date '+%H:%M:%S')] Build failed! See log for details:"
        tail -n 20 "$LOG_FILE"
        exit 1
    fi
fi
