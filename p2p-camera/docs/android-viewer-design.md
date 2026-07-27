# P2P Camera Android Viewer — 设计文档

> 版本: 1.0 | 日期: 2026-07-08

## 1. 概述

**目标**: 在 Android 手机上运行 P2P Camera Viewer，通过 P2P 网络接收 RV1106 摄像头发送的 H.265 视频流，实时解码渲染。

**核心指标**:
- 端到端延迟 ≤ 300ms（Rust Jitter Buffer 100ms + MediaCodec 硬解 ~50ms + 渲染 ~1 帧）
- 自动码流切换（Relay → sub 码流，直连 → main 码流）
- DCUtR 直连升级无缝切换

**技术栈**:
```
┌─────────────────────────────────────────────────────────────────┐
│  Android App (Kotlin)                                           │
│  ┌──────────┐  ┌──────────────────┐  ┌──────────────────────┐   │
│  │   UI     │  │  Video Pipeline  │  │  Audio Pipeline      │   │
│  │SurfaceView│  │  FramePoller →   │  │  FramePoller →       │   │
│  │  + info  │  │  MediaCodec(H265)│  │  AudioTrack(PCM)     │   │
│  └──────────┘  └────────┬─────────┘  └──────────┬───────────┘   │
│                         │                        │               │
│                    ┌────┴────────────────────────┴──────┐        │
│                    │  RustBridge (JNI)                   │        │
│                    │  nativePollVideoFrame()             │        │
│                    │  nativePollAudioFrame()             │        │
│                    │  nativePollEvent()                  │        │
│                    └─────────────────┬──────────────────┘        │
└──────────────────────────────────────┼───────────────────────────┘
                                       │ JNI (libmobile_core.so)
┌──────────────────────────────────────┼───────────────────────────┐
│  Rust mobile-core                    │                           │
│  ┌────────────────────┐              │                           │
│  │  P2P Swarm          │◄── Relay / DCUtR / mDNS               │
│  │  (libp2p)           │                                         │
│  └────────┬───────────┘                                          │
│           │ H.265 NAL + G.711/PCM frames                        │
│  ┌────────┴───────────┐                                          │
│  │  Jitter Buffer      │                                         │
│  │  Video: 100ms       │                                         │
│  │  Audio: 50ms        │                                         │
│  └────────┬───────────┘                                          │
│           │ crossbeam_channel (bounded)                          │
│  ┌────────┴───────────┐                                          │
│  │  JNI Bridge         │  ← 导出给 Android 调用                  │
│  └────────────────────┘                                         │
└─────────────────────────────────────────────────────────────────┘
```

---

## 2. 关键设计决策

### 2.1 为什么不直接使用 ExoPlayer

| 问题 | 说明 |
|------|------|
| **容器格式依赖** | ExoPlayer 的 Extractor 要求输入为 MP4/MKV/TS 等容器格式，不能直接消费裸 H.265 NAL |
| **伪码流开销** | 如果要在 Rust 侧将 H.265 NAL 封装为 fMP4/fMP4 片段再喂给 ExoPlayer，需要 `<em>引入 muxer 依赖，增加复杂度 |
| **延迟不可控** | ExoPlayer 面向文件/VOD 播放设计缓冲策略，即使调整 LoadControl 也很难做到 < 300ms 端到端延迟 |

**结论**: 使用 Android 系统 API（`MediaCodec` + `AudioTrack`）直接解码渲染，ExoPlayer 仅作为UI控件依赖（`PlayerView` 作为 Surface 容器）。

### 2.2 核心技术组件

| 组件 | 用途 | 输入格式 | 输出 |
|------|------|---------|------|
| `MediaCodec` | H.265 硬件解码 | H.265 NAL 单元 (byte[]) | Surface (零拷贝渲染) |
| `AudioTrack` | 音频播放 | PCM16LE (byte[]) | 扬声器输出 |
| `SurfaceView` | 视频渲染容器 | Surface (来自 MediaCodec) | 屏幕 |

---

## 3. Android 项目结构

```
p2p-camera/android-media/demos/mediaplayer/          ← 新建
├── build.gradle                                     # 依赖声明
├── src/main/
│   ├── AndroidManifest.xml
│   ├── java/com/p2pcamera/viewer/
│   │   ├── RustBridge.kt              # JNI native 声明 + 句柄管理
│   │   ├── MainActivity.kt            # 主界面
│   │   ├── ViewerViewModel.kt         # 连接状态管理
│   │   ├── video/
│   │   │   └── H265Decoder.kt         # MediaCodec 封装
│   │   └── audio/
│   │       └── PcmAudioPlayer.kt      # AudioTrack 封装
│   ├── jniLibs/
│   │   ├── arm64-v8a/libmobile_core.so
│   │   ├── armeabi-v7a/libmobile_core.so
│   │   └── x86_64/libmobile_core.so
│   └── res/
│       ├── layout/
│       │   └── activity_main.xml       # SurfaceView + 状态 UI
│       └── values/
│           └── strings.xml
```

### 3.1 依赖说明 (build.gradle)

```groovy
apply from: '../../../constants.gradle'
apply plugin: 'com.android.application'

