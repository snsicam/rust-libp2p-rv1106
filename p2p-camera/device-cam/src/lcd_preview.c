// SPDX-License-Identifier: MIT
// LCD 预览模块实现 — 从 rk_camera.c 的 lcd_vo_* 抽取而来。
// 管线: VI selfpath(chn) --RK_MPI_VI_GetChnFrame--> [VO SendFrame] + [帧消费者回调] --> VOP / rknn
// VOP 显示控制器硬件完成 NV12→RGB CSC + 缩放, 完全不占 CPU。
// 【为何不另起 RGA】librga 在同一进程内是单例上下文, SDK 的 VPSS/VENC/VO 内部已
// 共用同一个 RGA 单例做缩放/CSC; 进程内若再有第二个 RGA 使用者并发操作同一上下文
// 会破坏其内部 dma_buf/fd 记账, 触发 kernel panic。VO 路径全程走 MPP 自己的 RGA
// 上下文, 不存在第二使用者, 故安全且不占额外 CPU。

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <unistd.h>
#include <time.h>
#include <sys/time.h>

#include "rk_mpi_sys.h"
#include "rk_mpi_vi.h"
#include "rk_mpi_vo.h"
#include "rk_mpi_mb.h"
#include "rk_common.h"
#include "rk_comm_video.h"
#include "rk_comm_vi.h"

#include "lcd_preview.h"

// VI 设备号 (与 rk_camera.c 的 VI_DEV_ID 同为 0, 这里用独立宏避免重复定义)
#define LCD_VI_DEV_ID 0

// ---- 模块私有状态 (原 rk_camera.c 的 LCD 全局变量) ----
static int g_lcd_width = 720;
static int g_lcd_height = 720;
static int g_lcd_fps = 20;             // LCD(VO) 显示帧率, 用于限速喂帧 + u32DispFrmRt
static int g_sensor_fps = 30;          // sensor 原生帧率, 供 selfpath VI 通道设源帧率
static int g_lcd_disp_x = 0;          // VO video plane 在屏幕上的左上角 X (局部显示)
static int g_lcd_disp_y = 0;          // VO video plane 在屏幕上的左上角 Y (局部显示)
static pthread_t g_lcd_thread = 0;
static volatile int g_lcd_quit = 0;    // 模块私有退出标志 (泵线程用)
static int g_lcd_enabled = 0;          // 由 set_config 设置, is_enabled() 返回

static int g_lcd_use_vo = 0;
static int g_lcd_vo_layer = 0;
static int g_lcd_vo_dev   = 0;
static int g_lcd_vo_chn   = 0;
static int g_lcd_vi_selfpath_chn = 1;  // 1 = rkisp_selfpath (0=mainpath 已绑 VPSS)

// 帧消费者 (rknn 推理注册于此)
static lcd_preview_frame_cb g_frame_cb = NULL;
static void *g_frame_ctx = NULL;

// 帧源引用计数: LCD 显示 + rknn 推理任一需要即保持开启
static int g_source_users = 0;

void lcd_preview_set_config(int w, int h, int fps) {
    if (w > 0) g_lcd_width = w;
    if (h > 0) g_lcd_height = h;
    if (fps > 0) g_lcd_fps = fps;
    g_lcd_enabled = 1;
    printf("[lcd_preview] config: %dx%d @%dfps\n", g_lcd_width, g_lcd_height, fps);
}

// sensor 原生帧率: 由 rk_camera.c 的 VI 主通道初始化时写入;
// 模块内 selfpath VI 通道用其设源帧率。
void lcd_preview_set_sensor_fps(int fps) {
    if (fps > 0) g_sensor_fps = fps;
}
int lcd_preview_get_sensor_fps(void) {
    return g_sensor_fps > 0 ? g_sensor_fps : 30;
}

// 设置 video plane 在屏幕上的子矩形位置 (局部显示用)
//   x, y: 左上角坐标 (默认 0,0 = 全屏)
void lcd_preview_set_rect(int x, int y) {
    g_lcd_disp_x = x;
    g_lcd_disp_y = y;
    printf("[lcd_preview] disp rect: (%d,%d)\n", g_lcd_disp_x, g_lcd_disp_y);
}

int lcd_preview_is_enabled(void) {
    return g_lcd_enabled;
}

