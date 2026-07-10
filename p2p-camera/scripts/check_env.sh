#!/bin/bash
# Android 开发环境逐项核验脚本
# 用法: bash check_env.sh
# 在华为（或任何目标机器）上运行，逐项检查指南要求是否满足

echo "=============================================="
echo " Android 开发环境核验 (对照 android-setup-guide.md)"
echo "=============================================="
echo

PASS=0
FAIL=0

check() {
  local desc="$1"; shift
  if eval "$@" >/dev/null 2>&1; then
    echo "[OK]   $desc"
    PASS=$((PASS+1))
  else
    echo "[FAIL] $desc"
    FAIL=$((FAIL+1))
  fi
}

info() {
  local desc="$1"; shift
  local val
  val=$(eval "$@" 2>/dev/null)
  printf "      -> %s: %s\n" "$desc" "${val:-<未设置/未找到>}"
}

echo "---------- 场景 A 要求 ----------"
echo

echo "## 1. Rust 交叉编译 targets"
for t in aarch64-linux-android armv7-linux-androideabi x86_64-linux-android; do
  if rustup target list --installed 2>/dev/null | grep -q "$t"; then
    echo "[OK]   target $t 已安装"
    PASS=$((PASS+1))
  else
    echo "[FAIL] target $t 未安装  (修复: RUSTUP_DIST_SERVER=https://static.rust-lang.org rustup target add $t)"
    FAIL=$((FAIL+1))
  fi
done
echo

echo "## 2. cargo-ndk"
check "cargo-ndk 已安装" "command -v cargo-ndk"
info "cargo-ndk 版本" "cargo ndk --version 2>/dev/null || echo none"
echo

echo "## 3. Android SDK 命令行工具"
info "ANDROID_HOME" "echo \$ANDROID_HOME"
check "sdkmanager 可用" "command -v sdkmanager"
check "adb (platform-tools) 可用" "command -v adb"
echo

echo "## 4. SDK 组件"
check "NDK 27 已安装" "ls -d \$ANDROID_HOME/ndk/27*/ >/dev/null 2>&1"
check "platforms;android-34 已安装" "ls -d \$ANDROID_HOME/platforms/android-34 >/dev/null 2>&1"
check "build-tools;34.0.0 已安装" "ls -d \$ANDROID_HOME/build-tools/34.0.0 >/dev/null 2>&1"
echo

echo "## 5. Java (Android 构建需要)"
check "java 可用" "command -v java"
info "java 版本" "java -version 2>&1 | head -1"
echo

echo "## 6. mobile-core 编译产物 (.so)"
SO_DIR="$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)/target"
for abi in aarch64-linux-android armv7-linux-androideabi x86_64-linux-android; do
  if [ -f "$SO_DIR/$abi/release/libmobile_core.so" ]; then
    echo "[OK]   $SO_DIR/$abi/release/libmobile_core.so"
    PASS=$((PASS+1))
  else
    echo "[FAIL] 缺少 $SO_DIR/$abi/release/libmobile_core.so"
    FAIL=$((FAIL+1))
  fi
done
echo

echo "## 7. workspace 结构检查"
WS_ROOT="$(cd "$(dirname "$0")/.." 2>/dev/null && pwd)"
check "p2p-camera/Cargo.toml 存在 (workspace root)" "test -f \$WS_ROOT/Cargo.toml"
check "mobile-core 在 workspace 中" "grep -q 'mobile-core' \$WS_ROOT/Cargo.toml 2>/dev/null"
echo

echo "=============================================="
echo " 结果: PASS=$PASS  FAIL=$FAIL"
echo "=============================================="

if [ "$FAIL" -gt 0 ]; then
  echo "存在缺口，按上面 FAIL 项的修复提示逐项处理。"
  exit 1
else
  echo "全部通过，可以编译 Android .so 并接入 Android 工程。"
fi
