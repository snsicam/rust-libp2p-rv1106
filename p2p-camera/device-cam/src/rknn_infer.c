// SPDX-License-Identifier: MIT
// rknn 推理模块实现 — YOLOv5 (RKNN) 目标检测, 输出框经 bbox_shm 给 LVGL。
//
// 复用 lcd_preview 的 selfpath 帧源 (不另开 VI 通道): 显示泵每帧取到后
// 在 Release 之前调用本模块的 rknn_frame_cb, 我们把 NV12 像素 memcpy 到一个
// 无锁 SPSC 环形队列 (µs 级, 不阻塞显示泵); 真正的
// NV12->BGR -> bilinear resize(640x640) -> rknn_run -> 后处理 在 rknn_worker 异步跑。
//
// 坐标系 (对齐 luckfox_pico_yolov5 例子的 mapCoordinates):
//   模型输入是 stretch (非 letterbox) resize 到 640x640, 与例子一致。
//   模型框 (640 空间) -> 帧空间: x*frame_w/640, y*frame_h/640 (分别宽高缩放)
//   -> 屏幕空间: 再叠加 video plane 子矩形偏移 (disp_x, disp_y)。
//   disp_x/y/w/h 由 lcd_preview_get_disp_rect 取得 (== selfpath 帧分辨率 + 显示位置)。

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <math.h>
#include <pthread.h>
#include <unistd.h>
#include <sys/time.h>

#include "rknn_api.h"          // RKNN SDK
#include "lcd_preview.h"        // 帧源 (selfpath) + 消费者注册 + 屏幕子矩形
#include "bbox_shm.h"           // cam<->LVGL 检测框通道
#include "rk_mpi_mb.h"          // RK_MPI_MB_Handle2VirAddr: DMABUF 帧取 CPU 虚拟地址
#include "rk_mpi_sys.h"         // RK_MPI_SYS_MmzAlloc: 分配"自有"显示缓冲 (消除与 VI 环形缓冲竞争)

// ---- 后处理结果结构 (对齐例子 postprocess.h) ----
#define OBJ_NUMB_MAX_SIZE  128
typedef struct { int left, top, right, bottom; } image_rect_t;
typedef struct {
    image_rect_t box;
    float prop;
    int cls_id;
} object_detect_result;
typedef struct {
    int id;
    int count;
    object_detect_result results[OBJ_NUMB_MAX_SIZE];
} object_detect_result_list;

// ---------------- 模型 / 后处理常量 (对齐 luckfox 例子) ----------------
#define OBJ_NAME_MAX_SIZE 64
#define OBJ_CLASS_NUM 80
#define NMS_THRESH 0.45f
#define BOX_THRESH 0.25f
#define PROP_BOX_SIZE (5 + OBJ_CLASS_NUM)
#define RKNN_MODEL_W 640
#define RKNN_MODEL_H 640
#define BBOX_SHM_NAME "/bbox_cam_lvgl"

static const int anchor[3][6] = {
    {10, 13, 16, 30, 33, 23},
    {30, 61, 62, 45, 59, 119},
    {116, 90, 156, 198, 373, 326},
};

// ---------------- rknn 上下文 (C 版, RV1106 零拷贝布局) ----------------
typedef struct {
    rknn_context rknn_ctx;
    rknn_input_output_num io_num;
    rknn_tensor_attr *input_attrs;
    rknn_tensor_attr *output_attrs;
    rknn_tensor_mem *input_mems[1];
    rknn_tensor_mem *output_mems[3];
    int model_channel;
    int model_width;
    int model_height;
    int is_quant;
} rknn_app_context_t;

// ---------------- 无锁 SPSC 帧环形队列 ----------------
// 单生产者 (显示泵回调) / 单消费者 (rknn_worker)。
// prod_idx/cons_idx 用 volatile + 单生产者/消费者自然有序; 队列满时生产者直接丢帧。
#define FRAME_SLOTS 4
typedef struct {
    uint8_t *nv12;     // 预分配, 大小 = stride*height*3/2
    int      cap;      // 缓冲容量 (字节)
    int      w, h;     // 帧宽高
    int      stride;   // NV12 行步长 (u32VirWidth)
} frame_slot_t;

static frame_slot_t g_slots[FRAME_SLOTS];
static volatile size_t g_prod_idx = 0;
static volatile size_t g_cons_idx = 0;

// ---------------- 模块全局 ----------------
static rknn_app_context_t g_app;
static bbox_shm_t *g_shm = NULL;
static pthread_t g_worker = 0;
static volatile int g_quit = 0;
static int g_running = 0;

// 推理预处理复用缓冲 (避免每帧 malloc 1.5MB)
static uint8_t *g_bgr_buf = NULL;
static size_t   g_bgr_cap = 0;

// 自有显示缓冲池: 回调把"带框副本"画在这批 MB 上送 VO。
// 关键: 不画在 VI 环形缓冲上 —— 否则下一帧 VI 捕获会覆盖框像素(VO 还没扫完),
// 造成框闪烁; 自有缓冲 VO 显示完才释放引用, 我们用 round-robin 复用(安全)。
#define DISP_POOL 3
static MB_BLK    g_disp_mb[DISP_POOL];
static uint8_t  *g_disp_vaddr[DISP_POOL];
static long long g_disp_sent[DISP_POOL];
static int       g_disp_w = 0, g_disp_h = 0, g_disp_stride = 0;

