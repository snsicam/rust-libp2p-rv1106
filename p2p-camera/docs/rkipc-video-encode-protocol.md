# RKIPC 视频编码参数配置协议

> 来源: `RV1106_Linux_SDK/project/app/rkipc`  
> 版本: V1.0 (基于 rkipc 源码分析)  
> 日期: 2026-07-03

---

## 1. 概述

RKIPC 是 Rockchip RV1106 平台上的 IPC (IP Camera) 主程序。它通过 **Unix Domain Socket RPC** 对外暴露视频编码参数的 get/set 接口，外部程序可以实时修改编码参数（如分辨率、码率、帧率、编码格式等）而不需要重启进程。

---

## 2. 通信协议

### 2.1 传输层

| 属性 | 值 |
|------|-----|
| 协议类型 | Unix Domain Socket (Stream) |
| 服务端地址 | `/var/tmp/rkipc` |
| 编码字节序 | 本机字节序（Little-Endian on RV1106） |

### 2.2 消息帧格式

所有数据收发使用 `int` 对齐（4 字节）。函数名 — 参数两级协议：

```
[帧1] int len     → 函数名字符串字节数（不含 null terminator）
[帧2] char[len]   → 函数名字符串（如 "rk_video_set_gop"）
[帧3] 参数协议    → 根据具体函数定义（见第 4 节各函数详情）
```

服务端在 `map[]` 中查找函数名，匹配到则调用对应的 `ser_xxx(fd)` 处理函数。

---

## 3. 参数传递协议分类

### 3.1 整数类型 (int) — Get

```
Client → Server:  int stream_id        (4 bytes)
Server → Client:  int value            (4 bytes, 实际参数值)
                  int err              (4 bytes, 0=成功)
```

适用函数: `gop`, `max_rate`, `smartp_viridrlen`, `md_switch`, `md_sensebility`, `od_switch`, `image_quality`, `snapshot_interval_ms`, `enable_cycle_snapshot`

### 3.2 整数类型 (int) — Set

```
Client → Server:  int stream_id        (4 bytes)
                  int value            (4 bytes, 要设置的值)
Server → Client:  int ret/err          (4 bytes, 0=成功)
```

### 3.3 字符串类型 (char*) — Get

```
Client → Server:  int stream_id        (4 bytes)
Server → Client:  int len              (4 bytes, 字符串字节数，不含 null)
                  char[len] value      (字符串内容)
                  int err              (4 bytes)
```

### 3.4 字符串类型 (char*) — Set

```
Client → Server:  int stream_id        (4 bytes)
                  int len              (4 bytes, 字符串字节数)
                  char[len] value      (字符串内容)
Server → Client:  int ret              (4 bytes)
```

### 3.5 无 stream_id 参数 — Get

```
Server → Client:  int value            (4 bytes)
                  int err              (4 bytes)
```

适用函数: `rotation`, `image_quality`, `enable_cycle_snapshot`, `snapshot_interval_ms`, `md_switch`, `md_sensebility`, `od_switch`

### 3.6 无 stream_id 参数 — Set

```
Client → Server:  int value            (4 bytes)
Server → Client:  int err              (4 bytes)
```

### 3.7 无参数操作 — restart

```
Server → Client:  int err              (4 bytes)
```

### 3.8 批量设置 — rk_video_set (JSON 模式)

```
Client → Server:  int len              (4 bytes, JSON 字符串字节数)
                  char[len] json       (JSON 字符串)
Server → Client:  无返回值
```

> **注意**: 当前实现中 `ser_rk_video_set` 仅接收并打印 JSON，并未实际处理其内容（`LOG_DEBUG` 后直接 `return 0`）。批量设置暂不可用，请使用单项 set 接口。

---

## 4. 所有视频编码 RPC 函数一览

### 4.1 编码重启

| 函数名 | 类型 | 参数 | 说明 |
|--------|------|------|------|
| `rk_video_restart` | 无参操作 | — | 重启整个视频编码管线：先 deinit ISP/VI/VENC/存储，再重新 init |

