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

#include "rknn_api.h"          // RKNN SDK
#include "lcd_preview.h"        // 帧源 (selfpath) + 消费者注册 + 屏幕子矩形
#include "bbox_shm.h"           // cam<->LVGL 检测框通道

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
static void rknn_frame_cb(const VIDEO_FRAME_INFO_S *frame, void *ctx) {
    (void)ctx;
    size_t next = g_prod_idx + 1;
    if (next - g_cons_idx >= FRAME_SLOTS) return;  // 满, 丢帧 (不阻塞泵)
    frame_slot_t *s = &g_slots[g_prod_idx % FRAME_SLOTS];
    int w = (int)frame->stVFrame.u32Width;
    int h = (int)frame->stVFrame.u32Height;
    int stride = (int)frame->stVFrame.u32VirWidth;
    // NV12: Y 平面 = stride*h, UV 平面(stride*h/2), pVirAddr[0]=Y, [1]=UV
    uint32_t y_sz  = (uint32_t)stride * (uint32_t)h;
    uint32_t uv_sz = (uint32_t)stride * (uint32_t)h / 2;
    uint32_t sz = y_sz + uv_sz;
    if (s->nv12 == NULL || (int)sz > s->cap) {
        free(s->nv12);
        s->nv12 = (uint8_t *)malloc(sz + 16);
        s->cap = (int)sz + 16;
    }
    memcpy(s->nv12,                  (uint8_t *)frame->stVFrame.pVirAddr[0], y_sz);
    memcpy(s->nv12 + y_sz, (uint8_t *)frame->stVFrame.pVirAddr[1], uv_sz);
    s->w = w; s->h = h; s->stride = stride;
    g_prod_idx = next;
}

// ===================================================================
//  worker 线程: 取帧 -> 转换 -> 推理 -> 发布 bbox
// ===================================================================
static void *rknn_worker(void *arg) {
    (void)arg;
    printf("[rknn] worker started\n");
    while (!g_quit) {
        if (g_cons_idx == g_prod_idx) { usleep(1000); continue; }

        frame_slot_t *s = &g_slots[g_cons_idx % FRAME_SLOTS];
        int fw = s->w, fh = s->h, fstride = s->stride;

        uint8_t *bgr = (uint8_t *)malloc((size_t)fw * fh * 3);
        nv12_to_bgr(s->nv12, fw, fh, fstride, bgr);
        uint8_t *in = (uint8_t *)g_app.input_mems[0]->virt_addr;  // 640*640*3 NHWC
        bilinear_bgr_resize(bgr, fw, fh, in, RKNN_MODEL_W, RKNN_MODEL_H);
        free(bgr);

        g_cons_idx++;  // 消费完成, 释放 slot (生产者可覆盖)

        int ret = rknn_run(g_app.rknn_ctx, NULL);
        if (ret < 0) { printf("[rknn] rknn_run fail %d\n", ret); continue; }

        object_detect_result_list od;
        post_process(&g_app, g_app.output_mems, BOX_THRESH, NMS_THRESH, &od);

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
        if (g_shm) bbox_shm_publish(g_shm, boxes, n);
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
    printf("[rknn] stopped\n");
}
