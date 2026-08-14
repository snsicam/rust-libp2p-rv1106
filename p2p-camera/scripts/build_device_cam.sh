#!/bin/bash
# build_rv1106.sh — 交叉编译 device-cam 到 RV1106
#
# 两种模式:
#   ./build_rv1106.sh              # 文件模式 (musl 静态链接, 从文件读视频)
#   ./build_rv1106.sh rv1106       # 摄像头模式 (glibc + uclibc 工具链, 链接 SDK)
#   ./build_rv1106.sh deploy       # 文件模式 + scp (device-cam)
#   ./build_rv1106.sh rv1106 deploy  # 摄像头模式 + scp (device-cam + cam-test)
#
# 两种模式都会同时编译 cam-test (RV1106 本地抓拍/合成测试程序, 经 UDS 控 device-cam)
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
    TOOLCHAIN_DIR="${RV1106_TOOLCHAIN:-$PROJECT_ROOT/toolchain/arm-rockchip830-linux-uclibcgnueabihf}"
else
    # 文件模式: musl target (静态链接, 同 ping)
    TARGET="armv7-unknown-linux-gnueabihf"
    GCC_NAME="armv7l-linux-gnueabihf-gcc"
fi

DEVICE_CAM_BIN="$PROJECT_ROOT/target/$TARGET/debug/device-cam"

cd "$PROJECT_ROOT"

# ---- 检查 toolchain ----
echo "[1/3] Checking toolchain..."

if ! rustup target list --installed 2>/dev/null | grep -q "$TARGET"; then
    echo "[ERROR] Rust target '$TARGET' not installed."
    echo "  Run: rustup target add $TARGET"
    exit 1
fi

# 优先将 toolchain bin 加入 PATH (cargo build 需要)
if [ -n "${TOOLCHAIN_DIR:-}" ] && [ -d "$TOOLCHAIN_DIR/bin" ]; then
    export PATH="$TOOLCHAIN_DIR/bin:$PATH"
fi

GCC_PATH=$(which "$GCC_NAME" 2>/dev/null || echo "")
if [ -z "$GCC_PATH" ]; then
    echo "[ERROR] Cross compiler '$GCC_NAME' not found."
    if [ "$RV1106_MODE" = true ]; then
        echo "  Tried: $TOOLCHAIN_DIR/bin/$GCC_NAME"
        echo "  Set RV1106_TOOLCHAIN to Rockchip toolchain dir"
    else
        echo "  Install: apt-get install gcc-arm-linux-gnueabihf"
        echo "  Then:    ln -sf /usr/bin/arm-linux-gnueabihf-gcc /usr/local/bin/armv7l-linux-gnueabihf-gcc"
    fi
    exit 1
fi

echo "      target:  $TARGET"
echo "      linker:  $GCC_PATH"
echo "      rv1106:  $RV1106_MODE"

# ---- 设置 CC 环境变量 (ring/cc-rs 等 build script 需要) ----
# CC_<target> 用下划线替换连字符
TARGET_UNDERSCORE=$(echo "$TARGET" | tr '-' '_')
export CC_${TARGET_UNDERSCORE}="$GCC_NAME"
export CFLAGS_${TARGET_UNDERSCORE}="-fPIC"

# rv1106 模式: 覆盖 cargo linker 为 Rockchip 工具链
if [ "$RV1106_MODE" = true ]; then
    export CARGO_TARGET_ARMV7_UNKNOWN_LINUX_GNUEABIHF_LINKER="$GCC_NAME"
fi