void lcd_preview_register_frame_consumer(lcd_preview_frame_cb cb, void *ctx) {
    g_frame_cb = cb;
    g_frame_ctx = ctx;
}

void lcd_preview_get_disp_rect(int *x, int *y, int *w, int *h) {
    if (x) *x = g_lcd_disp_x;
    if (y) *y = g_lcd_disp_y;
    if (w) *w = g_lcd_width;
    if (h) *h = g_lcd_height;
}

// ---- selfpath VI 通道 (中性帧源) ----
// 仅开/关 selfpath 通道, 不碰 VO。由 ensure_source/release_source 引用计数管理。
static int lcd_vi_selfpath_init(void) {
    VI_CHN_ATTR_S vi_attr;
    memset(&vi_attr, 0, sizeof(vi_attr));
    vi_attr.stIspOpt.u32BufCount = 3;
    vi_attr.stIspOpt.enMemoryType = VI_V4L2_MEMORY_TYPE_DMABUF;
    vi_attr.stSize.u32Width  = (uint32_t)g_lcd_width;
    vi_attr.stSize.u32Height = (uint32_t)g_lcd_height;
    vi_attr.enPixelFormat = RK_FMT_YUV420SP;
    vi_attr.enCompressMode = COMPRESS_MODE_NONE;
    vi_attr.u32Depth = 2;  // 必须 < u32BufCount, 否则 GetChnFrame 失败
    // 显式设 selfpath 帧率: 源=sensor 原生帧率, 目的=LCD 显示帧率。
    vi_attr.stFrameRate.s32SrcFrameRate = g_sensor_fps > 0 ? g_sensor_fps : 30;
    vi_attr.stFrameRate.s32DstFrameRate = g_lcd_fps > 0 ? g_lcd_fps : 20;
    int ret = RK_MPI_VI_SetChnAttr(LCD_VI_DEV_ID, g_lcd_vi_selfpath_chn, &vi_attr);
    if (ret != RK_SUCCESS) {
        printf("[lcd_preview] VI SetChnAttr(selfpath) failed %x\n", ret);
        return -1;
    }
    ret = RK_MPI_VI_EnableChn(LCD_VI_DEV_ID, g_lcd_vi_selfpath_chn);
    if (ret != RK_SUCCESS) {
        printf("[lcd_preview] VI EnableChn(selfpath) failed %x\n", ret);
        return -1;
    }
    return 0;
}

static void lcd_vi_selfpath_deinit(void) {
    RK_MPI_VI_DisableChn(LCD_VI_DEV_ID, g_lcd_vi_selfpath_chn);
}

