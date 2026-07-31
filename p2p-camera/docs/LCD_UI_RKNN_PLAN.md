# LCD 局部显示 + 三进程架构方案（cam / UI / rknn）

> 整理自 2026-07-24 的对话。目标：把"LCD 屏局部显示视频"与"rknn 推理"落到
> **cam（设备端推流进程）、独立 UI 程序、rknn** 三者并存的架构里，形成可决策方案。
> 本文档只做决策依据，具体代码待方案拍板后实施。

---

## 1. 背景与目标

- 设备端需同时运行三个程序：**device-cam**（p2p 推流）、**独立 UI 程序**（LVGL，负责屏幕交互）、**rknn**（目标检测）。
- 某 UI 功能要求在 LCD 上**局部显示**视频（视频占屏幕子矩形，其余区域画 UI 控件 + 检测框）。
- 现状：device-cam 里已有 LCD 显示代码（`rk_camera.c` 的 `lcd_vo_init/thread/deinit`，走 **硬件 VO video plane**，稳 20fps）。该代码参考自 `luckfox_pico_yolov5` 例子，但例子本身用的是 `cvtColor+memcpy /dev/fb0` 的**软件死路**（~60% CPU / 7fps），我们已升级为硬件 VO，**不应退回**。

---

## 2. 硬约束（不可绕过的芯片/系统限制）

| # | 约束 | 来源 | 影响 |
|---|------|------|------|
| C1 | **ISP / rkaiq 是用户态单实例**，整颗 sensor 只能由**一个进程** init/start | 在板验证 | **cam 必须是视频源唯一拥有者**；UI、rknn 都只能做"下游消费者"，绝不能再各自 init ISP |
| C2 | **selfpath 是 ISP 唯一的预览路径**（`/dev/video12`，单一） | 在板验证 | 第二个进程直接抓 selfpath 会与 cam 抢，行为不确定 → 原始视频帧只能由 cam 产出再分发 |
| C3 | **VPSS group 不能混用 AUTO/USER** | 在板验证 | LCD 不能走 VPSS 通道，只能走 selfpath → VO（已是现状） |
| C4 | **UI = LVGL，渲染目标 `/dev/fb0`**（独占 VOP graphic plane） | 用户确认 | 视频不能也写 fb0（双进程写同一 fb0 会撕裂）→ 视频必须走另一条硬件层 |
| C5 | Rockchip VOP 默认 **video plane 在 graphic plane 之上** | 芯片特性 | 要让 LVGL 的框显示在视频上方，需调层序/alpha（见风险 R1） |

---

## 3. 已确定的结论（无需再议）

- **D1 · rknn 放 cam**：rknn 例子是单进程 monolith（capture + 推理同循环共享 `cv::Mat`），而 C1/C2 使 cam 是唯一原始帧拥有者。rknn 跟 capture 同进程（=cam）是原生写法；放 UI 则需 cam 经 IPC 传大块像素或 UI 争 selfpath（不可行）。
- **D2 · 显示继续用硬件 VO，不退回软件 fb0**：保留 device-cam 的 `RK_MPI_VO_SendFrame` video plane 路径（零拷贝、零 CPU 合成、稳 20fps）。
- **D3 · 检测框不烧进视频像素**：由 UI 在 graphic 图层叠加矢量框，视频像素零跨进程拷贝，仅 bbox 元数据走 IPC。
- **D4 · cam→UI 的 bbox 通道用轻量共享内存环形队列（或 UDS）**：每帧仅数个 `(x,y,w,h,cls,score)`，带宽可忽略。cam 需把 box 从模型空间（640×640 letterbox）映射回**屏幕子矩形坐标**再发送。
- **D5 · 代码组织采用 C 方案（模块提取，更优）**：把 cam 侧 LCD/VO 代码从 `rk_camera.c` 抽成独立模块 **`lcd_preview.c` + `lcd_preview_start/stop` API**。device-cam 继续链接该模块（行为零变化、零风险）；同源码还能另编出 **standalone `lcd-preview` 二进制**（离线取景器 / 上板测试用）。此维度独立于"显示机制 A/B"，无论选 A 还是 B 都先做 C。