// ===================================================================
//  数学小工具 (对齐例子 postprocess.cc)
// ===================================================================
static inline int clamp(float val, int min, int max) {
    return val > min ? (val < max ? (int)val : max) : min;
}
static inline float deqnt_affine_to_f32(int8_t qnt, int32_t zp, float scale) {
    return ((float)qnt - (float)zp) * scale;
}
static inline int8_t qnt_f32_to_affine(float f32, int32_t zp, float scale) {
    float dst = f32 / scale + zp;
    int v = (int)dst;
    return (int8_t)(v < -128 ? -128 : (v > 127 ? 127 : v));
}
static float CalculateOverlap(float xmin0, float ymin0, float xmax0, float ymax0,
                            float xmin1, float ymin1, float xmax1, float ymax1) {
    float w = fmaxf(0.f, fminf(xmax0, xmax1) - fmaxf(xmin0, xmin1) + 1.f);
    float h = fmaxf(0.f, fminf(ymax0, ymax1) - fmaxf(ymin0, ymin1) + 1.f);
    float i = w * h;
    float u = (xmax0 - xmin0 + 1.f) * (ymax0 - ymin0 + 1.f)
            + (xmax1 - xmin1 + 1.f) * (ymax1 - ymin1 + 1.f) - i;
    return u <= 0.f ? 0.f : (i / u);
}
// 降序快排索引 (对齐 quick_sort_indice_inverse)
static int quick_sort_indice_inverse(float *input, int left, int right, int *indices) {
    float key;
    int key_index;
    int low = left;
    int high = right;
    if (left < right) {
        key_index = indices[left];
        key = input[left];
        while (low < high) {
            while (low < high && input[high] <= key) high--;
            input[low] = input[high];
            indices[low] = indices[high];
            while (low < high && input[low] >= key) low++;
            input[high] = input[low];
            indices[high] = indices[low];
        }
        input[low] = key;
        indices[low] = key_index;
        quick_sort_indice_inverse(input, left, low - 1, indices);
        quick_sort_indice_inverse(input, low + 1, right, indices);
    }
    return low;
}
// 单类 NMS (过滤 order[j]==-1)
static void nms(int validCount, float *loc, int *classIds, int *order,
                int filterId, float threshold) {
    for (int i = 0; i < validCount; ++i) {
        if (order[i] == -1 || classIds[i] != filterId) continue;
        int n = order[i];
        for (int j = i + 1; j < validCount; ++j) {
            int m = order[j];
            if (m == -1 || classIds[i] != filterId) continue;
            float xmin0 = loc[n * 4 + 0], ymin0 = loc[n * 4 + 1];
            float xmax0 = xmin0 + loc[n * 4 + 2], ymax0 = ymin0 + loc[n * 4 + 3];
            float xmin1 = loc[m * 4 + 0], ymin1 = loc[m * 4 + 1];
            float xmax1 = xmin1 + loc[m * 4 + 2], ymax1 = ymin1 + loc[m * 4 + 3];
            float iou = CalculateOverlap(xmin0, ymin0, xmax0, ymax0,
                                       xmin1, ymin1, xmax1, ymax1);
            if (iou > threshold) order[j] = -1;
        }
    }
}

#define MAX_PRE_NMS 1024
static float g_boxes[MAX_PRE_NMS * 4];
static float g_scores[MAX_PRE_NMS];
static int   g_classId[MAX_PRE_NMS];
static int   g_order[MAX_PRE_NMS];

// RV1106 uint8 量化解码 (对齐 process_i8_rv1106)
static int process_i8_rv1106(int8_t *input, const int *anc, int grid_h, int grid_w,
                             int height, int width, int stride,
                             int *validCount, float threshold, int32_t zp, float scale) {
    (void)height; (void)width;
    int valid = *validCount;
    int8_t thres_i8 = qnt_f32_to_affine(threshold, zp, scale);
    int anchor_per_branch = 3;
    int align_c = PROP_BOX_SIZE * anchor_per_branch;
    for (int h = 0; h < grid_h; h++) {
        for (int w = 0; w < grid_w; w++) {
            for (int a = 0; a < anchor_per_branch; a++) {
                int hw_offset = h * grid_w * align_c + w * align_c + a * PROP_BOX_SIZE;
                int8_t *p = input + hw_offset;
                int8_t box_conf = p[4];
                if (box_conf >= thres_i8) {
                    int8_t maxClassProbs = p[5];
                    int maxClassId = 0;
                    for (int k = 1; k < OBJ_CLASS_NUM; ++k) {
                        int8_t prob = p[5 + k];
                        if (prob > maxClassProbs) { maxClassId = k; maxClassProbs = prob; }
                    }
                    float box_conf_f32 = deqnt_affine_to_f32(box_conf, zp, scale);
                    float class_prob_f32 = deqnt_affine_to_f32(maxClassProbs, zp, scale);
                    float limit = box_conf_f32 * class_prob_f32;
                    if (limit > threshold && valid < MAX_PRE_NMS) {
                        float bx = deqnt_affine_to_f32(p[0], zp, scale) * 2.0f - 0.5f;
                        float by = deqnt_affine_to_f32(p[1], zp, scale) * 2.0f - 0.5f;
                        float bw = deqnt_affine_to_f32(p[2], zp, scale) * 2.0f;
                        float bh = deqnt_affine_to_f32(p[3], zp, scale) * 2.0f;
                        bw *= bw; bh *= bh;
                        bx = (bx + w) * (float)stride;
                        by = (by + h) * (float)stride;
                        bw *= (float)anc[a * 2];
                        bh *= (float)anc[a * 2 + 1];
                        bx -= bw / 2.0f;
                        by -= bh / 2.0f;
                        g_boxes[valid * 4 + 0] = bx;
                        g_boxes[valid * 4 + 1] = by;
                        g_boxes[valid * 4 + 2] = bw;
                        g_boxes[valid * 4 + 3] = bh;
                        g_scores[valid] = limit;
                        g_classId[valid] = maxClassId;
                        valid++;
                    }
                }
            }
        }
    }
    *validCount = valid;
    return valid;
}