android {
    namespace 'com.p2pcamera.viewer'
    compileSdk project.ext.compileSdkVersion  // 36

    compileOptions {
        sourceCompatibility JavaVersion.VERSION_11
        targetCompatibility JavaVersion.VERSION_11
    }

    defaultConfig {
        versionName '1.0'
        versionCode 1
        minSdkVersion 26                                    // 最低 API 26
        targetSdkVersion project.ext.appTargetSdkVersion    // 34
        ndk { abiFilters 'arm64-v8a', 'armeabi-v7a', 'x86_64' }
    }

    buildTypes {
        release {
            shrinkResources true
            minifyEnabled true
            proguardFiles getDefaultProguardFile('proguard-android.txt')
        }
    }

    sourceSets {
        main {
            jniLibs.srcDirs = ['src/main/jniLibs']
        }
    }
}

dependencies {
    // Media3 UI (PlayerView 作为 Surface 容器)
    implementation project(modulePrefix + 'lib-ui')

    // Kotlin coroutines (协程替代线程)
    implementation 'org.jetbrains.kotlinx:kotlinx-coroutines-android:1.9.0'

    // AndroidX
    implementation 'androidx.appcompat:appcompat:1.6.1'
    implementation 'androidx.constraintlayout:constraintlayout:2.1.4'
    implementation 'androidx.lifecycle:lifecycle-viewmodel-ktx:2.5.1'
}
```

> **说明**: 不需要依赖 `lib-exoplayer`。只用 `lib-ui` 提供的 `SurfaceView` 相关工具类（`AspectRatioFrameLayout` 等），如果 `lib-ui` 依赖过重则直接只用 Android 原生 `SurfaceView`。

### 3.2 AndroidManifest.xml

```xml
<manifest xmlns:android="http://schemas.android.com/apk/res/android">
  <uses-permission android:name="android.permission.INTERNET"/>
  <uses-permission android:name="android.permission.ACCESS_NETWORK_STATE"/>
  <uses-permission android:name="android.permission.ACCESS_WIFI_STATE"/>
  <uses-sdk/>
  <application
      android:allowBackup="false"
      android:icon="@mipmap/ic_launcher"
      android:label="@string/app_name">
    <activity
        android:name="com.p2pcamera.viewer.MainActivity"
        android:exported="true"
        android:configChanges="orientation|screenSize"
        android:theme="@style/Theme.AppCompat.NoActionBar">
      <intent-filter>
        <action android:name="android.intent.action.MAIN"/>
        <category android:name="android.intent.category.LAUNCHER"/>
      </intent-filter>
    </activity>
  </application>