---

## 4. 两个独立维度（结论 = C + A）

本方案由两个**正交**维度叠加，请勿混为一谈：

| 维度 | 含义 | 选项 | 结论 |
|------|------|------|------|
| **维度一 · 代码组织** | cam 侧 LCD/VO 代码怎么组织 | C=抽成独立 `lcd_preview` 模块（device-cam 链接不变 + 可出 standalone）/ 不抽（留在 `rk_camera.c`） | **C（更优，已定 D5）** |
| **维度二 · 显示机制** | 三进程里视频怎么局部显示 | A=VO video plane + LVGL 叠加 / B=视频做 LVGL 图像控件 | **A（推荐，见下）** |

> 最终推荐 = **C + A**：模块提取（代码更干净、可独立测试）＋ VO video plane 显示（性能最优）。
> 维度一与维度二互不冲突，即使显示机制改选 B，C 仍照做。

### 维度二待决策：显示机制 A / B

这是维度二中唯一需要拍板的分叉。两者都满足"局部显示 + 检测框"，区别在**视频像素是否进 LVGL**。

### 方案 A（推荐 · 性能最优）

```
cam:  selfpath帧 ──► rknn(推理, 独立线程) ──► bbox 元数据 ──┐
      selfpath帧 ──► VO video plane (定位到屏幕子矩形)         │ IPC(共享内存)
                       │ 视频层在"下"                         ▼
LVGL (fb0 / graphic plane, 在"上"):
   · 视频子矩形区域留透明 → 透出下方 VO 视频
   · 在对应屏幕坐标画检测框(LVGL 矢量对象) + 所有 UI 控件
VOP 硬件合成: video plane + graphic plane → LCD
```

- 优点：视频像素**完全不进 fb0/UI**，零拷贝，保住 20fps；cam 与 UI 解耦最干净。
- 风险 R1（**必须上板验证**）：需把 VO video plane z-order 调到 graphic **之下**，或让 fb0 graphic 带 alpha 让视频区透出，否则 LVGL 框被视频盖住（C5）。

### 方案 B（回退 · LVGL 最直观）

```
cam:  selfpath帧 ──► rknn ──► bbox 元数据 ──┐
      selfpath帧 ──► dma-buf 零拷贝交给 LVGL   │
                       │                        ▼
LVGL (fb0): 视频做成 lv_img 图像控件嵌布局, 框自然画在视频控件之上 + 其他 UI
```

- 优点：视频是 LVGL 原生图像控件，UI 开发最直观，无层序/alpha 烦恼。
- 代价：每帧需把视频 blit 进 fb0（软件路径开销，但仅限视频子窗，非全屏）。

> **决策建议**：优先方案 A；若上板调不出"视频在下、LVGL 在上"的层序/alpha，再退回 B。

---

## 5. 推荐落地的整体架构（方案 A 视角）

```
                    ┌──────────────────────────────────────────┐
  sensor ─────────►│  device-cam  (ISP 单实例拥有者, C+Rust)   │
                    │  ISP → VI                                │
                    │   · mainpath chn0 → VPSS → VENC → p2p   │──► 远程 viewer
                    │   · selfpath → VO video plane(子矩形定位) │──┐ 硬件层
                    │   · rknn_infer.c (独立线程, C++ rknn_api)│  │
                    │        └─ bbox 元数据 ──┐                 │  │
                    └─────────────────────────┼─────────────────┘  │
                                              ▼                     ▼
                                        [共享内存环形队列]      VOP 合成
                                              │                     │
                    ┌─────────────────────────┼─────────────────────┘
                    │  独立 UI 程序 (LVGL, C, 写 /dev/fb0)          │
                    │  · 读 bbox 元数据 → 画检测框(矢量, 对齐子矩形) │
                    │  · 画所有 UI 控件/菜单/状态                    │
                    └─────────────────────────────────────────────────┘
                                          ▼
                                        LCD 屏幕
```

---

## 6. 实施步骤（拍板后，C + A）

