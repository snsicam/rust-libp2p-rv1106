#!/bin/bash
# build_rv1106.sh — 交叉编译 gateway 到 RV1106
#
# 两种模式:
#   ./build_rv1106.sh              # 文件模式 (musl 静态链接, 从文件读视频)
#   ./build_rv1106.sh rv1106       # 摄像头模式 (glibc + uclibc 工具链, 链接 SDK)
#   ./build_rv1106.sh deploy       # 文件模式 + scp
#   ./build_rv1106.sh rv1106 deploy  # 摄像头模式 + scp
#
# 环境变量:
#   RV1106_HOST         — RV1106 的 IP (deploy 用, 默认 192.168.1.100)
#   RV1106_SDK_INCLUDE  — SDK 头文件目录 (rv1106 模式)
#   RV1106_SDK_LIB      — SDK 库目录 (rv1106 模式, 包含 librockit_full.so)
#   RV1106_TOOLCHAIN    — Rockchip uclibc 工具链 bin 目录 (rv1106 模式)

set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

# 解析参数
RV1106_MODE=false
DEPLOY=false
for arg in "$@"; do
    case "$arg" in
        rv1106)   RV1106_MODE=true ;;
        deploy)   DEPLOY=true ;;
        *) echo "[ERROR] Unknown arg: $arg"; exit 1 ;;
    esac
done

# 根据 mode 选择 target 和工具链
if [ "$RV1106_MODE" = true ]; then
    # 摄像头模式: glibc target + Rockchip uclibc 工具链 (动态链接 SDK)
    TARGET="armv7-unknown-linux-gnueabihf"
    GCC_NAME="arm-rockchip830-linux-uclibcgnueabihf-gcc"
    TOOLCHAIN_DIR="${RV1106_TOOLCHAIN:-/home/song/samba/work/rv1106/luckfox-pico/tools/linux/toolchain/arm-rockchip830-linux-uclibcgnueabihf}"
else
    # 文件模式: musl target (静态链接, 同 ping)
    TARGET="armv7-unknown-linux-musleabihf"
    GCC_NAME="armv7l-linux-musleabihf-gcc"
fi

GATEWAY_BIN="$PROJECT_ROOT/target/$TARGET/release/gateway"

cd "$PROJECT_ROOT"

# ---- 检查 toolchain ----
echo "[1/3] Checking toolchain..."

if ! rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
    echo "[ERROR] Rust target '$TARGET' not installed."
    echo "  Run: rustup target add $TARGET"
    exit 1
fi

GCC_PATH=$(which "$GCC_NAME" 2>/dev/null || echo "")
if [ -z "$GCC_PATH" ]; then
    # 尝试在 TOOLCHAIN_DIR 中查找
    if [ -x "$TOOLCHAIN_DIR/bin/$GCC_NAME" ]; then
        export PATH="$TOOLCHAIN_DIR/bin:$PATH"
        GCC_PATH="$TOOLCHAIN_DIR/bin/$GCC_NAME"
    else
        echo "[ERROR] Cross compiler '$GCC_NAME' not found."
        echo "  For file mode: install musl toolchain from https://musl.cc/"
        echo "  For rv1106 mode: set RV1106_TOOLCHAIN to Rockchip toolchain dir"
        exit 1
    fi
fi

echo "      target:  $TARGET"
echo "      linker:  $GCC_PATH"
echo "      rv1106:  $RV1106_MODE"

# ---- 设置 CC 环境变量 (ring/cc-rs 等 build script 需要) ----
# CC_<target> 用下划线替换连字符
TARGET_UNDERSCORE=$(echo "$TARGET" | tr '-' '_')
export CC_${TARGET_UNDERSCORE}="$GCC_NAME"
export CFLAGS_${TARGET_UNDERSCORE}="-fPIC"

# ---- rv1106 模式: 设置 SDK 路径 ----
if [ "$RV1106_MODE" = true ]; then
    export RV1106_SDK_INCLUDE="${RV1106_SDK_INCLUDE:-/usr/include}"

    # SDK 库路径 — librockit_full.so / librkaiq.so / librockchip_mpp.so 在不同目录, 用冒号分隔
    # 如果未设置, 自动检测 SDK 源码树中的 .so 路径
    if [ -z "${RV1106_SDK_LIB:-}" ]; then
        SDK_ROOT="${RV1106_SDK_ROOT:-/home/song/samba/work/rv1106/lubancat}"
        ROCKIT_LIB="$SDK_ROOT/media/rockit/rockit/lib/lib32"
        RKAIQ_LIB="$SDK_ROOT/media/isp/release_camera_engine_rkaiq_rv1106_arm-rockchip830-linux-uclibcgnueabihf/lib"
        MPP_LIB="$SDK_ROOT/media/mpp/release_mpp_rv1106_arm-rockchip830-linux-uclibcgnueabihf/lib"

        if [ -f "$ROCKIT_LIB/librockit_full.so" ] && [ -f "$RKAIQ_LIB/librkaiq.so" ] && [ -f "$MPP_LIB/librockchip_mpp.so" ]; then
            export RV1106_SDK_LIB="$ROCKIT_LIB:$RKAIQ_LIB:$MPP_LIB"
        else
            echo "[ERROR] SDK .so files not found. Checked:"
            echo "  $ROCKIT_LIB/librockit_full.so"
            echo "  $RKAIQ_LIB/librkaiq.so"
            echo "  $MPP_LIB/librockchip_mpp.so"
            echo "Set RV1106_SDK_LIB manually (colon-separated paths)"
            exit 1
        fi
    fi

    echo "      sdk inc: $RV1106_SDK_INCLUDE"
    echo "      sdk lib: $RV1106_SDK_LIB"
fi

# ---- 编译 ----
echo ""
echo "[2/3] Building gateway for RV1106..."

if [ "$RV1106_MODE" = true ]; then
    cargo build --release -p gateway --target "$TARGET" --features rv1106
else
    cargo build --release -p gateway --target "$TARGET"
fi

# ---- 验证产物 ----
echo ""
echo "[3/3] Build complete!"
echo ""
echo "============================================"
echo "  Binary: $GATEWAY_BIN"
echo "  Target: $TARGET"
echo "  Mode:   $( [ "$RV1106_MODE" = true ] && echo "rv1106 (camera SDK)" || echo "file" )"
echo "============================================"
file "$GATEWAY_BIN" 2>/dev/null || true
echo ""
ls -lh "$GATEWAY_BIN"

# ---- deploy 模式 ----
if [ "$DEPLOY" = true ]; then
    RV1106_HOST="${RV1106_HOST:-192.168.1.100}"
    echo ""
    echo "[Deploy] Copying to RV1106 ($RV1106_HOST)..."
    scp "$GATEWAY_BIN" "root@$RV1106_HOST:/usr/bin/gateway"
    echo "[Deploy] Done. Run on RV1106:"
    if [ "$RV1106_MODE" = true ]; then
        echo "  gateway --relay <relay_addr> --width 1920 --height 1080 --fps 25 --bitrate 4096"
    else
        echo "  gateway --relay <relay_addr> --video-file /tmp/test.h265"
    fi
fi