// ---- VO (仅 LCD 显示需要) ----
// 假设 selfpath VI 已由 ensure_source 打开。VO 失败不影响帧源 (rknn 可能仍用)。
static int lcd_vo_init(void) {
    int ret;
    VO_PUB_ATTR_S         pub;
    VO_VIDEO_LAYER_ATTR_S layer;
    VO_CHN_ATTR_S         chn;
    memset(&pub, 0, sizeof(pub));
    memset(&layer, 0, sizeof(layer));
    memset(&chn, 0, sizeof(chn));

    ret = RK_MPI_VO_BindLayer(g_lcd_vo_layer, g_lcd_vo_dev, VO_LAYER_MODE_GRAPHIC);
    if (ret != RK_SUCCESS) {
        printf("[lcd_preview] VO: BindLayer failed %x\n", ret);
        return -1;
    }
    pub.enIntfType = VO_INTF_DEFAULT;
    pub.enIntfSync = VO_OUTPUT_DEFAULT;
    ret = RK_MPI_VO_SetPubAttr(g_lcd_vo_dev, &pub);
    if (ret != RK_SUCCESS) { printf("[lcd_preview] VO: SetPubAttr failed %x\n", ret); goto fail_unbind; }
    ret = RK_MPI_VO_Enable(g_lcd_vo_dev);
    if (ret != RK_SUCCESS) { printf("[lcd_preview] VO: Enable failed %x\n", ret); goto fail_unbind; }

    layer.enPixFormat      = RK_FMT_RGB888;
    layer.enCompressMode   = COMPRESS_AFBC_16x16;
    // 局部显示: video plane 定位到屏幕子矩形 (disp_x/disp_y 为左上角)
    layer.stDispRect.s32X  = g_lcd_disp_x;
    layer.stDispRect.s32Y  = g_lcd_disp_y;
    layer.stDispRect.u32Width  = (uint32_t)g_lcd_width;
    layer.stDispRect.u32Height = (uint32_t)g_lcd_height;
    layer.stImageSize.u32Width  = (uint32_t)g_lcd_width;
    layer.stImageSize.u32Height = (uint32_t)g_lcd_height;
    layer.u32DispFrmRt = g_lcd_fps > 0 ? g_lcd_fps : 25;
    ret = RK_MPI_VO_SetLayerAttr(g_lcd_vo_layer, &layer);
    if (ret != RK_SUCCESS) { printf("[lcd_preview] VO: SetLayerAttr failed %x\n", ret); goto fail_vo; }
    // 注意: 不调用 RK_MPI_VO_SetLayerDispBufLen 调大显示缓冲池。
    // 该平台显示缓冲内存有限, 设 8 帧会在 EnableLayer 时报
    // "not enough displaybuf buf len" 导致 VO 整体初始化失败 -> LCD 黑屏。
    // 改为在泵里用 SendFrame(0) 非阻塞: VO 显示缓冲满时立即丢帧,
    // 投喂速率自动跟随 composer 实际消费率, 从源头避免缓冲池堆积/ no free buffer。
    RK_MPI_VO_SetLayerSpliceMode(g_lcd_vo_layer, VO_SPLICE_MODE_RGA);
    ret = RK_MPI_VO_EnableLayer(g_lcd_vo_layer);
    if (ret != RK_SUCCESS) { printf("[lcd_preview] VO: EnableLayer failed %x\n", ret); goto fail_vo; }

    chn.stRect.s32X = g_lcd_disp_x;
    chn.stRect.s32Y = g_lcd_disp_y;
    chn.stRect.u32Width  = (uint32_t)g_lcd_width;
    chn.stRect.u32Height = (uint32_t)g_lcd_height;
    chn.u32FgAlpha = 255;
    chn.u32BgAlpha = 0;
    chn.enMirror = MIRROR_NONE;
    chn.enRotation = ROTATION_0;
    chn.u32Priority = 1;
    ret = RK_MPI_VO_SetChnAttr(g_lcd_vo_layer, g_lcd_vo_chn, &chn);
    if (ret != RK_SUCCESS) { printf("[lcd_preview] VO: SetChnAttr failed %x\n", ret); goto fail_vo; }
    ret = RK_MPI_VO_EnableChn(g_lcd_vo_layer, g_lcd_vo_chn);
    if (ret != RK_SUCCESS) { printf("[lcd_preview] VO: EnableChn failed %x\n", ret); goto fail_vo; }

    printf("[lcd_preview] VO initialized (VOP hardware CSC, %dx%d)\n",
           g_lcd_width, g_lcd_height);
    return 0;

fail_vo:
    RK_MPI_VO_DisableChn(g_lcd_vo_layer, g_lcd_vo_chn);
    RK_MPI_VO_DisableLayer(g_lcd_vo_layer);
    RK_MPI_VO_Disable(g_lcd_vo_dev);
fail_unbind:
    RK_MPI_VO_UnBindLayer(g_lcd_vo_layer, g_lcd_vo_dev);
    return -1;
}

static void lcd_vo_deinit(void) {
    RK_MPI_VO_DisableChn(g_lcd_vo_layer, g_lcd_vo_chn);
    RK_MPI_VO_DisableLayer(g_lcd_vo_layer);
    RK_MPI_VO_Disable(g_lcd_vo_dev);
    RK_MPI_VO_UnBindLayer(g_lcd_vo_layer, g_lcd_vo_dev);
    RK_MPI_VO_CloseFd();
}