</manifest>
```

---

## 4. 核心模块详细设计

### 4.1 RustBridge — JNI 桥接层

#### 职责
- 加载 `libmobile_core.so`
- 映射 `native*` JNI 方法到 Rust 对应的 JNI 导出函数
- 管理 ViewerHandle（long 类型指针，在 Rust 侧映射到全局句柄表索引）
- **增加 `nativePollAudioFrame()`** — 当前 JNI bridge 只导出了视频轮询，需补充音频

#### API（已有 + 待补）

```kotlin
object RustBridge {
    init { System.loadLibrary("mobile_core") }

    // ─── 生命周期 ───
    external fun nativeCreate(): Long
    external fun nativeDestroy(handle: Long)

    // ─── 控制 ───
    // json: {"relays":["/ip4/.../tcp/.../p2p/..."],"deviceId":"12D3..."}
    external fun nativeConnect(handle: Long, json: String): Boolean

    // ─── 数据轮询（非阻塞） ───
    external fun nativePollVideoFrame(handle: Long): ByteArray?   // H.265 NAL
    external fun nativePollAudioFrame(handle: Long): ByteArray?   // PCM/G.711 (待补)

    // ─── 事件 ───
    // 返回 JSON: {"type":"Connected","peer_id":"...","connection_type":"relay"}
    external fun nativePollEvent(handle: Long): String?
}
```

#### ⚠️ 待补充: 音频轮询 JNI 导出

当前 `jni_bridge.rs` 只轮询视频帧（`poll_video_frame()`），需要补充音频。

Rust 侧修改：
1. 在 `ViewerHandle` 中增加 `audio_rx: Receiver<MediaPacket>`
2. 后台线程循环中增加音频帧轮询
3. 导出 `Java_com_p2pcamera_viewer_RustBridge_nativePollAudioFrame`

---

### 4.2 H265Decoder — MediaCodec 封装

#### 设计原理

```
Rust JNI                 H265Decoder                    MediaCodec
    │                        │                              │
    │ nativePollVideoFrame() │                              │
    │ ──── ByteArray ──────→ │                              │
    │                        │ configure(CSD-0, Surface)    │
    │                        │ ────────────────────────────→│
    │                        │ queueInputBuffer(NAL)        │
    │                        │ ────────────────────────────→│
    │                        │                              │ decode async
    │                        │  onOutputBufferAvailable()   │
    │                        │ ←────────────────────────────│
    │                        │  renderOutputBuffer()        │
    │                        │  (自动渲染到 Surface)         │
```

#### 关键实现细节

```
H265Decoder
├── configure(surface: Surface)
│   - Codec: "video/hevc" → MediaCodecList 选择硬解
│   - Color format: COLOR_FormatSurface (零拷贝)
│   - CSD-0: VPS+SPS+PPS (从首帧 IDR 解析或设备硬编码)
│   - AsyncCallback 模式
│
├── feedFrame(nalBytes: ByteArray, isKeyframe: Boolean)
│   - 包装为 MediaCodec.BufferInfo
│   - BUFFER_FLAG_CODEC_CONFIG: CSD 数据
│   - BUFFER_FLAG_SYNC_FRAME: IDR 关键帧
│   - 普通帧: flags=0
│   - queueInputBuffer() 非阻塞
│
├── release()
│   - signalEndOfInputStream()
│   - stop() → release()
```

#### ⚠️ CSD (Codec-Specific Data) 问题

H.265 硬解需要 VPS+SPS+PPS 作为 CSD-0 配置。项目使用的 RV1106 编码器可能在每个 IDR 帧前都附带 VPS/SPS/PPS（内联参数集），也可能在首帧单独发送。

**策略**:
1. 首帧到达时检查是否有 VPS/SPS/PPS NAL（类型 32/33/34）
2. 如果有，提取为 CSD-0 缓冲区传入 MediaCodec
3. 如果没有内联参数集 → 使用设备端预编码的静态 CSD 数据（由 RV1106 固件提供）

**NAL 类型判断**:
```
NAL type = (nalBytes[0] >> 1) & 0x3F
VPS: 32
SPS: 33
PPS: 34
IDR_W_RADL: 19
IDR_N_LP: 20
P/B/TRAIL: 0..18
```

---

### 4.3 PcmAudioPlayer — AudioTrack 封装

#### 关键参数（需与 RV1106 编码配置一致）

| 参数 | 值 | 说明 |
|------|-----|------|
| 采样率 | 16000 | 16kHz，窄带语音 |
| 声道 | 1 | Mono |
| 编码 | PCM16LE | 16-bit little-endian |
| Buffer Size | 640 bytes | 20ms @ 16kHz mono 16bit |

```kotlin
class PcmAudioPlayer {
    private val track = AudioTrack.Builder()
        .setAudioAttributes(AudioAttributes.Builder()
            .setUsage(AudioAttributes.USAGE_MEDIA)
            .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
            .build())
        .setAudioFormat(AudioFormat.Builder()
            .setSampleRate(16000)
            .setChannelMask(AudioFormat.CHANNEL_OUT_MONO)
            .setEncoding(AudioFormat.ENCODING_PCM_16BIT)
            .build())
        .setBufferSizeInBytes(640 * 4)  // 4 x 20ms = 80ms 音频缓冲
        .setTransferMode(AudioTrack.MODE_STREAM)
        .build()