// 后处理 (对齐 post_process, RV1106 i8 路径)
static int post_process(rknn_app_context_t *app_ctx, rknn_tensor_mem **outputs,
                      float conf_threshold, float nms_threshold,
                      object_detect_result_list *od) {
    memset(od, 0, sizeof(*od));
    int validCount = 0;
    int model_in_w = app_ctx->model_width;
    int model_in_h = app_ctx->model_height;
    int nout = app_ctx->io_num.n_output;
    if (nout > 3) nout = 3;  // YOLOv5 固定 3 输出
    for (int i = 0; i < nout; i++) {
        int grid_h = app_ctx->output_attrs[i].dims[2];
        int grid_w = app_ctx->output_attrs[i].dims[1];
        int stride = model_in_h / grid_h;
        if (app_ctx->is_quant) {
            validCount += process_i8_rv1106(
                (int8_t *)outputs[i]->virt_addr, anchor[i],
                grid_h, grid_w, model_in_h, model_in_w, stride,
                &validCount, conf_threshold,
                app_ctx->output_attrs[i].zp, app_ctx->output_attrs[i].scale);
        }
    }
    if (validCount <= 0) return 0;

    for (int i = 0; i < validCount; i++) g_order[i] = i;
    quick_sort_indice_inverse(g_scores, 0, validCount - 1, g_order);

    // 逐类 NMS (对齐例子的 std::set<classId> 遍历)
    for (int c = 0; c < OBJ_CLASS_NUM; c++) {
        int has = 0;
        for (int i = 0; i < validCount; i++) if (g_classId[i] == c) { has = 1; break; }
        if (has) nms(validCount, g_boxes, g_classId, g_order, c, nms_threshold);
    }

    int last = 0;
    for (int i = 0; i < validCount; ++i) {
        if (g_order[i] == -1 || last >= OBJ_NUMB_MAX_SIZE) continue;
        int n = g_order[i];
        float x1 = g_boxes[n * 4 + 0];
        float y1 = g_boxes[n * 4 + 1];
        float x2 = x1 + g_boxes[n * 4 + 2];
        float y2 = y1 + g_boxes[n * 4 + 3];
        od->results[last].box.left   = clamp(x1, 0, model_in_w);
        od->results[last].box.top    = clamp(y1, 0, model_in_h);
        od->results[last].box.right  = clamp(x2, 0, model_in_w);
        od->results[last].box.bottom = clamp(y2, 0, model_in_h);
        od->results[last].prop    = g_scores[n];
        od->results[last].cls_id  = g_classId[n];
        last++;
    }
    od->count = last;
    return 0;
}

// ===================================================================
//  颜色空间转换
// ===================================================================
// NV12 (YUV420SP) -> BGR (逐行按 stride 拷贝, BT.601)
static void nv12_to_bgr(const uint8_t *yuv, int w, int h, int stride, uint8_t *bgr) {
    const uint8_t *yp = yuv;
    const uint8_t *uvp = yuv + (size_t)stride * h;
    for (int y = 0; y < h; y++) {
        const uint8_t *yrow = yp + (size_t)y * stride;
        const uint8_t *uvrow = uvp + (size_t)(y / 2) * stride;
        uint8_t *brow = bgr + (size_t)y * w * 3;
        for (int x = 0; x < w; x++) {
            int Y = yrow[x];
            int U = uvrow[(x / 2) * 2];
            int V = uvrow[(x / 2) * 2 + 1];
            int R = Y + (int)(1.402f   * (V - 128));
            int G = Y - (int)(0.344136f * (U - 128) + 0.714136f * (V - 128));
            int B = Y + (int)(1.772f   * (U - 128));
            brow[x * 3 + 0] = (uint8_t)(B < 0 ? 0 : (B > 255 ? 255 : B));
            brow[x * 3 + 1] = (uint8_t)(G < 0 ? 0 : (G > 255 ? 255 : G));
            brow[x * 3 + 2] = (uint8_t)(R < 0 ? 0 : (R > 255 ? 255 : R));
        }
    }
}
// BGR 双线性缩放到 dw x dh (写 NHWC BGR 连续缓冲, 喂 rknn input_mem)
static void bilinear_bgr_resize(const uint8_t *src, int sw, int sh,
                               uint8_t *dst, int dw, int dh) {
    float fx = (float)(sw - 1) / (dw - 1);
    float fy = (float)(sh - 1) / (dh - 1);
    for (int y = 0; y < dh; y++) {
        float fyv = fy * y;
        int y0 = (int)fyv;
        int y1 = y0 + 1 < sh ? y0 + 1 : y0;
        float ty = fyv - y0;
        for (int x = 0; x < dw; x++) {
            float fxv = fx * x;
            int x0 = (int)fxv;
            int x1 = x0 + 1 < sw ? x0 + 1 : x0;
            float tx = fxv - x0;
            const uint8_t *s00 = src + ((size_t)y0 * sw + x0) * 3;
            const uint8_t *s10 = src + ((size_t)y0 * sw + x1) * 3;
            const uint8_t *s01 = src + ((size_t)y1 * sw + x0) * 3;
            const uint8_t *s11 = src + ((size_t)y1 * sw + x1) * 3;
            uint8_t *d = dst + ((size_t)y * dw + x) * 3;
            for (int c = 0; c < 3; c++) {
                float v = (1 - tx) * (1 - ty) * s00[c]
                        + tx * (1 - ty) * s10[c]
                        + (1 - tx) * ty * s01[c]
                        + tx * ty * s11[c];
                d[c] = (uint8_t)(v + 0.5f);
            }
        }
    }
}

// ===================================================================
//  显示泵消费者回调 (µs 级: 仅拷贝 NV12 像素到队列, 不推理)
// ===================================================================
static volatile long g_cb_calls = 0;    // 回调被泵调用次数 (诊断)
static volatile long g_cb_queued = 0;   // 成功入队帧数 (诊断)
static volatile long g_overlay_frames = 0; // 已画框的显示帧数 (诊断)