> **注意**: `restart` 会短暂中断视频流，所有参数修改后需调用此函数的部分（见 4.2/4.3 中"需 restart"列）才会生效。

---

### 4.2 视频编码核心参数（立即生效）

以下参数通过 `RK_MPI_VENC_SetChnAttr` 直接修改硬件寄存器，**不需要 restart**：

| 函数名 | 参数类型 | 格式/范围 | 默认值 | 说明 |
|--------|---------|-----------|--------|------|
| `rk_video_get_gop` | int | 1 ~ 400 | 50 | GOP 长度（I帧间隔） |
| `rk_video_set_gop` | int | 1 ~ 400 | 50 | 设置 GOP，根据当前编码格式和 RC 模式写入对应 VENC 属性 |
| `rk_video_get_max_rate` | int | 依赖 stream_type | main:2048, sub:512 | 最大码率 (kbps) |
| `rk_video_set_max_rate` | int | 依赖 stream_type | — | CBR 模式下为固定码率；VBR 模式下 max_rate 是上限，min_rate=max_rate/3，mid_rate=max_rate*2/3 |
| `rk_video_get_smartp_viridrlen` | int | — | 25 | SmartP 虚拟 I 帧间隔 |
| `rk_video_set_smartp_viridrlen` | int | — | 25 | 设置 Virtual I-DR 帧间隔 |
| `rk_video_get_resolution` | string | `"W*H"` | `"2304*1296"` (main) | 当前分辨率 |
| `rk_video_set_resolution` | string | `"W*H"` 如 `"1920*1080"` | — | 修改分辨率，直接操作 VENC/VI 通道属性（不需要 restart） |
| `rk_video_get_frame_rate` | string | `"N"` 或 `"N/D"` | `"25"` (main), `"30"` (sub) | 输出帧率 |
| `rk_video_set_frame_rate` | string | `"N"` 或 `"N/D"` | — | 设置帧率。整数用 VI 帧率控制，非整数用 VENC 帧率控制 |
| `rk_video_get_frame_rate_in` | string | `"N"` | — | 输入帧率（源帧率） |
| `rk_video_set_frame_rate_in` | string | `"N"` | — | 设置输入源帧率 |
| `rk_video_get_rotation` | int | 0, 90, 180, 270 | 0 | 图像旋转角度 |
| `rk_video_set_rotation` | int | 0, 90, 180, 270 | — | 设置图像旋转 |

---

### 4.3 视频编码参数（需 restart 生效）

以下参数通过修改 INI 配置 + 调用 `rk_video_restart()` 生效：

| 函数名 | 参数类型 | 可选值 | 默认值 | 说明 |
|--------|---------|--------|--------|------|
| `rk_video_get_RC_mode` | string | `"CBR"`, `"VBR"` | `"CBR"` | 码率控制模式 |
| `rk_video_set_RC_mode` | string | `"CBR"`, `"VBR"` | — | 设置 RC 模式（自动 restart） |
| `rk_video_get_output_data_type` | string | `"H.264"`, `"H.265"` | `"H.265"` | 编码格式 |
| `rk_video_set_output_data_type` | string | `"H.264"`, `"H.265"` | — | 设置编码格式（自动 restart） |
| `rk_video_get_rc_quality` | string | `"lowest"`, `"lower"`, `"low"`, `"medium"`, `"high"`, `"higher"`, `"highest"` | `"high"` | 编码质量等级（调节 MinQp） |
| `rk_video_set_rc_quality` | string | 同上 | — | 设置编码质量（写入 VENC RC 参数，不自动 restart） |
| `rk_video_get_smart` | string | `"open"`, `"close"` | `"close"` | SmartP/SVC 智能编码开关 |
| `rk_video_set_smart` | string | `"open"`, `"close"` | — | 设置 smart（自动 restart） |
| `rk_video_get_gop_mode` | string | `"normalP"`, `"smartP"` | `"normalP"` | GOP 模式 |
| `rk_video_set_gop_mode` | string | `"normalP"`, `"smartP"` | — | 设置 GOP 模式（自动 restart） |
| `rk_video_get_stream_type` | string | `"mainStream"`, `"subStream"`, `"thirdStream"` | `"mainStream"` | 码流类型 |
| `rk_video_set_stream_type` | string | `"mainStream"`, `"subStream"`, `"thirdStream"` | — | 设置码流类型（仅写 INI，不 restart） |
| `rk_video_get_h264_profile` | string | `"high"`, `"main"`, `"baseline"` | `"high"` | H.264 编码档次 |
| `rk_video_set_h264_profile` | string | `"high"`, `"main"`, `"baseline"` | — | 设置 H.264 profile（自动 restart） |

