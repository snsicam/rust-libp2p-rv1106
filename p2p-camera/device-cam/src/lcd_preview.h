// SPDX-License-Identifier: MIT
// LCD 预览模块 — 从 rk_camera.c 抽取出的独立编译单元。
//
// 职责: 把 ISP selfpath 的原始帧经 MPP VO 模块送到 VOP video plane (硬件 CSC + 缩放,
//        零 CPU 转换, 不碰 RGA 单例)。
// 典型用途:
//   1) device-cam 链接本模块, 行为与原内联代码完全一致 (零风险);
//   2) 由 src/main_lcd_preview.c 编出 standalone `lcd-preview` 二进制
//      (离线取景器 / 上板验证 VOP 层序用), 不依赖 p2p / LVGL。
//
// 帧源模型 (2026-07-24 重构):
//   selfpath VI 通道 + 送帧泵线程是"中性帧源", 由引用计数管理:
//     - LCD 显示 (lcd_preview_start) 需要它;
//     - rknn 推理 (rknn_infer.c) 也需要它 (复用同一通道, 节约资源)。
//   任一需要即 lcd_preview_ensure_source() 开启; 双方都释放才真正关闭。
//   泵每取到一帧, 在 Release 之前调用已注册的帧消费者回调 (rknn 在里面拷贝像素)。

#ifndef LCD_PREVIEW_H
#define LCD_PREVIEW_H

#ifdef __cplusplus
extern "C" {
#endif

// 帧消费者回调类型 (rknn 等注册)。
//   frame: 本帧 selfpath 原始帧 (NV12), 回调返回前不可长期持有 — 泵随后会 Release。
//   ctx  : 注册时传入的上下文指针。
//   设计约束: 回调内只能做 µs 级操作 (如 memcpy 像素到自己的队列),
//             绝不能在此跑推理等重活 (否则阻塞显示泵)。
#include "rk_mpi_vi.h"
typedef void (*lcd_preview_frame_cb)(const VIDEO_FRAME_INFO_S *frame, void *ctx);

// 配置 LCD 预览参数 (必须在 lcd_preview_start 之前调用)
//   w, h : 预览分辨率
//   fps   : 显示帧率
// （sensor_fps 由 rk_camera.c 的 VI 主通道初始化时通过
//   lcd_preview_set_sensor_fps 写入, 模块内部默认 30, 与历史行为一致）
void lcd_preview_set_config(int w, int h, int fps);

// 设置/读取 sensor 原生帧率。
//   rk_camera.c 的 VI 主通道初始化时写入 (vpss/venc 动态改码率时也读取);
//   模块内 selfpath VI 通道用其设源帧率 (stFrameRate.s32SrcFrameRate)。
void lcd_preview_set_sensor_fps(int fps);
int  lcd_preview_get_sensor_fps(void);

// 设置 video plane 在屏幕上的子矩形位置 (局部显示用)
//   x, y : 左上角坐标 (默认 0,0 = 全屏)
void lcd_preview_set_rect(int x, int y);

// 是否启用 LCD 预览 (set_config 后返回 1)
int lcd_preview_is_enabled(void);

// 注册帧消费者 (rknn 推理用)。cb=NULL 取消注册。
void lcd_preview_register_frame_consumer(lcd_preview_frame_cb cb, void *ctx);

// 返回 video plane 在屏幕上的子矩形 (供 rknn 把模型坐标映射回屏幕像素)
//   x,y : 左上角; w,h : 宽高 (等于 selfpath 帧分辨率)
void lcd_preview_get_disp_rect(int *x, int *y, int *w, int *h);

// 确保 selfpath 帧源已开 (LCD 显示 或 rknn 推理 任一需要)。引用计数, 幂等。
//   首次调用: 创建 selfpath VI 通道 + 启动送帧泵线程; 返回 0。
int  lcd_preview_ensure_source(void);
// 释放对帧源的引用。引用归零时停止泵 + 关闭 selfpath VI 通道。
void lcd_preview_release_source(void);

// 启动 LCD 显示: 确保帧源已开 + 初始化 VO video plane + 送帧线程
//   返回 0 成功, -1 失败 (LCD 不启用, 模块内部状态已回滚)
int lcd_preview_start(void);

// 停止 LCD 显示: 释放 LCD 对帧源的引用 + 反初始化 VO
//   (若 rknn 仍持有引用, 帧源/泵继续为 rknn 服务)
void lcd_preview_stop(void);

#ifdef __cplusplus
}
#endif

#endif // LCD_PREVIEW_H