// ---- 逐物体跟踪缓存 + 时间平滑 (消除静止物体框闪烁 / 幽灵框累积) ----
// g_track: 每个"物体"一条记录, 位置随时间平滑收敛, 各自有 last(最近一次匹配时间)。
// 仅在 (now - last) > BOX_PERSIST_MS 时才清除单条记录(物体真正离开), 故静止物体框稳。
// 匹配按 类别+IoU 贪心指派, 避免同一物体产生多个幽灵框(旧版 cache 会无限累积→全屏乱闪)。
#ifndef MAX
#define MAX(a,b) ((a)>(b)?(a):(b))
#endif
#ifndef MIN
#define MIN(a,b) ((a)<(b)?(a):(b))
#endif
typedef struct { bbox_t b; long long last; int used; } track_t;
static pthread_mutex_t g_track_lock = PTHREAD_MUTEX_INITIALIZER;
static track_t g_track[BBOX_SHM_MAX_BOXES];
static int     g_track_n = 0;
#define BOX_PERSIST_MS 1500                // 超过该时长未匹配任何检出 -> 清除该物体
#define BOX_SMOOTH_W   0.3f                // 新检测融合权重(越小越稳)
#define BOX_MATCH_IOU  0.4f                // 同类别匹配 IoU 阈值
static long long now_ms(void) {
    struct timeval tv; gettimeofday(&tv, NULL);
    return (long long)tv.tv_sec * 1000 + tv.tv_usec / 1000;
}
static float box_iou(const bbox_t *a, const bbox_t *b) {
    int x1 = MAX(a->x, b->x), y1 = MAX(a->y, b->y);
    int x2 = MIN(a->x + a->w, b->x + b->w), y2 = MIN(a->y + a->h, b->y + b->h);
    int iw = x2 - x1, ih = y2 - y1;
    if (iw <= 0 || ih <= 0) return 0.f;
    int inter = iw * ih;
    int ua = a->w * a->h + b->w * b->h - inter;
    return ua > 0 ? (float)inter / ua : 0.f;
}
// worker 每检出一次调用: 对检出框(按置信度降序)贪心匹配缓存里同类别 IoU 最佳者做平滑;
// 未匹配且未满则新增; 过期(>BOX_PERSIST_MS 无匹配)的记录在此删除。
static void update_box_cache(const bbox_t *boxes, int n) {
    long long t = now_ms();
    pthread_mutex_lock(&g_track_lock);
    // 1) 删除过期记录
    for (int i = 0; i < g_track_n; ) {
        if (t - g_track[i].last > BOX_PERSIST_MS) {
            g_track[i] = g_track[g_track_n - 1];
            g_track_n--;
        } else i++;
    }
    if (n <= 0) { pthread_mutex_unlock(&g_track_lock); return; }
    if (n > BBOX_SHM_MAX_BOXES) n = BBOX_SHM_MAX_BOXES;
    // 2) 按 score 降序排列检出索引
    int ord[BBOX_SHM_MAX_BOXES];
    for (int i = 0; i < n; i++) ord[i] = i;
    for (int i = 1; i < n; i++) {
        int k = ord[i], j = i - 1;
        while (j >= 0 && boxes[ord[j]].score < boxes[k].score) {
            ord[j + 1] = ord[j]; j--;
        }
        ord[j + 1] = k;
    }
    for (int i = 0; i < g_track_n; i++) g_track[i].used = 0;
    // 3) 贪心指派
    for (int oi = 0; oi < n; oi++) {
        const bbox_t *nb = &boxes[ord[oi]];
        int mi = -1; float best = BOX_MATCH_IOU;
        for (int j = 0; j < g_track_n; j++) {
            if (g_track[j].used || g_track[j].b.cls != nb->cls) continue;
            float v = box_iou(&g_track[j].b, nb);
            if (v > best) { best = v; mi = j; }
        }
        if (mi >= 0) {
            bbox_t *c = &g_track[mi].b;
            c->x     = (int32_t)(c->x     * (1 - BOX_SMOOTH_W) + nb->x     * BOX_SMOOTH_W);
            c->y     = (int32_t)(c->y     * (1 - BOX_SMOOTH_W) + nb->y     * BOX_SMOOTH_W);
            c->w     = (int32_t)(c->w     * (1 - BOX_SMOOTH_W) + nb->w     * BOX_SMOOTH_W);
            c->h     = (int32_t)(c->h     * (1 - BOX_SMOOTH_W) + nb->h     * BOX_SMOOTH_W);
            c->score = nb->score;
            g_track[mi].last = t;
            g_track[mi].used = 1;
        } else if (g_track_n < BBOX_SHM_MAX_BOXES) {
            g_track[g_track_n].b = *nb;
            g_track[g_track_n].last = t;
            g_track[g_track_n].used = 1;
            g_track_n++;
        }
    }
    pthread_mutex_unlock(&g_track_lock);
}

// ---- 在 NV12 帧上画检测框 + 类别标签 (对齐例子 cv::rectangle + cv::putText) ----
// 把框画进我们"自有"的显示缓冲 NV12 像素(由泵送 VO), 不依赖 fb0 图层, 避开 VOP 层序/alpha。
#define BOX_THICK 3

// COCO 80 类名 (与例子 coco_cls_to_name 一致)
static const char *g_coco_names[OBJ_CLASS_NUM] = {
    "person","bicycle","car","motorcycle","airplane","bus","train","truck","boat",
    "traffic light","fire hydrant","stop sign","parking meter","bench","bird","cat",
    "dog","horse","sheep","cow","elephant","bear","zebra","giraffe","backpack",
    "umbrella","handbag","tie","suitcase","frisbee","skis","snowboard","sports ball",
    "kite","baseball bat","baseball glove","skateboard","surfboard","tennis racket",
    "bottle","wine glass","cup","fork","knife","spoon","bowl","banana","apple",
    "sandwich","orange","broccoli","carrot","hot dog","pizza","donut","cake","chair",
    "couch","potted plant","bed","dining table","toilet","tv","laptop","mouse","remote",
    "keyboard","cell phone","microwave","oven","toaster","sink","refrigerator","book",
    "clock","vase","scissors","teddy bear","hair drier","toothbrush",
};
static const char *cls_name(int cls) {
    if (cls < 0 || cls >= OBJ_CLASS_NUM) return "obj";
    return g_coco_names[cls];
}

// 调色板 (Y,U,V), 按 COCO 类别取模上色
static const uint8_t g_box_pal[][3] = {
    {255,128,128}, // 0 白
    { 76, 85,255}, // 1 红
    {150, 46, 21}, // 2 绿
    { 29,255,128}, // 3 蓝
    {226,  3,149}, // 4 黄
    {179,174, 21}, // 5 青
    {105,213,255}, // 6 品红
    {173, 30,187}, // 7 橙
    { 76, 85,255}, // 8
    {150, 46, 21}, // 9
};

