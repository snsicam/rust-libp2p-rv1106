#!/bin/bash
set -e

# ============================================================
# P2P Camera — 端到端 APK 构建脚本
#
# 流程:
#   1. 编译 mobile-core Rust 库 → .so (build_android.sh)
#   2. 复制 .so 到 media_player demo
#   3. Gradle 构建 APK
#
# 输出: android-media/demos/media_player/buildout/outputs/apk/release/*.apk
# ============================================================

SCRIPT_DIR="$(cd "$(dirname "$0")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
ANDROID_DIR="$WORKSPACE_DIR/../android-media"
DEMO_DIR="$ANDROID_DIR/demos/media_player"
# 注意: 此 android-media 项目 buildDir 在 gradle.properties 中设为 buildout
BUILD_DIR_NAME="${BUILD_DIR_NAME:-buildout}"

echo "========================================="
echo "  P2P Camera — APK Build"
echo "========================================="
echo ""

# ── Step 1: 编译 Rust .so ──
echo ">>> Step 1/2: 编译 Rust .so"
bash "$SCRIPT_DIR/build_android.sh"
echo ""

# ── Step 2: Gradle 构建 APK ──
echo ">>> Step 2/2: Gradle 构建 APK"

if [ ! -f "$ANDROID_DIR/gradlew" ]; then
    echo "✗ gradlew 未找到: $ANDROID_DIR/gradlew"
    echo "  请先确认 android-media 项目完整"
    exit 1
fi

cd "$ANDROID_DIR"

# 构建模式
BUILD_TYPE="${1:-release}"

echo "  构建类型: $BUILD_TYPE"
./gradlew ":demo-media-player:assemble${BUILD_TYPE^}" --no-daemon 2>&1 | \
    grep -E "BUILD|FAILED|deprecated|Error:|error:" | head -20
echo ""

APK_DIR="$DEMO_DIR/$BUILD_DIR_NAME/outputs/apk/$BUILD_TYPE"
APK_FILE=$(ls "$APK_DIR"/*.apk 2>/dev/null | head -1)

if [ -n "$APK_FILE" ] && [ -f "$APK_FILE" ]; then
    APK_SIZE=$(du -h "$APK_FILE" | cut -f1)
    echo "========================================="
    echo "  APK 构建成功"
    echo "  文件: $APK_FILE"
    echo "  体积: $APK_SIZE"
    echo "========================================="
else
    echo "✗ APK 构建失败，详见上方日志"
    echo "  预期路径: $APK_DIR/*.apk"
    exit 1
fi
