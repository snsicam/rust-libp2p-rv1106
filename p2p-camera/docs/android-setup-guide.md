# Android 开发环境搭建指南

## 场景 A：Linux 无头服务器（命令行）

### 1. Rust 交叉编译 targets

```bash
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android
```

### 2. cargo-ndk

```bash
cargo install cargo-ndk
```

### 3. Android SDK 命令行工具

```bash
# 目录结构要求: ~/android-sdk/cmdline-tools/latest/bin/
mkdir -p ~/android-sdk/cmdline-tools
cd ~/android-sdk/cmdline-tools

# 下载（版本号可能更新，去 https://developer.android.com/studio 查最新）
curl -L "https://dl.google.com/android/repository/commandlinetools-linux-11076708_latest.zip" \
  -o cmdline-tools.zip

unzip cmdline-tools.zip
mv cmdline-tools latest

# 设置环境变量
export ANDROID_HOME=~/android-sdk
export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"

# 写入 shell 配置（永久生效）
echo 'export ANDROID_HOME=$HOME/android-sdk' >> ~/.zshrc
echo 'export PATH="$ANDROID_HOME/cmdline-tools/latest/bin:$ANDROID_HOME/platform-tools:$PATH"' >> ~/.zshrc
```

### 4. 安装 SDK 组件

```bash
# 接受许可
yes | sdkmanager --licenses

# 安装 NDK、platform、build-tools
sdkmanager "platform-tools" \
           "platforms;android-34" \
           "ndk;27.0.12077973" \
           "build-tools;34.0.0"
```

### 5. 编译 mobile-core 为 Android .so

> **重要**：必须在 `p2p-camera/` 目录下运行（workspace root），
> 不能进 `mobile-core/` 子目录。Cargo 需要找到 `Cargo.toml` 中定义的 workspace。

```bash
cd p2p-camera

# cargo-ndk 4.x 必须用 --target / --platform 长参数名，短 -t -p 会冲突！

# 编译 64 位 ARM（主流手机）
cargo ndk --target aarch64-linux-android --platform 26 build --release -p mobile-core

# 编译 32 位 ARM（老旧设备）
cargo ndk --target armv7-linux-androideabi --platform 26 build --release -p mobile-core

# 编译 x86_64（模拟器）
cargo ndk --target x86_64-linux-android --platform 26 build --release -p mobile-core

# 产物位置:
# target/aarch64-linux-android/release/libmobile_core.so
# target/armv7-linux-androideabi/release/libmobile_core.so
# target/x86_64-linux-android/release/libmobile_core.so
```

> `--platform 26` 表示最低 API Level 26 (Android 8.0)，覆盖 95%+ 设备

---

## 场景 B：PC/笔记本（Android Studio GUI 模式）

### 1. 安装 Android Studio

- 下载: https://developer.android.com/studio
- Linux: 解压到 `~/android-studio/`，运行 `./bin/studio.sh`
- macOS: 拖入 Applications
- Windows: 运行 installer

### 2. 首次启动配置

- 选择 "Standard" 安装类型
- SDK Manager 会自动安装：
  - Android SDK Platform 34+
  - Android SDK Build-Tools 34+
  - Android NDK（如果没有勾选，去 SDK Manager → SDK Tools 手动勾选 NDK）
  - Platform-Tools (adb)

### 3. 安装 Rust 交叉编译工具

```bash
# 安装 Rust（如果还没装）
curl --proto '=https' --tlsv1.2 -sSf https://sh.rustup.rs | sh

# Android targets
rustup target add aarch64-linux-android armv7-linux-androideabi x86_64-linux-android

# cargo-ndk
cargo install cargo-ndk
```

### 4. 设置环境变量

```bash
# macOS/Linux 写入 ~/.zshrc 或 ~/.bashrc
export ANDROID_HOME=$HOME/Android/Sdk          # Linux 默认
# export ANDROID_HOME=$HOME/Library/Android/sdk  # macOS 默认
# export ANDROID_HOME=$LOCALAPPDATA/Android/Sdk  # Windows 默认

export NDK_HOME=$ANDROID_HOME/ndk/27.0.12077973
export PATH="$ANDROID_HOME/platform-tools:$PATH"
```

---

## 项目结构（编译后）

```
p2p-camera/
├── android/                       # ← 新建 Android 项目放这里
│   ├── build.gradle.kts
│   ├── app/
│   │   ├── build.gradle.kts
│   │   └── src/main/
│   │       ├── java/yj/mediaplayer/
│   │       │   └── MainActivity.kt
│   │       └── jniLibs/
│   │           ├── arm64-v8a/
│   │           │   └── libmobile_core.so    ← Rust 编译产物
│   │           ├── armeabi-v7a/
│   │           │   └── libmobile_core.so
│   │           └── x86_64/
│   │               └── libmobile_core.so
│   └── ...
├── mobile-core/                   # Rust 库（已有）
└── scripts/
    └── build_android.sh           # ← 一键编译 .so 并复制到 android/
```

---

## 一键构建脚本

创建 `p2p-camera/scripts/build_android.sh`：

```bash
#!/bin/bash
set -e

# Workspace root = p2p-camera/，必须从这里运行
WORKSPACE_DIR="$(cd "$(dirname "$0")/.." && pwd)"
ANDROID_DIR="$(cd "$(dirname "$0")/../android" && pwd 2>/dev/null || true)"
API_LEVEL=26

echo "=== Building mobile-core for Android ==="
cd "$WORKSPACE_DIR"

for TARGET in aarch64-linux-android armv7-linux-androideabi; do
    echo "--- Building $TARGET ---"
    cargo ndk --target "$TARGET" --platform "$API_LEVEL" build --release -p mobile-core
done

# 产物映射
declare -A TARGET_MAP=(
    ["aarch64-linux-android"]="arm64-v8a"
    ["armv7-linux-androideabi"]="armeabi-v7a"
)

if [ -n "$ANDROID_DIR" ]; then
    JNI_DIR="$ANDROID_DIR/app/src/main/jniLibs"
    for rust_target in "${!TARGET_MAP[@]}"; do
        abi="${TARGET_MAP[$rust_target]}"
        mkdir -p "$JNI_DIR/$abi"
        cp "target/$rust_target/release/libmobile_core.so" "$JNI_DIR/$abi/"
        echo "--- Copied to $JNI_DIR/$abi/libmobile_core.so"
    done
    echo "=== Done! .so files ready in android/app/src/main/jniLibs/ ==="
else
    echo "=== Done! No android/ directory found, .so left in target/ ==="
fi
```

使用方法：

```bash
chmod +x scripts/build_android.sh
./scripts/build_android.sh
```
