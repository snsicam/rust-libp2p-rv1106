#!/bin/bash
set -uo pipefail

# ============================================================
# P2P Camera — 一键启动 Android 模拟器并安装运行 APK
#
# 用法:
#   ./run_emulator.sh              # 启动模拟器 + 安装最新 APK + 运行
#   ./run_emulator.sh --build      # 先编译 APK，再启动模拟器运行
#   ./run_emulator.sh --emulator   # 仅启动模拟器
#   ./run_emulator.sh --install    # 仅安装 APK 到已运行的模拟器
#   ./run_emulator.sh --stop       # 停止模拟器
# ============================================================

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
WORKSPACE_DIR="$(cd "$SCRIPT_DIR/.." && pwd)"
APK_DIR="$WORKSPACE_DIR/../android-media/demos/media_player/buildout/outputs/apk/release"

# --- 配置 ---
AVD_NAME="${AVD_NAME:-Medium_Phone_API_34}"
PACKAGE_NAME="com.p2pcamera.mediaplayer"
ACTIVITY_NAME="$PACKAGE_NAME/.MainActivity"

# --- SDK/NDK 路径 ---
if [ -z "${ANDROID_HOME:-}" ]; then
    ANDROID_HOME="$HOME/Android/Sdk"
fi
export ANDROID_HOME
ADB="$ANDROID_HOME/platform-tools/adb"
EMULATOR="$ANDROID_HOME/emulator/emulator"

# --- 颜色 ---
GREEN='\033[0;32m'
YELLOW='\033[1;33m'
RED='\033[0;31m'
NC='\033[0m'

info()  { echo -e "${GREEN}[INFO]${NC} $*"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $*"; }
error() { echo -e "${RED}[ERROR]${NC} $*"; }

# --- 查找 APK ---
find_apk() {
    APK_FILE=""
    if [ -d "$APK_DIR" ]; then
        for f in "$APK_DIR"/*.apk; do
            if [ -f "$f" ]; then
                APK_FILE="$f"
                break
            fi
        done
    fi
}

# --- 检查工具 ---
check_tools() {
    if [ ! -f "$ADB" ]; then
        error "adb 未找到: $ADB"
        error "请先安装 Android SDK platform-tools"
        exit 1
    fi
    if [ ! -f "$EMULATOR" ]; then
        error "emulator 未找到: $EMULATOR"
        error "请先安装 Android SDK emulator"
        exit 1
    fi
}

# --- 获取 boot_completed 属性（容错） ---
get_boot_completed() {
    local result
    result=$("$ADB" shell getprop sys.boot_completed 2>/dev/null || true)
    echo "$result" | tr -d '\r'
}

# --- 启动模拟器 ---
start_emulator() {
    # 检查是否已在运行
    local boot_completed
    boot_completed=$(get_boot_completed)
    if [ "$boot_completed" = "1" ]; then
        info "模拟器已在运行"
        return 0
    fi

    # 检查 AVD 是否存在
    if ! "$EMULATOR" -list-avds 2>/dev/null | grep -q "^${AVD_NAME}$"; then
        error "AVD '$AVD_NAME' 不存在"
        echo ""
        echo "  可用 AVD 列表:"
        "$EMULATOR" -list-avds 2>/dev/null | sed 's/^/    /'
        echo ""
        echo "  创建新 AVD:"
        echo "    sdkmanager \"system-images;android-34;default;x86_64\""
        echo "    avdmanager create avd -n $AVD_NAME -k \"system-images;android-34;default;x86_64\" -d medium_phone"
        exit 1
    fi

    info "启动模拟器: $AVD_NAME"
    # 用宿主 GPU 加速 (hw.gpu.mode=host)。在 Wayland 会话下强制 X11 后端,
    # 否则模拟器创建 EGL 上下文会失败 (与 Android Studio 相同的坑)。X11 会话下此设置无害。
    export QT_QPA_PLATFORM=xcb
    nohup "$EMULATOR" -avd "$AVD_NAME" -no-snapshot-load -gpu host > /tmp/emulator.log 2>&1 &

    info "等待模拟器启动..."
    "$ADB" wait-for-device || true
    for i in $(seq 1 60); do
        local boot
        boot=$(get_boot_completed)
        if [ "$boot" = "1" ]; then
            info "模拟器启动完成"
            return 0
        fi
        sleep 2
    done
    error "模拟器启动超时"
    exit 1
}

# --- 安装 APK ---
install_apk() {
    find_apk
    if [ -z "$APK_FILE" ] || [ ! -f "$APK_FILE" ]; then
        error "APK 未找到: $APK_DIR/*.apk"
        echo ""
        echo "  请先编译 APK:"
        echo "    ./scripts/build_apk.sh"
        echo "  或使用 --build 参数自动编译"
        exit 1
    fi

    local apk_size
    apk_size=$(du -h "$APK_FILE" | cut -f1)
    info "安装 APK: $APK_FILE ($apk_size)"

    if "$ADB" install -r "$APK_FILE" 2>&1; then
        info "APK 安装成功"
    else
        error "APK 安装失败"
        exit 1
    fi
}

# --- 启动应用 ---
launch_app() {
    info "启动应用: $PACKAGE_NAME"
    "$ADB" shell am start -n "$ACTIVITY_NAME" 2>&1 || true
    info "应用已启动"
}

# --- 停止模拟器 ---
stop_emulator() {
    info "停止模拟器..."
    "$ADB" emu kill 2>/dev/null || true
    info "模拟器已停止"
}

# --- 编译 APK ---
build_apk() {
    info "编译 APK..."
    if ! bash "$SCRIPT_DIR/build_apk.sh"; then
        error "APK 编译失败, 中止 (不安装旧包)"
        exit 1
    fi
    find_apk
}

# --- 查看日志 ---
show_log() {
    info "实时日志 (Ctrl+C 退出)..."
    "$ADB" logcat -s "$PACKAGE_NAME" 2>/dev/null || "$ADB" logcat "*:W" 2>/dev/null || true
}

# ============================================================
# 主流程
# ============================================================

check_tools

case "${1:-run}" in
    --build)
        echo "========================================="
        echo "  P2P Camera — 编译 + 模拟器运行"
        echo "========================================="
        echo ""
        build_apk
        echo ""
        start_emulator
        echo ""
        install_apk
        echo ""
        launch_app
        ;;
    --emulator)
        echo "========================================="
        echo "  P2P Camera — 启动模拟器"
        echo "========================================="
        echo ""
        start_emulator
        ;;
    --install)
        echo "========================================="
        echo "  P2P Camera — 安装 APK"
        echo "========================================="
        echo ""
        install_apk
        echo ""
        launch_app
        ;;
    --stop|stop)
        stop_emulator
        ;;
    --log|log)
        show_log
        ;;
    --help|-h)
        echo "用法: $0 [选项]"
        echo ""
        echo "  (无参数)       启动模拟器 + 安装 APK + 运行应用"
        echo "  --build        先编译 APK，再启动模拟器运行"
        echo "  --emulator     仅启动模拟器"
        echo "  --install      仅安装 APK 到已运行的模拟器"
        echo "  --stop         停止模拟器"
        echo "  --log          查看应用实时日志"
        echo "  --help         显示帮助"
        echo ""
        echo "环境变量:"
        echo "  AVD_NAME       模拟器名称 (默认: Medium_Phone_API_34)"
        ;;
    run|*)
        echo "========================================="
        echo "  P2P Camera — 模拟器运行"
        echo "========================================="
        echo ""
        start_emulator
        echo ""
        install_apk
        echo ""
        launch_app
        ;;
esac