0. **（C）模块提取**：把 `rk_camera.c` 的 `lcd_vo_init/thread/deinit` 抽到 `lcd_preview.c` + `lcd_preview_start/stop` API；device-cam 改为链接该模块（行为不变）；新增 `lcd-preview` standalone 二进制（仅 VO video plane 本地预览，无 p2p / 无 LVGL）用于上板验证 R1。
1. **cam 接入 rknn**：新增 `rknn_infer.c`（或扩 `rk_camera.c`），用 `rknn_api` 加载 `luckfox_pico_yolov5` 的 model；在独立线程里对 selfpath 帧做 letterbox→推理→后处理，产出 bbox 列表。
2. **VO 视频层局部定位**：现有 `lcd_vo_init` 用 `SetLayerAttr`/`SetLayerSpliceMode` 把 video plane 定位到目标子矩形（"局部显示"）。
3. **bbox 元数据通道**：cam 侧把 box 映射回屏幕子矩形坐标，写入共享内存环形队列；提供 LVGL 侧 C 读取接口。
4. **LVGL 侧**：开一个定时器/任务读取 bbox 队列，在对应屏幕坐标用 LVGL 画检测框；视频子矩形区域保持透明透出。
5. **验证 R1**：上板确认 video plane 在 graphic 之下（或 fb0 alpha 透出）。

---

## 7. 上板验证清单（RISK）

- [ ] **R1**：VOP 层序/alpha 使"视频在下、LVGL 框在上"成立（决定 A 是否可行，否则转 B）。
- [ ] **R2**：cam 的 rknn 独立线程与编码管线并行时，CPU/NPU 负载与帧率是否达标。
- [ ] **R3**（若选 A 之前的旧顾虑）：device-cam 与独立 UI 并存时，ISP/VI 跨进程共享无冲突（C1/C2 已保证 cam 独占 ISP）。
- [ ] **R4**：bbox 坐标映射（模型空间→屏幕子矩形）对齐准确，LVGL 框与视频目标重合。

---

## 8. 决策清单（需要你拍板的）

| 编号 | 维度 | 问题 | 选项 | 当前建议 |
|------|------|------|------|----------|
| Q0 | 代码组织（维度一） | cam 侧 LCD/VO 是否抽模块 | C=独立 `lcd_preview` 模块 + API + standalone / 不抽 | **C（已定 D5）** |
| Q1 | 显示机制（维度二） | 显示方案 A 还是 B？ | A=VO video plane + LVGL 叠加（性能优）/ B=视频做 LVGL 图像控件（直观） | **A**，调不出层序再转 B |
| Q2 | rknn 位置 | cam / UI | cam / UI | **cam**（已定 D1） |
| Q3 | bbox 通道形式 | 共享内存环形队列 / UDS | 共享内存环形队列 / UDS | **共享内存环形队列**（已定 D4） |

> 维度一（Q0=C）已确定；维度二仅 **Q1（A/B）** 待你最终拍板即可开工。最终落地 = **C + A**。

---

## 9. 实施进度（已落地的代码）

> 决策定下后已开始写代码。以下为**已落地**部分；rknn / LVGL 两侧因依赖外部 SDK/仓库，留待下一步。

### 已完成（C + A 基础设施）