// ---- 5x7 点阵字体 (a-z, 0-9, 空格, '.', '%'), 每字形 7 行 x 5 列, 行字节 bit4..0 = 列0..4 ----
static const uint8_t g_font[39][7] = {
    {0x00,0x00,0x00,0x00,0x00,0x00,0x00}, // 0 space
    {0x0E,0x11,0x11,0x1F,0x11,0x11,0x11}, // 1 a
    {0x1E,0x11,0x11,0x1E,0x11,0x11,0x1E}, // 2 b
    {0x0E,0x11,0x10,0x10,0x10,0x11,0x0E}, // 3 c
    {0x0F,0x11,0x11,0x11,0x11,0x11,0x0F}, // 4 d
    {0x0E,0x11,0x11,0x1E,0x10,0x11,0x0E}, // 5 e
    {0x0E,0x08,0x08,0x0E,0x08,0x08,0x08}, // 6 f
    {0x0E,0x11,0x11,0x1F,0x11,0x11,0x17}, // 7 g
    {0x10,0x10,0x10,0x1E,0x11,0x11,0x11}, // 8 h
    {0x04,0x00,0x04,0x04,0x04,0x04,0x04}, // 9 i
    {0x02,0x00,0x02,0x02,0x02,0x12,0x0C}, // 10 j
    {0x10,0x10,0x14,0x18,0x14,0x12,0x11}, // 11 k
    {0x06,0x04,0x04,0x04,0x04,0x04,0x0E}, // 12 l
    {0x11,0x1B,0x1F,0x1B,0x1B,0x1B,0x1B}, // 13 m
    {0x11,0x19,0x15,0x15,0x15,0x15,0x15}, // 14 n
    {0x0E,0x11,0x11,0x11,0x11,0x11,0x0E}, // 15 o
    {0x1E,0x11,0x11,0x1E,0x10,0x10,0x10}, // 16 p
    {0x0F,0x11,0x11,0x11,0x15,0x13,0x0D}, // 17 q
    {0x11,0x11,0x14,0x18,0x14,0x11,0x11}, // 18 r
    {0x0E,0x11,0x10,0x0E,0x01,0x11,0x0E}, // 19 s
    {0x04,0x04,0x04,0x0E,0x04,0x04,0x04}, // 20 t
    {0x11,0x11,0x11,0x11,0x11,0x11,0x0E}, // 21 u
    {0x11,0x11,0x11,0x11,0x0A,0x0A,0x04}, // 22 v
    {0x1B,0x1B,0x1B,0x1B,0x1B,0x15,0x15}, // 23 w
    {0x11,0x11,0x0A,0x04,0x0A,0x11,0x11}, // 24 x
    {0x11,0x11,0x0A,0x04,0x04,0x04,0x04}, // 25 y
    {0x1F,0x01,0x02,0x04,0x08,0x10,0x1F}, // 26 z
    {0x0E,0x11,0x13,0x15,0x19,0x11,0x0E}, // 27 0
    {0x04,0x0C,0x04,0x04,0x04,0x04,0x0E}, // 28 1
    {0x0E,0x11,0x01,0x02,0x04,0x08,0x1F}, // 29 2
    {0x1F,0x10,0x1E,0x10,0x01,0x11,0x0E}, // 30 3
    {0x10,0x18,0x14,0x12,0x1F,0x10,0x10}, // 31 4
    {0x1F,0x11,0x1E,0x01,0x01,0x11,0x0E}, // 32 5
    {0x06,0x08,0x10,0x1E,0x11,0x11,0x0E}, // 33 6
    {0x1F,0x01,0x02,0x04,0x08,0x08,0x08}, // 34 7
    {0x0E,0x11,0x11,0x0E,0x11,0x11,0x0E}, // 35 8
    {0x0E,0x11,0x11,0x0F,0x01,0x02,0x0C}, // 36 9
    {0x00,0x00,0x00,0x00,0x00,0x06,0x06}, // 37 .
    {0x15,0x15,0x04,0x08,0x13,0x15,0x15}, // 38 %
};
static int font_idx(char c) {
    if (c == ' ') return 0;
    if (c >= 'a' && c <= 'z') return 1 + (c - 'a');
    if (c >= '0' && c <= '9') return 27 + (c - '0');
    if (c == '.') return 37;
    if (c == '%') return 38;
    return -1; // 不支持字符 -> 跳过
}

static inline void nv12_set_px(uint8_t *base, int stride, int h,
                               int px, int py, uint8_t Y, uint8_t U, uint8_t V) {
    if (px < 0 || py < 0 || px >= stride || py >= h) return;
    base[py * stride + px] = Y;
    int uidx = (py / 2) * stride + (px / 2) * 2;   // UV 平面 (stride*h/2, 4:2:0)
    base[(size_t)stride * h + uidx]     = U;
    base[(size_t)stride * h + uidx + 1] = V;
}
static void fill_rect_nv12(uint8_t *base, int w, int h, int stride,
                           int x0, int y0, int x1, int y1,
                           uint8_t Y, uint8_t U, uint8_t V) {
    int cx0 = x0 < 0 ? 0 : x0, cy0 = y0 < 0 ? 0 : y0;
    int cx1 = x1 > w ? w : x1, cy1 = y1 > h ? h : y1;
    for (int yy = cy0; yy < cy1; yy++)
        for (int xx = cx0; xx < cx1; xx++)
            nv12_set_px(base, stride, h, xx, yy, Y, U, V);
}
// 在 (px,py) 处画字符串(5x7 字体, 字符间距 1px), 颜色 (Y,U,V)
static void draw_text_nv12(uint8_t *base, int w, int h, int stride,
                           int px, int py, const char *s,
                           uint8_t Y, uint8_t U, uint8_t V) {
    int cur = px;
    for (const char *p = s; *p; p++) {
        int gi = font_idx(*p);
        if (gi < 0) { cur += 6; continue; }
        const uint8_t *g = g_font[gi];
        for (int r = 0; r < 7; r++) {
            uint8_t row = g[r];
            for (int c = 0; c < 5; c++) {
                if (row & (1u << (4 - c)))
                    nv12_set_px(base, stride, h, cur + c, py + r, Y, U, V);
            }
        }
        cur += 6;
        if (cur > w) break;
    }
}