    fun write(pcmBytes: ByteArray) {
        track.write(pcmBytes, 0, pcmBytes.size)
    }

    fun play() { track.play() }
    fun release() { track.stop(); track.release() }
}
```

#### G.711 音频转换（如需要）

RV1106 硬件编码器可能输出 G.711A (flags=2) 或 G.711U (flags=3)。如果是 G.711 编码，需要在 Android 侧做 G.711→PCM16 转换后再交给 AudioTrack。

```kotlin
object G711Decoder {
    // G.711A μ-law → PCM16 (查表法，性能极高)
    fun decodeALaw(g711Bytes: ByteArray): ShortArray {
        // 1 byte → 1 sample (8kHz) → 6 bytes padding → 256 samples
        // 使用预计算查找表，O(n) 转换
    }
}
```

> 也可以要求 RV1106 固件输出 PCM16LE 原始音频，避免 Android 侧转换开销。

---

### 4.4 ViewerViewModel — 连接状态管理

```kotlin
enum class ConnectionState {
    IDLE,        // 未连接
    CONNECTING,  // 正在连接 Relay
    CONNECTED,   // 已连接（可能经过 Relay）
    DIRECT,      // 直连（DCUtR 成功或 LAN 发现）
    DISCONNECTED // 断开
}

class ViewerViewModel : ViewModel() {
    // ─── 配置 ───
    data class Config(
        val relays: List<String>,   // 如 ["/ip4/1.2.3.4/tcp/5001/p2p/12D3..."]
        val deviceId: String         // DeviceCam 的 PeerId
    )

    // ─── LiveData / StateFlow ───
    val connectionState: StateFlow<ConnectionState>
    val natInfo: StateFlow<String>              // NAT 诊断信息
    val streamInfo: StateFlow<String>           // 码流信息 (main/sub)

    // ─── 生命周期 ───
    fun connect(config: Config)   // 调用 nativeCreate + nativeConnect
    fun disconnect()              // 调用 nativeDestroy

    // ─── 内部轮询 ───
    // viewModelScope 后台协程:
    //   - 每 5ms 轮询 nativePollEvent() → 更新 connectionState
    //     - Disconnected 事件: Rust 侧已自动重连，Android 侧更新 UI 状态
    //     - Connecting 事件: 显示"重连中"
    //     - Connected + StreamReady: 恢复播放
    //   - 每 5ms 轮询 nativePollVideoFrame() → → H265Decoder.feedFrame()
    //   - 每 10ms 轮询 nativePollAudioFrame() → → PcmAudioPlayer.write()
}
```

---

## 5. 线程模型

```
┌────────────────────────────────────────────────────┐
│  Rust 后台线程 (std::thread::spawn)                 │
│  ┌────────────────┐    ┌─────────────────────────┐ │
│  │ tokio runtime  │    │ pull_swarm() + poll loop │ │
│  │ (block_on)     │    │ → frame_tx / event_tx   │ │
│  └────────────────┘    └─────────────────────────┘ │
│                    crossbeam bounded channels       │
└──────────────────────┬─────────────────────────────┘
                       │ JNI 边界