---

### 4.4 JPEG 抓图参数

| 函数名 | 参数类型 | 范围/可选值 | 默认值 | 说明 |
|--------|---------|-------------|--------|------|
| `rk_video_get_enable_cycle_snapshot` | int | 0/1 | 0 | 周期抓图开关 |
| `rk_video_set_enable_cycle_snapshot` | int | 0/1 | — | 启用/禁用周期抓图 |
| `rk_video_get_image_quality` | int | — | 70 (qfactor) | JPEG 图片质量 |
| `rk_video_set_image_quality` | int | — | — | 设置 JPEG 质量 |
| `rk_video_get_snapshot_interval_ms` | int | 毫秒 | 1000 | 抓图间隔 |
| `rk_video_set_snapshot_interval_ms` | int | 毫秒 | — | 设置抓图间隔 |
| `rk_video_get_jpeg_resolution` | string | `"W*H"` | `"1920*1080"` | JPEG 抓图分辨率 |
| `rk_video_set_jpeg_resolution` | string | `"W*H"` | — | 设置 JPEG 分辨率 |

---

### 4.5 智能分析相关（IVS）

| 函数名 | 参数类型 | 范围 | 说明 |
|--------|---------|------|------|
| `rk_video_get_md_switch` | int | 0/1 | 移动侦测开关 |
| `rk_video_set_md_switch` | int | 0/1 | 设置后自动 restart |
| `rk_video_get_md_sensebility` | int | 1/2/3 | 移动侦测灵敏度 |
| `rk_video_set_md_sensebility` | int | 1/2/3 | 设置后自动 restart |
| `rk_video_get_od_switch` | int | 0/1 | 遮挡检测开关 |
| `rk_video_set_od_switch` | int | 0/1 | 设置后自动 restart |

---

## 5. stream_id 说明

RKIPC 支持 3 路编码通道：

| stream_id | INI section | 默认用途 | 默认分辨率 | 默认码率 |
|-----------|-------------|---------|-----------|---------|
| 0 | `[video.0]` | mainStream (主码流) | 2304×1296 | 2048 kbps |
| 1 | `[video.1]` | subStream (子码流) | 704×576 | 512 kbps |
| 2 | `[video.2]` | thirdStream (第三码流) | 960×540 | (默认关闭) |

---

## 6. 分辨率支持

根据 `capability.video` 中的配置：

| 码流类型 | 支持分辨率 |
|---------|-----------|
| mainStream | `2304*1296`, `1920*1080`, `1280*720`, `960*540`, `640*360`, `320*240` |
| subStream | `704*576`, `640*480`, `352*288`, `320*240` |
| thirdStream | `416*416` |

---

## 7. 主码流码率选项

根据 stream_type 不同，可选的最大码率值：
- **mainStream**: 256, 512, 1024, 2048, 3072, 4096, 6144 (kbps)
- **subStream**: 128, 256, 512 (kbps)
- **thirdStream**: 256, 512 (kbps)

---

## 8. 帧率支持

帧率格式：`"N"` 表示整数帧率，`"N/D"` 表示分数帧率（如 `"1/2"` 表示 0.5fps）。

可选帧率值：`"1/2"`, `"1"`, `"2"`, `"4"`, `"6"`, `"8"`, `"10"`, `"12"`, `"14"`, `"16"`, `"18"`, `"20"`, `"25"`, `"30"`

帧率受 `sFrameRateIn`（输入帧率）上限约束：`src ≤ src_in`。

---

## 9. RC Quality 质量等级与 MinQp 映射