// boxes 已是屏幕子矩形坐标; 减 disp 偏移得 selfpath 帧内坐标再画。
// 每框: 彩色边框 + 顶部类别标签(填充底色 + 暗色文字, 便于在复杂背景上读)。
// 过期的单条记录 (now - last > BOX_PERSIST_MS) 跳过不画(物体已离开)。
static void draw_boxes_on_nv12(uint8_t *base, int w, int h, int stride,
                               const track_t *boxes, int count, int disp_x, int disp_y) {
    char label[48];
    long long now = now_ms();
    for (int i = 0; i < count; i++) {
        const track_t *t = &boxes[i];
        if (now - t->last > BOX_PERSIST_MS) continue;  // 已过期, 不画
        const bbox_t *b = &t->b;
        int x0 = b->x - disp_x, y0 = b->y - disp_y;
        int x1 = x0 + b->w,      y1 = y0 + b->h;
        int cx0 = x0 < 0 ? 0 : x0;
        int cy0 = y0 < 0 ? 0 : y0;
        int cx1 = x1 > w ? w : x1;
        int cy1 = y1 > h ? h : y1;
        if (cx1 <= cx0 || cy1 <= cy0) continue;
        const uint8_t *c = g_box_pal[b->cls % 10];
        uint8_t Y = c[0], U = c[1], V = c[2];
        // 边框
        int th = BOX_THICK;
        for (int yy = cy0; yy < cy1; yy++) {
            for (int xx = cx0; xx < cx1; xx++) {
                int edge = (yy < cy0 + th) || (yy >= cy1 - th) ||
                           (xx < cx0 + th) || (xx >= cx1 - th);
                if (edge) nv12_set_px(base, stride, h, xx, yy, Y, U, V);
            }
        }
        // 标签: "name 92"
        snprintf(label, sizeof(label), "%s %d", cls_name(b->cls),
                 (int)(b->score * 100));
        int tw = (int)strlen(label) * 6;          // 文字像素宽 (5+1 间距)
        int lx0 = cx0, ly0 = cy0 - 9;             // 标签条置于框顶上方
        if (ly0 < 0) { ly0 = cy0 + 1; }           // 顶部越界则放框内
        int lx1 = lx0 + tw, ly1 = ly0 + 8;
        if (lx1 > w) lx1 = w;
        fill_rect_nv12(base, w, h, stride, lx0, ly0, lx1, ly1, Y, U, V); // 底色=框色
        draw_text_nv12(base, w, h, stride, lx0 + 1, ly0 + 1, label, 16, 128, 128); // 暗色字
    }
}

static int rknn_frame_cb(const VIDEO_FRAME_INFO_S *frame, VIDEO_FRAME_INFO_S *out_frame, void *ctx) {
    (void)ctx;
    g_cb_calls++;
    size_t next = g_prod_idx + 1;
    if (next - g_cons_idx >= FRAME_SLOTS) return 0;  // 满, 丢帧 (不阻塞泵)
    frame_slot_t *s = &g_slots[g_prod_idx % FRAME_SLOTS];
    int w = (int)frame->stVFrame.u32Width;
    int h = (int)frame->stVFrame.u32Height;
    int stride = (int)frame->stVFrame.u32VirWidth;
    // selfpath 通道是 DMABUF, pVirAddr 为 NULL; 须用 Handle2VirAddr 经 pMbBlk 取 CPU 虚拟地址
    uint8_t *base = (uint8_t *)RK_MPI_MB_Handle2VirAddr(frame->stVFrame.pMbBlk);
    if (base == NULL) {
        static int warn = 0;
        if (!warn) { printf("[rknn] WARN: Handle2VirAddr NULL (DMABUF pMbBlk)\n"); warn = 1; }
        return 0;
    }
    // NV12: Y 平面 = stride*h, UV 平面(stride*h/2), 二者连续: Y 在 base, UV 在 base+stride*h
    uint32_t y_sz  = (uint32_t)stride * (uint32_t)h;
    uint32_t uv_sz = (uint32_t)stride * (uint32_t)h / 2;
    uint32_t sz = y_sz + uv_sz;
    if (s->nv12 == NULL || (int)sz > s->cap) {
        free(s->nv12);
        s->nv12 = (uint8_t *)malloc(sz + 16);
        s->cap = (int)sz + 16;
    }
    memcpy(s->nv12,                  base, y_sz);
    memcpy(s->nv12 + y_sz, base + y_sz, uv_sz);
    s->w = w; s->h = h; s->stride = stride;
    g_prod_idx = next;
    if (g_cb_queued == 0) {
        printf("[rknn] first frame queued: %dx%d stride=%d base=%p\n", w, h, stride, (void *)base);
        fflush(stdout);
    }
    g_cb_queued++;

    // 把"带框副本"画到自有显示缓冲并交回泵送 VO:
    // 选最久未送的缓冲(round-robin, 保证 VO 已显示完上一轮 -> 无竞争)。
    int slot = 0; long long oldest = g_disp_sent[0];
    for (int i = 1; i < DISP_POOL; i++)
        if (g_disp_sent[i] < oldest) { oldest = g_disp_sent[i]; slot = i; }
    uint8_t *d = g_disp_vaddr[slot];
    if (d == NULL) return 0;  // 自有缓冲未分配 -> 退回送原始帧(无框)
    memcpy(d,     base, y_sz);
    memcpy(d + y_sz, base + y_sz, uv_sz);

    // 画"逐物体跟踪缓存"里的框 + 标签 (送 VO 即带框显示, 参考 rknn 例 cv::rectangle/cv::putText)。
    // 过期条目由 draw_boxes_on_nv12 内部跳过(不在此修改 g_track, 避免破坏跟踪状态)。
    int n = 0;
    pthread_mutex_lock(&g_track_lock);
    n = g_track_n;
    if (n > 0) {
        int dx, dy, dw, dh;
        lcd_preview_get_disp_rect(&dx, &dy, &dw, &dh);  // 屏幕子矩形偏移
        draw_boxes_on_nv12(d, w, h, stride, g_track, n, dx, dy);
        g_overlay_frames++;
        if (g_overlay_frames == 1) {
            printf("[rknn] overlay active: drawing %d tracked boxes (with labels) on own buffer\n", n);
            fflush(stdout);
        }
    }
    pthread_mutex_unlock(&g_track_lock);

    // 填充分发帧: 复制 selfpath 帧的全部元数据(分辨率/格式/压缩/序列号等 VO 必需字段),
    // 仅把像素缓冲替换成我们"自有 MB"(画了框的副本)。VO 显示完释放其引用, 我们仍持有可复用。
    memset(out_frame, 0, sizeof(*out_frame));
    const VIDEO_FRAME_S *sf = &frame->stVFrame;
    VIDEO_FRAME_S *df = &out_frame->stVFrame;
    df->u32Width      = sf->u32Width;
    df->u32Height     = sf->u32Height;
    df->u32VirWidth   = sf->u32VirWidth;
    df->u32VirHeight  = sf->u32VirHeight;
    df->enField       = sf->enField;
    df->enPixelFormat = sf->enPixelFormat;
    df->enVideoFormat = sf->enVideoFormat;
    df->enCompressMode = sf->enCompressMode;
    df->enDynamicRange = sf->enDynamicRange;
    df->enColorGamut  = sf->enColorGamut;
    df->u32TimeRef    = sf->u32TimeRef;
    df->u64PTS        = sf->u64PTS;
    df->u32FrameFlag  = sf->u32FrameFlag;
    df->pMbBlk        = g_disp_mb[slot];
    df->pVirAddr[0]   = d;
    df->pVirAddr[1]   = d + (size_t)stride * h;  // NV12: UV 平面紧跟 Y 之后
    g_disp_sent[slot] = now_ms();
    return 1;  // 送 out_frame
}