┌──────────────────────┴─────────────────────────────┐
│  Kotlin 层                                         │
│                                                    │
│  ┌───────────────────────────────┐                 │
│  │ viewModelScope coroutine      │                 │
│  │   while (running) {            │                 │
│  │     delay(5)                   │    ← 主轮询循环 │
│  │     pollEvents()               │                 │
│  │     pollVideoFrame → decoder   │                 │
│  │     pollAudioFrame → player    │                 │
│  │   }                            │                 │
│  └───────────────────────────────┘                 │
│                                                    │
│  ┌───────────────────────────────┐                 │
│  │ MediaCodec AsyncCallback      │ ← 解码回调线程  │
│  │   onOutputBufferAvailable()   │                 │
│  │   → renderOutputBuffer()      │                 │
│  └───────────────────────────────┘                 │
│                                                    │
│  ┌───────────────────────────────┐                 │
│  │ Main Thread (UI)              │ ← SurfaceView   │
│  │   SurfaceView rendering       │   渲染           │
│  │   State updates via StateFlow │                 │
│  └───────────────────────────────┘                 │
└────────────────────────────────────────────────────┘
```

### 线程间数据结构

```
Rust crossbeam_channel (有界, 非阻塞 try_recv)
    frame_tx → frame_rx (capacity: 120 frames)
    event_tx → event_rx (capacity: 32 events)

Android 侧（Kotlin）
    无需额外队列, JNI 直接 try_recv 到 Kotlin ByteArray
```

**为什么不用 BlockingQueue**: JNI 的 `try_recv` 已经是非阻塞的，Kotlin 侧 `nativePollVideoFrame` 返回 null 表示无帧，`delay(5ms)` 控制轮询间隔。不需要再放一层 Java 队列。

---

## 6. UI 布局

```xml
<!-- activity_main.xml -->
<androidx.constraintlayout.widget.ConstraintLayout>

    <!-- 视频播放区域 -->
    <SurfaceView
        android:id="@+id/surface_video"
        android:layout_width="match_parent"
        android:layout_height="0dp"
        app:layout_constraintTop_toTopOf="parent"
        app:layout_constraintBottom_toTopOf="@id/layout_info"
        android:keepScreenOn="true" />

    <!-- 连接状态 + 码流信息 -->
    <LinearLayout
        android:id="@+id/layout_info"
        android:orientation="vertical"
        android:layout_width="match_parent"
        android:layout_height="wrap_content"
        app:layout_constraintBottom_toBottomOf="parent">

        <TextView android:id="@+id/txt_connection" />  <!-- "连接中..." / "直连 15fps" -->
        <TextView android:id="@+id/txt_nat" />         <!-- NAT 诊断信息 -->
        <TextView android:id="@+id/txt_stream" />      <!-- "主码流: 1280x720" -->

        <Button
            android:id="@+id/btn_reconnect"
            android:text="重连" />
    </LinearLayout>
</androidx.constraintlayout.widget.ConstraintLayout>
```

---

## 7. 数据流协议回顾

### MediaPacket 格式（Rust → JNI）

```
Offset  Size  Field
0       1     Track (0x01=Video, 0x02=Audio)
1       8     Timestamp (ms, big-endian u64)
9       1     Flags
                Video: 保留 (置 0); 关键帧由接收端字节扫描判定, 不经此字段
                Audio: bit0-1: 0→PCM16LE, 1→AAC, 2→G711A, 3→G711U
