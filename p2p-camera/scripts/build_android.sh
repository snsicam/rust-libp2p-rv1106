#!/bin/bash
set -e

# ============================================================
# P2P Camera - Android .so 编译脚本
# 编译 mobile-core Rust 库为 Android 原生库
# ============================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
API_LEVEL=26

# --- Android SDK/NDK 路径 ---
# 如果在当前 shell 没设，尝试默认路径
if [ -z "$ANDROID_HOME" ]; then
    ANDROID_HOME="$HOME/android-sdk"
fi
if [ -z "$ANDROID_NDK_HOME" ]; then
    # 自动查找最新 NDK 版本
    NDK_VER=$(ls "$ANDROID_HOME/ndk/" 2>/dev/null | sort -V | tail -1)
    if [ -n "$NDK_VER" ]; then
        ANDROID_NDK_HOME="$ANDROID_HOME/ndk/$NDK_VER"
    fi
fi

if [ ! -d "$ANDROID_NDK_HOME" ]; then
    echo "✗ ANDROID_NDK_HOME 未找到: $ANDROID_NDK_HOME"
    echo "  请先安装 NDK: sdkmanager 'ndk;27.0.12077973'"
    exit 1
fi

export ANDROID_HOME
export ANDROID_NDK_HOME

echo "ANDROID_HOME     = $ANDROID_HOME"
echo "ANDROID_NDK_HOME = $ANDROID_NDK_HOME"
echo "API Level        = $API_LEVEL"
echo ""

# --- 确保 Rust targets 已安装 ---
MISSING_TARGETS=()
for t in aarch64-linux-android armv7-linux-androideabi x86_64-linux-android; do
    if ! rustup target list --installed | grep -q "$t"; then
        MISSING_TARGETS+=("$t")
    fi
done
if [ ${#MISSING_TARGETS[@]} -gt 0 ]; then
    echo "=== 安装缺失的 Rust targets: ${MISSING_TARGETS[*]} ==="
    rustup target add "${MISSING_TARGETS[@]}"
fi

# --- 编译 ---
cd "$WORKSPACE_DIR"
TARGETS=(
    "aarch64-linux-android"
    "armv7-linux-androideabi"
    "x86_64-linux-android"
)
# ABI 目录名映射
declare -A ABI_MAP=(
    ["aarch64-linux-android"]="arm64-v8a"
    ["armv7-linux-androideabi"]="armeabi-v7a"
    ["x86_64-linux-android"]="x86_64"
)

echo "=== Building mobile-core for Android ==="
for target in "${TARGETS[@]}"; do
    echo ""
    echo "--- $target ---"
    cargo ndk --target "$target" --platform "$API_LEVEL" build --release -p mobile-core
    echo "     -> target/$target/release/libmobile_core.so"
done

# --- 复制 .so 到所有 Android 项目 ---
echo ""
echo "=== 复制 .so 到 Android 项目 ==="

copy_to_project() {
    local jni_dir="$1"
    local label="$2"
    local parent_dir
    parent_dir="$(dirname "$jni_dir")"
    if [ ! -d "$parent_dir" ]; then
        echo "  (跳过 $label: $parent_dir 不存在)"
        return
    fi
    for target in "${TARGETS[@]}"; do
        abi="${ABI_MAP[$target]}"
        SRC="$WORKSPACE_DIR/target/$target/release/libmobile_core.so"
        DST_DIR="$jni_dir/$abi"
        mkdir -p "$DST_DIR"
        cp "$SRC" "$DST_DIR/"
        echo "  $SRC -> $DST_DIR/"
    done
    echo "  ✓ $label"
}

# 1) 旧 android/ 项目 (如果存在)
if [ -d "$WORKSPACE_DIR/android/app" ]; then
    copy_to_project "$WORKSPACE_DIR/android/app/src/main/jniLibs" "android/app"
fi

# 2) media_player demo (../android-media/demos/media_player)
copy_to_project "$WORKSPACE_DIR/../android-media/demos/media_player/src/main/jniLibs" "media_player demo"

echo ""
echo "=== Build Complete ==="