// ---- 送帧泵线程 (帧源核心) ----
// 单生产者: 每帧 GetChnFrame -> [VO SendFrame(仅 LCD 时)] -> [帧消费者回调(如 rknn)] -> Release。
// 推理等重活在消费者回调里只做"拷贝像素到队列" (µs 级), 真正推理在 rknn 自己线程异步跑。
static void *lcd_vo_thread(void *arg) {
    (void)arg;
    printf("[lcd_preview] frame pump started (selfpath -> VO?%d, consumer?%d)\n",
           g_lcd_use_vo, (g_frame_cb != NULL));
    VIDEO_FRAME_INFO_S frame;
    long long frames = 0;
    long long start_us = 0;

    while (!g_lcd_quit) {
        int ret = RK_MPI_VI_GetChnFrame(LCD_VI_DEV_ID, g_lcd_vi_selfpath_chn, &frame, 1000);
        if (ret != RK_SUCCESS) {
            // 超时或暂无可取帧: 继续, 不阻塞编码管线
            continue;
        }

        // 视频显示: 仅 LCD 启用时送 VO video plane (VOP 硬件 CSC, 零 CPU)
        // SendFrame 用 0 (非阻塞): VO 显示缓冲满时立即返回失败, 此处直接丢帧。
        if (g_lcd_use_vo) {
            ret = RK_MPI_VO_SendFrame(g_lcd_vo_layer, g_lcd_vo_chn, &frame, 0);
        }

        // 帧消费者 (rknn): 必须在 Release 之前调用 (Release 后缓冲可能被回收)。
        // 回调内只能拷贝像素, 不得长期持有 frame / 跑推理。
        if (g_frame_cb) {
            g_frame_cb(&frame, g_frame_ctx);
        }

        RK_MPI_VI_ReleaseChnFrame(LCD_VI_DEV_ID, g_lcd_vi_selfpath_chn, &frame);

        // 仅 LCD 模式: VO 缓冲满(或暂时不可送)时丢弃此帧继续, 不阻塞、不堆积
        if (g_lcd_use_vo && ret != RK_SUCCESS) {
            continue;
        }

        struct timeval tv; gettimeofday(&tv, NULL);
        long long now = (long long)tv.tv_sec * 1000000 + tv.tv_usec;
        frames++;
        if (start_us == 0) {
            start_us = now;
        } else if (now - start_us >= 5000000) {
            double fps = frames / ((now - start_us) / 1000000.0);
            printf("[lcd_preview][PUMP_STAT] fps=%.1f\n", fps);
            fflush(stdout);
            start_us = now; frames = 0;
        }
    }
    printf("[lcd_preview] frame pump stopped\n");
    return NULL;
}

// ---- 中性帧源引用计数 ----
int lcd_preview_ensure_source(void) {
    if (g_source_users == 0) {
        if (lcd_vi_selfpath_init() != 0) {
            printf("[lcd_preview] selfpath init failed\n");
            return -1;
        }
        g_lcd_quit = 0;
        int ret = pthread_create(&g_lcd_thread, NULL, lcd_vo_thread, NULL);
        if (ret != 0) {
            printf("[lcd_preview] WARN: pump thread create failed\n");
            lcd_vi_selfpath_deinit();
            return -1;
        }
    }
    g_source_users++;
    return 0;
}

void lcd_preview_release_source(void) {
    if (g_source_users <= 0) return;
    g_source_users--;
    if (g_source_users == 0) {
        g_lcd_quit = 1;
        if (g_lcd_thread) {
            pthread_join(g_lcd_thread, NULL);
            g_lcd_thread = 0;
        }
        lcd_vi_selfpath_deinit();
    }
}

// ---- LCD 显示启动 / 停止 ----
int lcd_preview_start(void) {
    if (g_lcd_use_vo) {
        printf("[lcd_preview] VO already running\n");
        return 0;
    }
    // 确保中性帧源已开 (可能 rknn 已先开)
    if (lcd_preview_ensure_source() != 0) {
        g_lcd_enabled = 0;
        return -1;
    }
    if (lcd_vo_init() != 0) {
        printf("[lcd_preview] VO init failed, LCD disabled\n");
        // 释放本次对帧源的引用 (rknn 可能仍持有, 不在此强关)
        lcd_preview_release_source();
        g_lcd_enabled = 0;
        return -1;
    }
    g_lcd_use_vo = 1;
    printf("[lcd_preview] started (VO hardware CSC)\n");
    return 0;
}

void lcd_preview_stop(void) {
    int was_vo = g_lcd_use_vo;
    g_lcd_use_vo = 0;
    // 释放 LCD 对帧源的引用 (若 rknn 仍持有, 泵/通道继续为 rknn 服务)
    lcd_preview_release_source();
    if (was_vo) {
        lcd_vo_deinit();
    }
    g_lcd_enabled = 0;
}