// ===================================================================
//  worker 线程: 取帧 -> 转换 -> 推理 -> 发布 bbox
// ===================================================================
static void *rknn_worker(void *arg) {
    (void)arg;
    printf("[rknn] worker started\n");
    long infers = 0, dets = 0;          // 5s 窗口: 推理次数 / 检出框数 (诊断)
    struct timeval tv0; gettimeofday(&tv0, NULL);
    long long stat_us = (long long)tv0.tv_sec * 1000000 + tv0.tv_usec;
    while (!g_quit) {
        // 每 5s 打印一次统计: 无论有没有帧都打 (定位链路断点)
        struct timeval tvn; gettimeofday(&tvn, NULL);
        long long now = (long long)tvn.tv_sec * 1000000 + tvn.tv_usec;
        if (now - stat_us >= 5000000) {
            int cn = 0;
            pthread_mutex_lock(&g_track_lock); cn = g_track_n; pthread_mutex_unlock(&g_track_lock);
            printf("[rknn][STAT] cb_calls=%ld queued=%ld infers=%ld dets=%ld over=%ld track=%d\n",
                   g_cb_calls, g_cb_queued, infers, dets, g_overlay_frames, cn);
            fflush(stdout);
            stat_us = now; infers = 0; dets = 0;
        }
        if (g_cons_idx == g_prod_idx) { usleep(1000); continue; }

        frame_slot_t *s = &g_slots[g_cons_idx % FRAME_SLOTS];
        int fw = s->w, fh = s->h, fstride = s->stride;

        if (!g_bgr_buf || g_bgr_cap < (size_t)fw * fh * 3) {
            free(g_bgr_buf);
            g_bgr_buf = (uint8_t *)malloc((size_t)fw * fh * 3);
            g_bgr_cap = (size_t)fw * fh * 3;
        }
        nv12_to_bgr(s->nv12, fw, fh, fstride, g_bgr_buf);
        uint8_t *in = (uint8_t *)g_app.input_mems[0]->virt_addr;  // 640*640*3 NHWC
        bilinear_bgr_resize(g_bgr_buf, fw, fh, in, RKNN_MODEL_W, RKNN_MODEL_H);

        g_cons_idx++;  // 消费完成, 释放 slot (生产者可覆盖)

        int ret = rknn_run(g_app.rknn_ctx, NULL);
        if (ret < 0) { printf("[rknn] rknn_run fail %d\n", ret); continue; }
        infers++;

        object_detect_result_list od;
        post_process(&g_app, g_app.output_mems, BOX_THRESH, NMS_THRESH, &od);
        dets += od.count;

        // 坐标映射: 模型(640) -> 帧 -> 屏幕子矩形
        int dx, dy, dw, dh;
        lcd_preview_get_disp_rect(&dx, &dy, &dw, &dh);
        float sx = (float)fw / RKNN_MODEL_W;
        float sy = (float)fh / RKNN_MODEL_H;
        bbox_t boxes[BBOX_SHM_MAX_BOXES];
        int n = 0;
        for (int i = 0; i < od.count && n < BBOX_SHM_MAX_BOXES; i++) {
            float x1 = od.results[i].box.left   * sx + dx;
            float y1 = od.results[i].box.top    * sy + dy;
            float x2 = od.results[i].box.right  * sx + dx;
            float y2 = od.results[i].box.bottom * sy + dy;
            boxes[n].x = (int32_t)x1;
            boxes[n].y = (int32_t)y1;
            boxes[n].w = (int32_t)(x2 - x1);
            boxes[n].h = (int32_t)(y2 - y1);
            boxes[n].cls = od.results[i].cls_id;
            boxes[n].score = od.results[i].prop;
            n++;
        }
        update_box_cache(boxes, n);                  // 更新持久缓存 (时间平滑)
        if (g_shm) bbox_shm_publish(g_shm, boxes, n);  // 仍发布给潜在 LVGL 消费端
    }
    printf("[rknn] worker stopped\n");
    return NULL;
}

// ===================================================================
//  对外 API
// ===================================================================
int rknn_infer_init(const char *model_path) {
    memset(&g_app, 0, sizeof(g_app));
    int ret = rknn_init(&g_app.rknn_ctx, (char *)model_path, 0, 0, NULL);
    if (ret < 0) {
        printf("[rknn] rknn_init fail! ret=%d\n", ret);
        return -1;
    }
    ret = rknn_query(g_app.rknn_ctx, RKNN_QUERY_IN_OUT_NUM, &g_app.io_num, sizeof(g_app.io_num));
    if (ret != RKNN_SUCC) { printf("[rknn] query in/out num fail\n"); return -1; }

    // 输入: UINT8 / NHWC (RV1106 零拷贝要求)
    rknn_tensor_attr in_attr;
    memset(&in_attr, 0, sizeof(in_attr));
    in_attr.index = 0;
    ret = rknn_query(g_app.rknn_ctx, RKNN_QUERY_NATIVE_INPUT_ATTR, &in_attr, sizeof(in_attr));
    if (ret != RKNN_SUCC) { printf("[rknn] query input attr fail\n"); return -1; }
    in_attr.type = RKNN_TENSOR_UINT8;
    in_attr.fmt  = RKNN_TENSOR_NHWC;
    g_app.input_attrs = (rknn_tensor_attr *)malloc(sizeof(rknn_tensor_attr));
    memcpy(g_app.input_attrs, &in_attr, sizeof(in_attr));
    g_app.input_mems[0] = rknn_create_mem(g_app.rknn_ctx, in_attr.size_with_stride);
    rknn_set_io_mem(g_app.rknn_ctx, g_app.input_mems[0], &in_attr);

    // 输出: RV1106 用 RKNN_QUERY_NATIVE_NHWC_OUTPUT_ATTR
    g_app.output_attrs = (rknn_tensor_attr *)malloc(sizeof(rknn_tensor_attr) * g_app.io_num.n_output);
    int nout = g_app.io_num.n_output;
    if (nout > 3) nout = 3;
    for (int i = 0; i < nout; i++) {
        g_app.output_attrs[i].index = i;
        ret = rknn_query(g_app.rknn_ctx, RKNN_QUERY_NATIVE_NHWC_OUTPUT_ATTR,
                         &g_app.output_attrs[i], sizeof(rknn_tensor_attr));
        if (ret != RKNN_SUCC) { printf("[rknn] query output %d attr fail\n", i); return -1; }
        g_app.output_mems[i] = rknn_create_mem(g_app.rknn_ctx, g_app.output_attrs[i].size_with_stride);
        rknn_set_io_mem(g_app.rknn_ctx, g_app.output_mems[i], &g_app.output_attrs[i]);
    }

    g_app.is_quant = (g_app.output_attrs[0].qnt_type == RKNN_TENSOR_QNT_AFFINE_ASYMMETRIC);
    if (g_app.input_attrs[0].fmt == RKNN_TENSOR_NCHW) {
        g_app.model_channel = g_app.input_attrs[0].dims[1];
        g_app.model_height  = g_app.input_attrs[0].dims[2];
        g_app.model_width   = g_app.input_attrs[0].dims[3];
    } else {
        g_app.model_height  = g_app.input_attrs[0].dims[1];
        g_app.model_width   = g_app.input_attrs[0].dims[2];
        g_app.model_channel = g_app.input_attrs[0].dims[3];
    }
    printf("[rknn] model input %dx%dx%d, quant=%d\n",
           g_app.model_width, g_app.model_height, g_app.model_channel, g_app.is_quant);
    printf("[rknn] NOTE: 本模块按 640x640 输入实现 (与 luckfox 例子一致)\n");
    return 0;
}