| Quality 值 | H.264/H.265 MinQp | 说明 |
|------------|-------------------|------|
| `"highest"` | 10 | 最高画质，最大码率 |
| `"higher"` | 15 | 较高画质 |
| `"high"` | 20 | 高画质（默认） |
| `"medium"` | 25 | 中等画质 |
| `"low"` | 30 | 低画质 |
| `"lower"` | 35 | 较低画质 |
| `"lowest"` | 40 | 最低画质，最小码率 |

---

## 10. 完整 RPC 交互示例

### 10.1 获取主码流 GOP

```
Client → Server:
  0x0000000e                    // int: 14 (strlen("rk_video_get_gop"))
  "rk_video_get_gop"            // char[14]

Server 接收函数名 → 查表 → 调用 ser_rk_video_get_gop(fd)

Client → Server (ser 内部):
  0x00000000                    // int stream_id = 0 (主码流)

Server → Client:
  0x00000032                    // int value = 50 (GOP=50)
  0x00000000                    // int err = 0
```

### 10.2 设置主码流码率为 3072 kbps

```
Client → Server:
  0x00000013                    // int: 19 (strlen("rk_video_set_max_rate"))
  "rk_video_set_max_rate"       // char[19]

Server 接收函数名 → 查表 → 调用 ser_rk_video_set_max_rate(fd)

Client → Server (ser 内部):
  0x00000000                    // int stream_id = 0
  0x00000C00                    // int value = 3072

Server → Client:
  0x00000000                    // int err = 0
```

### 10.3 切换主码流编码格式为 H.264

```
Client → Server:
  0x0000001E                    // int: 30 (strlen("rk_video_set_output_data_type"))
  "rk_video_set_output_data_type"  // char[30]

Server 接收函数名 → 查表 → 调用 ser_rk_video_set_output_data_type(fd)

Client → Server (ser 内部):
  0x00000000                    // int stream_id = 0
  0x00000005                    // int len = 5
  "H.264"                       // char[5]

Server → Client:
  0x00000000                    // int ret = 0

Server 内部自动调用 rk_video_restart()，会短暂中断视频流
```

### 10.4 设置分辨率为 1920×1080

```
Client → Server:
  0x00000015                    // int: 21 (strlen("rk_video_set_resolution"))
  "rk_video_set_resolution"     // char[21]

Server 接收函数名 → 查表 → 调用 ser_rk_video_set_resolution(fd)

Client → Server (ser 内部):
  0x00000000                    // int stream_id = 0
  0x00000009                    // int len = 9
  "1920*1080"                   // char[9]

Server → Client:
  0x00000000                    // int ret = 0
```

### 10.5 设置帧率为 15

```
Client → Server:
  0x00000016                    // int: 22 (strlen("rk_video_set_frame_rate"))
  "rk_video_set_frame_rate"     // char[22]

Server 接收函数名 → 查表 → 调用 ser_rk_video_set_frame_rate(fd)

Client → Server (ser 内部):
  0x00000001                    // int stream_id = 1 (子码流)
  0x00000002                    // int len = 2
  "15"                          // char[2]

Server → Client:
  0x00000000                    // int ret = 0
```

---

## 11. 错误码

| 值 | 含义 |
|----|------|
| 0 | 成功 |
| -1 (SOCKERR_IO) | Socket 读写错误 |
| -2 (SOCKERR_CLOSED) | Socket 已关闭 |
| -3 (SOCKERR_INVARG) | 无效参数 |

函数返回 `-1` 表示连接断开，RPC 服务端会清理连接资源。

---

## 12. restart 触发规则总结

以下操作会**自动触发 `rk_video_restart()`**（即内部调用后立即重启编码管线）：

| 函数 | 自动 restart |
|------|:---:|
| `set_RC_mode` | ✅ |
| `set_output_data_type` | ✅ |
| `set_smart` | ✅ |
| `set_gop_mode` | ✅ |
| `set_h264_profile` | ✅ |
| `set_md_switch` | ✅ |
| `set_md_sensebility` | ✅ |
| `set_od_switch` | ✅ |