| 项 | 文件 | 说明 |
|----|------|------|
| C 模块抽取 | `device-cam/src/lcd_preview.c` + `lcd_preview.h` | 原 `rk_camera.c` 的 `lcd_vo_init/deinit/thread` 整体迁入；对外 `lcd_preview_start/stop/set_config/set_rect/is_enabled`。线程退出改模块私有 `g_lcd_quit`；`VI_DEV_ID` 用模块内 `LCD_VI_DEV_ID` 常量避免与 `rk_camera.c` 宏重复。 |
| C 链接不变 | `device-cam/build.rs` | 新增 `.file("src/lcd_preview.c")`，device-cam 行为零变化。 |
| C standalone | `device-cam/src/main_lcd_preview.c` + `Makefile.lcd_preview` | 仅本地 VO 预览二进制 `lcd-preview`，不依赖 p2p/LVGL；用于上板验证 R1（VOP 层序）。 |
| A 子矩形定位 | `lcd_preview.c` + `rk_camera.c`(`rk_camera_set_vo_rect`) + `rk_video_source.rs`(`with_lcd_rect`) | video plane 的 `layer.stDispRect`/`chn.stRect` 应用 `disp_x/disp_y`，可从 Rust 配置端到端指定"局部显示"位置。 |
| A→LVGL 通道 | `device-cam/src/bbox_shm.h` + `bbox_shm.c` | 共享内存环形队列（单生产者/单消费者，无锁原子同步）。坐标**已是屏幕子矩形空间**（cam 发布前映射），LVGL 直接画框。已纳入 `build.rs`。 |
| **rknn 模块** | `device-cam/src/rknn_infer.c` + `rknn_infer.h` | **与 `lcd_preview.c` 并列的独立 C 模块**，链接进 `device-cam`（= 加入 cam 生产路径）。`rk_camera.c` 新增 `rk_camera_enable_rknn(path)` + `deinit` 调 `rknn_infer_stop`；`rk_video_source.rs` 新增 `with_rknn(path)` + extern + spawn 接线；`build.rs` 加 `rknn_infer.c` + `RV1106_RKNN_INCLUDE` + 链接 `librknnmrt.so`。 |
| **rknn 复用 selfpath 帧源** | `lcd_preview.c`（重构） | `lcd_preview` 的 selfpath VI 通道 + 送帧泵改为**中性帧源**：引用计数 `lcd_preview_ensure_source/release_source`，LCD 显示与 rknn 推理任一需要即保持开启；泵每帧在 Release 前调用已注册的帧消费者回调（`rknn_infer` 在里面 µs 级 memcpy 像素到无锁 SPSC 环形队列）。**不再自开第二个 VI 通道**。 |
| **rknn 后处理（C 移植）** | `rknn_infer.c` | 对齐 luckfox `postprocess.cc`：`process_i8_rv1106`（RV1106 int8 affine 量化路径）+ `nms` + `quick_sort_indice_inverse` + `CalculateOverlap` + NV12→BGR + bilinear resize 640×640。预处理 = **stretch resize（非 letterbox）**，与例子 `cv::resize` 一致。 |

### 待做（依赖外部输入）

- **rknn_infer.c**（step 4，**落点已定**）：做成与 `lcd_preview.c` **并列的独立 C 模块** `rknn_infer.c`，链接进 `device-cam`（= 加入 cam 生产路径），**不塞进 `lcd-preview` standalone 二进制**。
  - **取帧 = 复用 LCD 的 selfpath 通道**（用户 2026-07-24 拍板，节约资源）：`lcd_preview` 已有的 chn1 是唯一帧源，`rknn_infer` 作为消费者挂上去，**不再自开第二个 VI selfpath 通道**（原"多 selfpath 通道可行性未知"风险点因此消除）。
  - **帧流**：泵线程 `GetChnFrame(chn1)` → ①VO SendFrame(显示) ②拷贝像素进 rknn 输入队列(nonblock, 满则丢) ③ReleaseChnFrame；rknn worker 线程 pop→NV12→BGR→bilinear stretch resize 640x640→RKNN 推理→NMS→坐标映射(640 空间→屏幕子矩形, 经 `lcd_preview_get_disp_rect`)→写 `bbox_shm`。**推理异步，绝不阻塞显示泵**。
  - **已实现并接线**：① 交叉编译链 `rknn_api.h`+`librknnmrt.so`（`build.rs` 已加 `rknn_infer.c` + `RV1106_RKNN_INCLUDE` + 链接 `rknnmrt`，env 可覆盖路径）；② **selfpath 通道开关 = "LCD 或 rknn 任一开即开"**（`lcd_preview.c` 已重构为中性帧源引用计数 `ensure_source/release_source`，用户 2026-07-24 拍板）；③ LVGL 消费端**用户定下一步再做**（仓库不在本 repo）。
- **LVGL 消费端**（step 6）：需 LVGL 程序仓库路径，我才能放 `bbox_shm.h` + 读取示例片段；该仓库不在本 repo。

> 注：以上 C/C 侧代码**尚未在 RV1106 目标上交叉编译验证**（本机无工具链），上板前需在 SDK 环境 `cargo build --features rv1106` 与 `make -f Makefile.lcd_preview` 验证。