10      4     DataLen (big-endian u32)
14      N     Data (H.265 NAL units / audio samples)
```

### Android 侧需要的 JNI 数据转换

| 字段 | JNI 返回 | Android 消费方式 |
|------|---------|----------------|
| `data` (H.265 NAL) | `ByteArray` | 直接喂 `MediaCodec.queueInputBuffer()` |
| `flags` (is_keyframe) | `ByteArray[0]` 解析 | `MediaCodec.BUFFER_FLAG_SYNC_FRAME` |
| `timestamp_ms` | 暂无（需补充） | `MediaCodec.BufferInfo.presentationTimeUs` |

#### ⚠️ 待办：JNI 增加 PTS 返回

当前 `nativePollVideoFrame` 只返回 `ByteArray`（NAL 裸数据），**缺少 PTS 时间戳**。

**方案**: 在 `jni_bridge.rs` 中，将 `MediaPacket` 的 `timestamp_ms` 和 `is_keyframe` 信息编码进返回的 byte[] 前面：

```
返回格式: [flags:2B][pts_ms:8B][nal_data:N]
          0-1      2-9       10..N
```

或在 Kotlin 侧增加第二个 JNI 调用 `nativePollVideoPts(handle: Long): Long` 获取上帧 PTS。

**推荐**: 在 `nativePollVideoFrame` 返回数据前预留 10 字节头，避免两次 JNI 调用。

---

## 8. 错误处理与边缘场景

### 8.1 连接断开检测

```
ConnctionClosed (num_established == 0) 或 Stream EOF
  │
  ├─ viewer.rs 发送 MediaPlayerEvent::Disconnected
  ├─ jni_bridge.rs 检测到 Disconnected 事件
  │   ├─ 发送 ViewerEvent::Disconnected → Android 侧 nativePollEvent() 返回 {"type":"Disconnected"}
  │   ├─ 发送 ViewerEvent::Connecting → Android 侧显示"重连中"
  │   ├─ 调用 viewer.reconnect() 自动重连
  │   │   ├─ 成功: 发送 ViewerEvent::Connected + StreamReady → 恢复播放
  │   │   └─ 失败: 发送 ViewerEvent::Error → 退出
  │   └─ （自动重连，无需用户手动触发）
  ├─ MediaCodec signalEndOfInputStream()
  ├─ AudioTrack stop()
  └─ → UI 显示"连接断开"（如重连失败）

DCUtR 升级→码流切换→旧 circuit 关闭 (num_established > 0)
  │
  └─ 忽略，已在 viewer.rs 中正确区分
```

### 8.2 视频解码错误

```
MediaCodec 输出格式变更 (onOutputFormatChanged)
  │
  ├─ 获取新 width/height → UI 更新码流信息
  └─ 无需重建 codec（Surface 模式自动适配）

缓冲溢出 (queueInputBuffer 返回 -1)
  │
  └─ 丢弃当前帧，继续下一帧（Jitter Buffer 已做跳帧）
```

### 8.3 Surface 生命周期

```
Activity.onResume  → 确保 MediaCodec Surface 有效
Activity.onPause   → 不做操作（后台继续解码节省重建开销）
                     （可选: 暂停视频渲染，音频继续播放）

Surface destroyed   → MediaCodec.signalEndOfInputStream()
Surface created     → MediaCodec.configure(surface) + start()
                      ↑ 需要重新启动 MediaCodec
```

### 8.4 低内存 / 后台限制

```
onTrimMemory(TRIM_MEMORY_UI_HIDDEN)
  → 降低轮询间隔 (delay 50ms → 100ms)
  → 音频暂停，保留视频解码

onLowMemory()
  → 断开 P2P 连接，完全停止解码
```

---

## 9. 构建流程

### 9.1 一键构建脚本

```bash
# p2p-camera/scripts/build_android.sh（已存在）
# Step 1-2: 编译 mobile-core .so，复制到 jniLibs/

# Step 3: Gradle 编译 APK
cd p2p-camera/android-media
ANDROID_HOME=$HOME/android-sdk ./gradlew :demos:mediaplayer:assembleRelease
```

### 9.2 开发工作流

```bash
# 1. 修改 Rust 代码后
./scripts/build_android.sh                    # 编译 .so + 复制到 jniLibs