以下操作**不需要 restart，直接生效**：

| 函数 | 方式 |
|------|------|
| `set_gop` | 直接 RK_MPI_VENC_SetChnAttr |
| `set_max_rate` | 直接 RK_MPI_VENC_SetChnAttr |
| `set_resolution` | 直接 unbind → 修改 VI/VENC → rebind |
| `set_frame_rate` | 直接修改 VI/VENC 帧率属性 |
| `set_frame_rate_in` | 直接修改 VENC RC 属性 |
| `set_rc_quality` | 直接 RK_MPI_VENC_SetRcParam |
| `set_smartp_viridrlen` | 直接 RK_MPI_VENC_SetChnAttr |
| `set_rotation` | 直接修改 ISP 旋转 + restart (内部) |
| `set_stream_type` | 仅写 INI 文件，不修改硬件 |

---

## 13. INI 配置文件结构参考

RKIPC 启动时通过 `-c` 参数加载 INI 文件（默认 `/userdata/rkipc.ini`）。编码参数存储在 `[video.0]`、`[video.1]`、`[video.2]` 三个 section 中。

```
[video.0]                       # 对应 stream_id=0（主码流）
width = 2304
height = 1296
rc_mode = CBR                   # CBR | VBR
rc_quality = high               # lowest|lower|low|medium|high|higher|highest
output_data_type = H.265        # H.264 | H.265
smart = close                   # open | close
h264_profile = high             # baseline | main | high
gop = 50                        # 1~400
gop_mode = normalP              # normalP | smartP
smartp_viridrlen = 25
max_rate = 2048                 # kbps
mid_rate = 1024
min_rate = 0
src_frame_rate_num = 25         # 输入帧率分子
src_frame_rate_den = 1
dst_frame_rate_num = 25         # 输出帧率分子
dst_frame_rate_den = 1
stream_type = mainStream        # mainStream|subStream|thirdStream
stream_smooth = 50              # 1~100
```

---

## 14. 源码文件清单

| 文件 | 行数 | 功能 |
|------|------|------|
| `rkipc/common/socket_server/socket.h` | 20 | Socket 基础 API、路径宏定义 |
| `rkipc/common/socket_server/server.c` | ~5861 | Socket 服务端：监听、函数名路由、所有 ser_ 函数 |
| `rkipc/src/rv1106_ipc/video/video.h` | 77 | 视频编码 API 头文件声明 |
| `rkipc/src/rv1106_ipc/video/video.c` | ~3440 | 视频编码核心实现：init/deinit/各参数 get/set |
| `rkipc/src/rv1106_ipc/main.c` | 198 | 主入口：初始化顺序 |
| `rkipc/common/param/param.h` / `param.c` | — | INI 参数读写（`rk_param_get_int/set_int` 等） |
| `out/share/rkipc-300w-2304x1296.ini` | ~543 | 默认 INI 配置文件 |

---

## 15. 注意事项

1. **字节序**: 协议未做网络字节序转换，直接使用本机字节序（Little-Endian）。跨平台客户端需要注意。

2. **字符串不含 null**: `len` 字段标记字符串字节数，**不含 null terminator**。服务端内部会 `malloc(len)` 读取，不依赖 null-terminated。

3. **并发**: Socket 服务端是多线程的（为每个连接创建线程），但底层 RK MPI 函数并非全部线程安全。建议串行调用或加锁。

4. **restart 影响**: `rk_video_restart` 会完整重启 ISP → VI → VENC 管线，持续约 1-3 秒，期间视频流中断。

5. **码率切换精度**: VBR 模式下 `set_max_rate` 会按 `min=1/3*max`, `mid=2/3*max`, `max` 三档分配码率。CBR 模式下直接设为固定码率。

6. **分辨率切换**: `set_resolution` 不调用 restart，而是直接 unbind → 修改属性 → rebind。切换期间会出现短时间视频中断。

7. **rk_video_set 批量接口**: 当前实现仅为占位（`LOG_DEBUG` 打印 JSON），尚未实现批量参数设置。请使用单项 set 接口。
