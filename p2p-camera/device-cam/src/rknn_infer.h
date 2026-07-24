// SPDX-License-Identifier: MIT
// rknn 推理模块 — 与 lcd_preview.c 平级的独立编译单元。
//
// 职责: 消费 lcd_preview 的 selfpath 帧 (复用 LCD 通道, 节约资源),
//        对每帧跑 YOLOv5 (RKNN) 推理, 把检测框 (已是屏幕子矩形坐标)
//        经 bbox_shm 共享内存发布给 LVGL UI。
//
// 设计要点:
//   - 推理线程 (rknn_worker) 与显示泵解耦: 泵回调只做 µs 级 memcpy 像素到
//     无锁 SPSC 环形队列; 真正 NV12->BGR->resize->RKNN 在 worker 异步跑, 不阻塞 LCD 泵。
//   - 坐标: 例子用 cv::resize 做 stretch (非 letterbox), 故模型 640 空间 ->
//     帧空间 按 (frame_w/640, frame_h/640) 分别缩放; 再叠加 video plane 偏移
//     (disp_x,disp_y) 得到屏幕坐标, 直接喂 bbox_shm (LVGL 无需再换算)。
//   - RV1106 走 int8 affine 量化路径 (process_i8_rv1106), 零拷贝 io_mem。
//
// 头文件依赖 rknn_api.h (由 RKNN SDK include 路径提供, 见 build.rs 的
//   RNNN_SDK_INCLUDE)。整个 crate 只在 RV1106 SDK 环境下编译。

#ifndef RKNN_INFER_H
#define RKNN_INFER_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// 初始化 RKNN 模型 (加载 .rknn)。返回 0 成功, -1 失败。
int rknn_infer_init(const char *model_path);

// 启动推理: 注册为 lcd_preview 帧消费者 + 开启中性帧源 + 起 worker 线程。
//   返回 0 成功, -1 失败。
int rknn_infer_start(void);

// 停止推理: 注销消费者 + 释放帧源引用 + join worker + 关 bbox_shm。
void rknn_infer_stop(void);

#ifdef __cplusplus
}
#endif

#endif // RKNN_INFER_H
