// SPDX-License-Identifier: MIT
// cam → LVGL 的 bbox 元数据通道 (共享内存环形队列, 零拷贝)。
//
// 设计: 单生产者 (cam/rknn 线程) + 单消费者 (LVGL UI 线程)。
//   - 坐标已是**屏幕子矩形空间** (cam 在发布前把模型空间 box 映射回
//     VO video plane 的子矩形屏幕坐标), LVGL 直接用这些坐标画框, 无需再换算。
//   - 多个 slot 双缓冲避免读写撕裂; 消费者取"最近一次写完"的快照。
//   - 用原子序号做无锁同步 (C11 atomic), 不依赖 pthread 互斥, 跨进程安全。
//
// 用法:
//   producer (cam):  bbox_shm_t *s = bbox_shm_open("/bbox_cam_lvgl", 1);  // create
//                     bbox_shm_publish(s, boxes, n);
//   consumer (LVGL): bbox_shm_t *s = bbox_shm_open("/bbox_cam_lvgl", 0);  // attach
//                     bbox_snapshot_t snap; if (bbox_shm_acquire(s, &snap)) { ...画框... }

#ifndef BBOX_SHM_H
#define BBOX_SHM_H

#include <stdint.h>

#ifdef __cplusplus
extern "C" {
#endif

// 单个检测框 (屏幕子矩形坐标, 单位 = LCD 像素)
typedef struct {
    int32_t x;       // 左上角 X (相对 LCD 屏幕原点)
    int32_t y;       // 左上角 Y
    int32_t w;       // 宽
    int32_t h;       // 高
    int32_t cls;      // 类别 id (COCO 80 类)
    float   score;    // 置信度 0..1
} bbox_t;

// 一帧检测快照 (LVGL 每次取一整帧)
typedef struct {
    uint32_t seq;            // 生产者写入序号 (单调递增)
    int32_t  count;          // 本帧有效框数
    int32_t  disp_w;         // 对应 video plane 子矩形宽 (供 LVGL 校验)
    int32_t  disp_h;         // 对应 video plane 子矩形高
    bbox_t  boxes[32];      // 最多 32 个框
} bbox_snapshot_t;

// 共享内存布局 (放在 shm 中)
typedef struct {
    volatile uint32_t write_idx;   // 下一个写入 slot (生产者原子自增)
    volatile uint32_t seq;         // 全局序号 (每发布一帧 +1)
    int32_t  slot_count;          // = BBOX_SHM_SLOTS
    int32_t  max_boxes;           // = 32
    bbox_snapshot_t slots[4];     // 4 个双缓冲 slot
} bbox_shm_t;

#define BBOX_SHM_SLOTS 4
#define BBOX_SHM_MAX_BOXES 32

// 打开 (或创建) 共享内存。
//   name   : 共享内存对象名 (建议 "/bbox_cam_lvgl")
//   create : 1=生产者创建并初始化; 0=消费者仅 attach
// 返回映射地址, 失败返回 NULL。
bbox_shm_t *bbox_shm_open(const char *name, int create);

// 关闭 (生产者应传 free=1 以 unlink 共享内存对象)
void bbox_shm_close(bbox_shm_t *s, int free);

// 生产者: 发布一帧检测结果 (盒子坐标须已是屏幕子矩形空间)
void bbox_shm_publish(bbox_shm_t *s, const bbox_t *boxes, int n);

// 消费者: 取最近一次写完的快照。返回 1 成功 (snap 有效), 0 无新数据。
int bbox_shm_acquire(bbox_shm_t *s, bbox_snapshot_t *snap);

#ifdef __cplusplus
}
#endif

#endif // BBOX_SHM_H