# ---- rv1106 模式: 设置 SDK 路径 ----
if [ "$RV1106_MODE" = true ]; then
    SDK_ROOT="${RV1106_SDK_ROOT:-$PROJECT_ROOT/RV1106_Linux_SDK}"
    # 必须 export 给 build.rs, 否则 RGA 自动检测会回退到硬编码的
    # /workspace/rv1106/RV1106_Linux_SDK (本机不存在) 而找不到 RgaApi.h
    export RV1106_SDK_ROOT="$SDK_ROOT"

    # SDK 头文件路径 — rockit MPI 头文件 (rk_mpi_sys.h 等)
    # 如果未设置, 自动检测 SDK 源码树中的 include 路径
    if [ -z "${RV1106_SDK_INCLUDE:-}" ]; then
        ROCKIT_INC="$SDK_ROOT/media/rockit/rockit/mpi/sdk/include"
        if [ -f "$ROCKIT_INC/rk_mpi_sys.h" ]; then
            export RV1106_SDK_INCLUDE="$ROCKIT_INC"
        else
            echo "[ERROR] rockit include dir not found. Checked:"
            echo "  $ROCKIT_INC/rk_mpi_sys.h"
            echo "Set RV1106_SDK_INCLUDE manually"
            exit 1
        fi
    fi

    # SDK 库路径 — librockit_full.so / librkaiq.so / librockchip_mpp.so 在不同目录, 用冒号分隔
    # 如果未设置, 自动检测 SDK 源码树中的 .so 路径
    if [ -z "${RV1106_SDK_LIB:-}" ]; then
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
echo "[2/4] Building device-cam for RV1106..."

if [ "$RV1106_MODE" = true ]; then
    cargo build -p device-cam --target "$TARGET" --features rv1106
else
    cargo build -p device-cam --target "$TARGET"
fi

# 同时编译 cam-test (RV1106 本地抓拍/合成测试程序, 经 UDS 控 device-cam)
# 注意: cam-test 是 p2p-camera workspace 成员, 需指定 --manifest-path (父 rust-libp2p workspace 不含它)
# 该 Rockchip uclibc 工具链的 libc 未导出 getauxval (Rust std stack_overflow 检测需要), 用 gnueabihf target 但
# 让链接器忽略该未定义符号 (cam-test 为轻量工具, 不会触发栈溢出检测崩溃)。
echo ""
echo "[3/4] Building cam-test for RV1106..."
P2P_CAMERA_DIR="$SCRIPT_DIR/.."
export RUSTFLAGS="-C link-arg=-Wl,--unresolved-symbols=ignore-all"
cargo build --manifest-path "$P2P_CAMERA_DIR/Cargo.toml" -p cam-test --target "$TARGET"
CAM_TEST_BIN="$P2P_CAMERA_DIR/target/$TARGET/debug/cam-test"
unset RUSTFLAGS

# ---- 验证产物 ----
echo ""
echo "[4/4] Build complete!"
echo ""
echo "============================================"
echo "  device-cam: $DEVICE_CAM_BIN"
echo "  cam-test:   $CAM_TEST_BIN"
echo "  Target:     $TARGET"
echo "  Mode:       $( [ "$RV1106_MODE" = true ] && echo "rv1106 (camera SDK)" || echo "file" )"
echo "============================================"
file "$DEVICE_CAM_BIN" 2>/dev/null || true
echo ""
ls -lh "$DEVICE_CAM_BIN" "$CAM_TEST_BIN"

# ---- deploy 模式 ----
if [ "$DEPLOY" = true ]; then
    RV1106_HOST="${RV1106_HOST:-192.168.1.100}"
    echo ""
    echo "[Deploy] Copying to RV1106 ($RV1106_HOST)..."
    scp "$DEVICE_CAM_BIN" "root@$RV1106_HOST:/usr/bin/device-cam"
    scp "$CAM_TEST_BIN" "root@$RV1106_HOST:/usr/bin/cam-test"
    echo "[Deploy] Done. Run on RV1106:"
    if [ "$RV1106_MODE" = true ]; then
        echo "  device-cam --relay <relay_addr> --width 1920 --height 1080 --fps 25 --bitrate 4096"
        echo "  cam-test snapshot            # 抓拍一张 JPG"
        echo "  cam-test compose [fps]       # 合成 AVI (默认 fps=5)"
        echo "  cam-test loop [n] [fps]      # 循环抓拍 n 次后合成"
    else
        echo "  device-cam --relay <relay_addr> --video-file /tmp/test.h265"
    fi
fi