# 2. 用 Android Studio 打开 android-media/ 目录
# 3. 选择 demos:mediaplayer 模块运行
```

---

## 10. 实现步骤与优先级

### Step 1: Rust 侧补充（当前进行中）

| 子任务 | 状态 | 说明 |
|--------|------|------|
| ~~JNI bridge 基本框架~~ | ✅ 完成 | nativeCreate/Connect/PollVideo/Event/Destroy |
| ~~p2p-camera workspace 修复~~ | ✅ 完成 | 独立 [workspace] 定义 |
| ~~Android cross-compile 调试~~ | ✅ 完成 | cargo-ndk 参数修正 |
| ~~断连检测 + 自动重连~~ | ✅ 完成 | MediaPlayerEvent::Disconnected + reconnect() + ViewerEvent::Disconnected |
| **nativePollVideoFrame 增加 PTS** | ❌ 待做 | 返回数据前 2B flags + 8B timestamp |
| **nativePollAudioFrame 导出** | ❌ 待做 | 音频帧轮询 + JNI 导出 |
| **构建脚本集成** | ❌ 待做 | `build_android.sh` 自动复制 .so |

### Step 2: Android 项目新建

| 子任务 | 说明 |
|--------|------|
| 创建 `demos/mediaplayer/` 目录 | `build.gradle` + `AndroidManifest.xml` |
| 注册到 Gradle settings | `core_settings.gradle` 或新建 `settings.gradle` |
| 验证 Gradle sync 通过 | 能编译空 Activity |

### Step 3: 核心管道实现

| 子任务 | 说明 |
|--------|------|
| `RustBridge.kt` | JNI 加载 + native 声明 |
| `H265Decoder.kt` | MediaCodec 封装 + CSD 处理 |
| `PcmAudioPlayer.kt` | AudioTrack 封装 |
| `ViewerViewModel.kt` | 后台协程轮询 |

### Step 4: UI + 集成

| 子任务 | 说明 |
|--------|------|
| `MainActivity.kt` | SurfaceView + 状态显示 |
| `activity_main.xml` | 布局 |
| 端到端测试 | 连接真实 DeviceCam 验证 |

---

## 11. 风险与缓解

| 风险 | 影响 | 缓解方案 |
|------|------|---------|
| **CSD 参数集不完整** | MediaCodec 无法初始化 | 回退到字节流模式 (ByteBuffer)，手动提取 VPS/SPS/PPS；或从 DeviceCam 拉取静态 CSD |
| **G.711 音频兼容** | 无声音 | Android 侧查表转 PCM16LE，或在 DeviceCam 侧配置输出 PCM |
| **Android 版本差异** | MediaCodec 行为不一致 | 最低 API 26，使用 AsyncCallback（API 21+）和 Surface 输入模式（API 21+） |
| **Surface 重建开销** | Activity 切换时黑屏 500ms+ | 可选: 使用 TextureView 替代 SurfaceView（支持动画但性能稍差） |
| **DCUtR 升级切换延迟** | 码流切换时短暂黑屏 | Viewer 内部已处理: 先 open main stream 再关闭 sub stream |

---

## 12. 附录

### A. 相关文件索引

| 文件 | 用途 |
|------|------|
| `mobile-core/src/jni_bridge.rs` | JNI 导出函数（待补充音频+PTS） |
| `mobile-core/src/viewer.rs` | MediaPlayer 核心逻辑 |
| `mobile-core/src/jitter_buffer.rs` | 音视频 Jitter Buffer |
| `proto/src/media_packet.rs` | MediaPacket 协议定义 |
| `scripts/build_android.sh` | 交叉编译脚本 |
| `android-media/demos/surface/` | 参考: 最简 Media3 demo |
| `android-media/constants.gradle` | SDK 版本常量 |

### B. Android API 要求

| 功能 | 最低 API |
|------|---------|
| MediaCodec H.265 硬解 | 21 (大多数设备支持) |
| MediaCodec AsyncCallback | 21 |
| Surface 输入模式 | 21 |
| AudioTrack Builder | 21 |
| Kotlin coroutines | — |
| 本项目 minSdk | **26 (Android 8.0)** 覆盖 95%+ 设备 |