int rknn_infer_start(void) {
    if (g_running) return 0;

    // 帧队列缓冲 (按当前 selfpath 帧分辨率预估)
    int fw = 0, fh = 0, dx = 0, dy = 0;
    lcd_preview_get_disp_rect(&dx, &dy, &fw, &fh);
    if (fw <= 0 || fh <= 0) { fw = 720; fh = 720; }
    for (int i = 0; i < FRAME_SLOTS; i++) {
        g_slots[i].nv12 = (uint8_t *)malloc((size_t)fw * fh * 3 / 2 + 16);
        g_slots[i].cap = fw * fh * 3 / 2 + 16;
        g_slots[i].w = 0; g_slots[i].h = 0; g_slots[i].stride = 0;
    }

    g_shm = bbox_shm_open(BBOX_SHM_NAME, 1);
    if (!g_shm) printf("[rknn] WARN: bbox_shm open failed (LVGL 收不到框)\n");

    g_quit = 0;
    // 分配"自有显示缓冲"池 (画框副本送 VO 用, 避免与 VI 环形缓冲竞争)
    {
        int dw = 0, dh = 0, ddx = 0, ddy = 0;
        lcd_preview_get_disp_rect(&ddx, &ddy, &dw, &dh);
        if (dw <= 0 || dh <= 0) { dw = 720; dh = 720; }
        g_disp_w = dw; g_disp_h = dh; g_disp_stride = dw;
        for (int i = 0; i < DISP_POOL; i++) {
            uint32_t bsz = (uint32_t)g_disp_stride * g_disp_h * 3 / 2;
            MB_BLK mb = NULL;
            if (RK_MPI_SYS_MmzAlloc(&mb, NULL, NULL, bsz) == RK_SUCCESS && mb != NULL) {
                g_disp_mb[i] = mb;
                g_disp_vaddr[i] = (uint8_t *)RK_MPI_MB_Handle2VirAddr(mb);
                g_disp_sent[i] = 0;
            } else {
                g_disp_mb[i] = NULL; g_disp_vaddr[i] = NULL;
                printf("[rknn] WARN: disp buffer %d alloc failed (MB %u bytes)\n", i, bsz);
            }
        }
    }
    // 注册为 lcd_preview 帧消费者 + 复用其 selfpath 通道
    lcd_preview_register_frame_consumer(rknn_frame_cb, NULL);
    if (lcd_preview_ensure_source() != 0) {
        printf("[rknn] ensure_source (selfpath) failed\n");
        lcd_preview_register_frame_consumer(NULL, NULL);
        if (g_shm) { bbox_shm_close(g_shm, 1); g_shm = NULL; }
        for (int i = 0; i < FRAME_SLOTS; i++) { free(g_slots[i].nv12); g_slots[i].nv12 = NULL; }
        return -1;
    }
    int ret = pthread_create(&g_worker, NULL, rknn_worker, NULL);
    if (ret != 0) {
        printf("[rknn] worker thread create failed\n");
        lcd_preview_release_source();
        lcd_preview_register_frame_consumer(NULL, NULL);
        if (g_shm) { bbox_shm_close(g_shm, 1); g_shm = NULL; }
        for (int i = 0; i < FRAME_SLOTS; i++) { free(g_slots[i].nv12); g_slots[i].nv12 = NULL; }
        return -1;
    }
    g_running = 1;
    printf("[rknn] started (reuse LCD selfpath channel)\n");
    return 0;
}

void rknn_infer_stop(void) {
    if (!g_running) return;
    g_running = 0;
    g_quit = 1;
    if (g_worker) { pthread_join(g_worker, NULL); g_worker = 0; }
    lcd_preview_register_frame_consumer(NULL, NULL);
    lcd_preview_release_source();   // 释放对帧源的引用 (LCD 可能仍持有)
    if (g_shm) { bbox_shm_close(g_shm, 1); g_shm = NULL; }
    for (int i = 0; i < FRAME_SLOTS; i++) {
        free(g_slots[i].nv12);
        g_slots[i].nv12 = NULL;
        g_slots[i].cap = 0;
    }
    free(g_bgr_buf); g_bgr_buf = NULL; g_bgr_cap = 0;
    for (int i = 0; i < DISP_POOL; i++) {
        if (g_disp_mb[i]) { RK_MPI_SYS_MmzFree(g_disp_mb[i]); g_disp_mb[i] = NULL; }
        g_disp_vaddr[i] = NULL; g_disp_sent[i] = 0;
    }
    pthread_mutex_lock(&g_track_lock); g_track_n = 0; pthread_mutex_unlock(&g_track_lock);
    printf("[rknn] stopped\n");
}
