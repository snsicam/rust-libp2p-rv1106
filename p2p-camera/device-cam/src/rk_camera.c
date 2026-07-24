// SPDX-License-Identifier: MIT
// RV1106 Camera SDK C shim — 封装 VI + VPSS + VENC (三码流) 初始化为简单接口
//
// 管线:
//   VI(dev=0, chn=0) → VPSS(grp=0) ┬→ VENC(chn=0) 主码流
//                                    ├→ VENC(chn=1) 子码流
//                                    └→ VENC(chn=2) 第三码流
//
// 编译: 由 build.rs 自动编译为 librk_camera.a
// 链接: librockit_full.so + librkaiq.so

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <unistd.h>
#include <time.h>
#include <fcntl.h>
#include <errno.h>
#include <sys/ioctl.h>
#include <sys/time.h>
#include <sys/mman.h>
#include <linux/fb.h>
#include <linux/videodev2.h>

#include "rk_mpi_sys.h"
#include "rk_mpi_vi.h"
#include "rk_mpi_vo.h"
#include "rk_mpi_venc.h"
#include "rk_mpi_vpss.h"
#include "rk_mpi_mb.h"
#include "rk_mpi_ai.h"
#include "rk_mpi_aenc.h"
#include "rk_common.h"
#include "rk_comm_video.h"
#include "rk_comm_venc.h"
#include "rk_comm_vi.h"
#include "rk_comm_vpss.h"
#include "rk_comm_aio.h"
#include "RgaApi.h"

#include "lcd_preview.h"   // LCD 预览模块 (抽取自原 lcd_vo_*)

// ISP (rkaiq) 头文件
#include "rk_aiq_user_api2_sysctl.h"
#include "rk_aiq_user_api2_imgproc.h"

// ---- 前向声明: rk_camera_set_chn_config 等在使用点之前调用这些 static 辅助函数 ----
static int get_codec_type(const char *codec_str);
static int get_rc_mode(const char *rc_str, int codec);
static int get_rc_quality(const char *q_str);
static int get_profile(const char *p_str);
static int get_gop_mode(const char *g_str);
static int get_mirror(const char *m_str);

// ---- 前向声明: 控制通道持久化恢复接口 ----
int rk_isp_set_from_ini(int cam_id);
static void rk_video_set_from_ini(void);

// ---- 常量定义 ----

#define VI_DEV_ID           0
#define VI_CHN_ID           0   // rkisp_mainpath
#define VPSS_GRP_ID         0
#define CAM_ID              0
#define IQ_FILE_DIR         "/etc/iqfiles"

// 三路 VENC 通道
#define VENC_CHN_MAIN       0
#define VENC_CHN_SUB        1
#define VENC_CHN_THIRD      2
#define VENC_MAX_CHN        3

// VPSS 三路输出通道 (编码) — LCD 显示不再经过 VPSS, 单独走 V4L2 路径
#define VPSS_CHN_MAIN       VPSS_CHN0
#define VPSS_CHN_SUB        VPSS_CHN1
#define VPSS_CHN_THIRD      VPSS_CHN2

// ---- 全局状态 ----

static volatile int g_quit = 0;
static volatile int g_initialized = 0;
static rk_aiq_sys_ctx_t *g_aiq_ctx = NULL;

// LCD 显示 — MPP VO 模块 (VOP 硬件 CSC, 零 CPU 转换)
// 管线: VI selfpath(chn1) --RK_MPI_VI_GetChnFrame--> RK_MPI_VO_SendFrame --> VOP
// VOP 显示控制器硬件完成 NV12→RGB CSC + 缩放, 完全不占 CPU。
// 【为何不另起 RGA】librga 在同一进程内是单例上下文, SDK 的 VPSS/VENC/VO 内部已
// 共用同一个 RGA 单例做缩放/CSC; 进程内若再有第二个 RGA 使用者并发操作同一上下文
// 会破坏其内部 dma_buf/fd 记账, 触发 kernel panic。VO 路径全程走 MPP 自己的 RGA
// 上下文, 不存在第二使用者, 故安全且不占额外 CPU。
static volatile int g_fb_enabled = 0;  // LCD 显示使能 (1=已启用 VO 预览)
// 以下 LCD 显示私有状态(g_lcd_width/height/fps, g_sensor_fps, g_lcd_thread)
// 已迁移至 lcd_preview.c 模块 (见 lcd_preview.h), 由 lcd_preview_start/stop 控制。

// MPP 系统引用计数 (与 audio 共享)
static volatile int g_sys_init_count = 0;

// 每个 encoder channel 的取流线程
static pthread_t g_stream_threads[VENC_MAX_CHN];
static volatile int g_chn_enabled[VENC_MAX_CHN] = {0, 0, 0};

// 每通道的 encoder 参数配置
typedef struct {
    int width;              // 输出分辨率
    int height;
    int src_fps_num;        // 源帧率
    int src_fps_den;
    int dst_fps_num;        // 目标帧率 (实际编码帧率)
    int dst_fps_den;
    int bitrate_kbps;       // 码率
    int gop;                // GOP 帧数
    int codec;              // RK_VIDEO_ID_HEVC 或 RK_VIDEO_ID_AVC
    int rc_mode;            // VENC_RC_MODE_H265CBR 等
    int rc_quality;         // 0=lowest..6=highest (-1=不设置)
    int gop_mode;           // VENC_GOP_MODE_NORMALP 或 VENC_GOP_MODE_SMARTP
    int profile;            // 0=main, 100=high
    int mirror;             // MIRROR_NONE / HORIZONTAL / VERTICAL / BOTH
    int smartp_viridrlen;
    int stream_buf_cnt;
} ChnEncAttr;

static ChnEncAttr g_chn_attr[VENC_MAX_CHN];

// 帧回调: fn(chn_id, data, len, pts)
// 注意: 不再传 is_keyframe —— 关键帧判定改由 viewer 侧字节扫描完成,
// cam 侧不计算(省 RV1106 每帧 CPU), 见 rk_video_source.rs。
typedef void (*frame_callback_t)(int chn_id, const uint8_t *data, uint32_t len,
                                  uint64_t pts);
static frame_callback_t g_callback = NULL;

// ---- 辅助函数 ----

static uint64_t get_now_us() {
    struct timespec ts = {0, 0};
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000 + (uint64_t)ts.tv_nsec / 1000;
}

static int ensure_sys_init() {
    if (g_sys_init_count == 0) {
        int ret = RK_MPI_SYS_Init();
        if (ret != RK_SUCCESS) {
            printf("[rk_camera] RK_MPI_SYS_Init failed: %x\n", ret);
            return -1;
        }
    }
    g_sys_init_count++;
    return 0;
}

static void maybe_sys_exit() {
    if (g_sys_init_count > 0) {
        g_sys_init_count--;
        if (g_sys_init_count == 0) {
            RK_MPI_SYS_Exit();
        }
    }
}

// ---- ISP 初始化 ----

static int isp_init() {
    int ret;
    rk_aiq_working_mode_t wdr_mode = RK_AIQ_WORKING_MODE_NORMAL;

    char hdr_str[16];
    snprintf(hdr_str, sizeof(hdr_str), "%d", (int)wdr_mode);
    setenv("HDR_MODE", hdr_str, 1);

    rk_aiq_static_info_t aiq_static_info;
    ret = rk_aiq_uapi2_sysctl_enumStaticMetasByPhyId(CAM_ID, &aiq_static_info);
    if (ret < 0 || aiq_static_info.sensor_info.phyId == -1) {
        printf("[rk_camera] WARN: sensor not found, ISP disabled\n");
        return 0;
    }
    printf("[rk_camera] sensor: %s\n", aiq_static_info.sensor_info.sensor_name);

    rk_aiq_uapi2_sysctl_preInit_devBufCnt(
        aiq_static_info.sensor_info.sensor_name, "rkraw_rx", 2);

    // sub_scene 不能为 NULL (对标 rkipc: 至少传空字符串 "")
    ret = rk_aiq_uapi2_sysctl_preInit_scene(
        aiq_static_info.sensor_info.sensor_name, "normal", "");
    if (ret < 0) printf("[rk_camera] WARN: preInit_scene failed\n");

    g_aiq_ctx = rk_aiq_uapi2_sysctl_init(
        aiq_static_info.sensor_info.sensor_name, IQ_FILE_DIR, NULL, NULL);
    if (!g_aiq_ctx) {
        printf("[rk_camera] WARN: sysctl_init failed, ISP disabled\n");
        return 0;
    }

    if (rk_aiq_uapi2_sysctl_prepare(g_aiq_ctx, 0, 0, wdr_mode)) {
        printf("[rk_camera] WARN: sysctl_prepare failed\n");
        g_aiq_ctx = NULL;
        return 0;
    }
    if (rk_aiq_uapi2_sysctl_start(g_aiq_ctx)) {
        printf("[rk_camera] WARN: sysctl_start failed\n");
        g_aiq_ctx = NULL;
        return 0;
    }

    printf("[rk_camera] ISP started (IQ: %s)\n", IQ_FILE_DIR);
    return 0;
}

static void isp_deinit() {
    if (g_aiq_ctx) {
        rk_aiq_uapi2_sysctl_stop(g_aiq_ctx, false);
        rk_aiq_uapi2_sysctl_deinit(g_aiq_ctx);
        g_aiq_ctx = NULL;
        printf("[rk_camera] ISP stopped\n");
    }
}

// ---- VI 初始化 (捕获主码流分辨率) ----

static int vi_dev_init() {
    int devId = VI_DEV_ID;
    int pipeId = devId;
    int ret;

    VI_DEV_ATTR_S stDevAttr;
    VI_DEV_BIND_PIPE_S stBindPipe;
    memset(&stDevAttr, 0, sizeof(stDevAttr));
    memset(&stBindPipe, 0, sizeof(stBindPipe));

    ret = RK_MPI_VI_GetDevAttr(devId, &stDevAttr);
    if (ret == RK_ERR_VI_NOT_CONFIG) {
        ret = RK_MPI_VI_SetDevAttr(devId, &stDevAttr);
        if (ret != RK_SUCCESS) return -1;
    }

    ret = RK_MPI_VI_GetDevIsEnable(devId);
    if (ret != RK_SUCCESS) {
        ret = RK_MPI_VI_EnableDev(devId);
        if (ret != RK_SUCCESS) return -1;

        stBindPipe.u32Num = 1;
        stBindPipe.PipeId[0] = pipeId;
        ret = RK_MPI_VI_SetDevBindPipe(devId, &stBindPipe);
        if (ret != RK_SUCCESS) return -1;
    }

    return 0;
}

static int vi_chn_init(int width, int height, int fps, int sensor_fps) {
    VI_CHN_ATTR_S vi_chn_attr;
    memset(&vi_chn_attr, 0, sizeof(vi_chn_attr));
    vi_chn_attr.stIspOpt.u32BufCount = 3;
    vi_chn_attr.stIspOpt.enMemoryType = VI_V4L2_MEMORY_TYPE_DMABUF;
    vi_chn_attr.stSize.u32Width = width;
    vi_chn_attr.stSize.u32Height = height;
    vi_chn_attr.enPixelFormat = RK_FMT_YUV420SP;
    vi_chn_attr.enCompressMode = COMPRESS_MODE_NONE;
    vi_chn_attr.u32Depth = 0;
    // VI 全速运行在 sensor 原生帧率 (src=dst=sensor_fps), 不做丢帧;
    // 各码流的帧率控制由各 VENC 的 fr32DstFrameRate 完成 (见 venc_init_single)。
    if (sensor_fps <= 0) sensor_fps = fps;  // 回退: sensor_fps 未知时使用 target fps
    lcd_preview_set_sensor_fps(sensor_fps);
    vi_chn_attr.stFrameRate.s32SrcFrameRate = sensor_fps;
    vi_chn_attr.stFrameRate.s32DstFrameRate = sensor_fps;

    printf("[rk_camera] VI chn: %dx%d src_fps=%d dst_fps=%d\n", width, height, sensor_fps, fps);

    int ret = RK_MPI_VI_SetChnAttr(VI_DEV_ID, VI_CHN_ID, &vi_chn_attr);
    ret |= RK_MPI_VI_EnableChn(VI_DEV_ID, VI_CHN_ID);
    return ret;
}

// ---- VPSS 初始化 (3 路缩放输出) ----

static int vpss_init(int main_w, int main_h,
                     int sub_w, int sub_h,
                     int third_w, int third_h) {
    int ret;
    VPSS_GRP_ATTR_S stGrpAttr;
    VPSS_CHN_ATTR_S stChnAttr;

    memset(&stGrpAttr, 0, sizeof(stGrpAttr));
    stGrpAttr.u32MaxW = 4096;
    stGrpAttr.u32MaxH = 4096;
    stGrpAttr.enPixelFormat = RK_FMT_YUV420SP;
    // VPSS 不做帧率控制, 全速透传 (对标 rkipc: 由 VI 和 VENC 各自控制帧率)
    stGrpAttr.stFrameRate.s32SrcFrameRate = -1;
    stGrpAttr.stFrameRate.s32DstFrameRate = -1;
    stGrpAttr.enCompressMode = COMPRESS_MODE_NONE;

    ret = RK_MPI_VPSS_CreateGrp(VPSS_GRP_ID, &stGrpAttr);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_VPSS_CreateGrp failed: %x\n", ret);
        return ret;
    }

    // 设置 VProc 设备 (使用 RGA 做缩放)
    ret = RK_MPI_VPSS_SetVProcDev(VPSS_GRP_ID, VIDEO_PROC_DEV_RGA);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_VPSS_SetVProcDev failed: %x\n", ret);
        return ret;
    }

    // 编码通道: CHN0 (主), CHN1 (子), CHN2 (第三)
    {
        VPSS_CHN vpss_chns[] = {VPSS_CHN_MAIN, VPSS_CHN_SUB, VPSS_CHN_THIRD};
        int widths[] = {main_w, sub_w, third_w};
        int heights[] = {main_h, sub_h, third_h};

        for (int i = 0; i < 3; i++) {
            memset(&stChnAttr, 0, sizeof(stChnAttr));
            stChnAttr.enChnMode = VPSS_CHN_MODE_AUTO;  // 绑定到 VENC 的通道用 AUTO 模式
            stChnAttr.enDynamicRange = DYNAMIC_RANGE_SDR8;
            stChnAttr.enPixelFormat = RK_FMT_YUV420SP;
            stChnAttr.stFrameRate.s32SrcFrameRate = -1;
            stChnAttr.stFrameRate.s32DstFrameRate = -1;
            stChnAttr.u32Width = widths[i];
            stChnAttr.u32Height = heights[i];
            stChnAttr.enCompressMode = COMPRESS_MODE_NONE;

            ret = RK_MPI_VPSS_SetChnAttr(VPSS_GRP_ID, vpss_chns[i], &stChnAttr);
            if (ret != RK_SUCCESS) {
                printf("[rk_camera] VPSS SetChnAttr[%d] failed: %x\n", i, ret);
                return ret;
            }

            ret = RK_MPI_VPSS_EnableChn(VPSS_GRP_ID, vpss_chns[i]);
            if (ret != RK_SUCCESS) {
                printf("[rk_camera] VPSS EnableChn[%d] failed: %x\n", i, ret);
                return ret;
            }
        }
    }

    // 注意: RV1106 仅 1 个 VPSS Group, 同一 Group 内不能混用
    // AUTO(绑定 VENC) 和 USER(GetChnFrame) 模式, 会导致 MPP buffer 崩溃。
    // 因此 LCD 预览不走 VPSS 通道, 而用 ISP selfpath (/dev/video12) 独立抓帧。
    // 代价: selfpath 是 ISP 独立的预览路径, 内部缓冲比 mainpath(编码源) 多 1~2 帧,
    // 故 LCD 预览延时通常略大于网络流; 已通过 BUF_CNT=2 + 实时线程优先级尽量压低。

    // 启动 VPSS Group
    ret = RK_MPI_VPSS_StartGrp(VPSS_GRP_ID);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_VPSS_StartGrp failed: %x\n", ret);
        return ret;
    }

    printf("[rk_camera] VPSS init: group=%d, outputs: %dx%d / %dx%d / %dx%d%s\n",
           VPSS_GRP_ID, main_w, main_h, sub_w, sub_h, third_w, third_h,
           g_fb_enabled ? " + LCD" : "");
    return 0;
}

// ---- VENC 单通道初始化 ----
static int venc_init_single(int chn_id, int width, int height,
                             int fps_num, int fps_den, int sensor_fps,
                             int bitrate_kbps, int gop,
                             int codec, int rc_mode, int rc_quality,
                             int profile, int gop_mode, int mirror,
                             int smartp_viridrlen, int stream_buf_cnt) {
    VENC_CHN_ATTR_S stAttr;
    VENC_RC_ATTR_S *pRcAttr;
    memset(&stAttr, 0, sizeof(stAttr));

    // 编码器类型
    stAttr.stVencAttr.enType = codec;
    stAttr.stVencAttr.enPixelFormat = RK_FMT_YUV420SP;
    stAttr.stVencAttr.u32PicWidth = width;
    stAttr.stVencAttr.u32PicHeight = height;
    stAttr.stVencAttr.u32VirWidth = width;
    stAttr.stVencAttr.u32VirHeight = height;
    stAttr.stVencAttr.u32Profile = profile;
    stAttr.stVencAttr.u32StreamBufCnt = stream_buf_cnt;
    stAttr.stVencAttr.u32BufSize = width * height * 3 / 2;
    stAttr.stVencAttr.enMirror = mirror;

    // 码率控制
    pRcAttr = &stAttr.stRcAttr;
    pRcAttr->enRcMode = rc_mode;

    // 设置帧率/GOP/码率 (根据编码类型和RC模式设置对应 union 字段)
    int is_vbr = (rc_mode == VENC_RC_MODE_H264VBR || rc_mode == VENC_RC_MODE_H265VBR);

    // 帧率控制 (对标 rkipc video.c rk_video_set_frame_rate 的分数帧率路径):
    // 本管线 VI 为三路共享、无法按单路控速, 因此由 VENC 统一按目标帧率丢帧。
    // 关键: u32SrcFrameRate 必须等于 VENC 实际接收帧率(=VI/VPSS 输出=输入源帧率 sensor_fps),
    //       否则 Src==Dst 时 ratio=1 不丢帧, 实测会跑满 sensor 原生帧率(如 30fps, 配置 20 却出 30)。
    //       fr32DstFrameRate 是真正生效的丢帧字段(对标 rkipc 对分数 fps 的用法)。
    int venc_src_num = (sensor_fps > 0) ? sensor_fps : fps_num;
    int venc_src_den = 1;
    int venc_dst_num = fps_num;
    int venc_dst_den = (fps_den > 0) ? fps_den : 1;

    if (codec == RK_VIDEO_ID_AVC) {
        // H264: GOP + 帧率 (CBR/VBR 字段布局相同, base 通用)
        pRcAttr->stH264Cbr.u32Gop = gop;
        pRcAttr->stH264Cbr.u32SrcFrameRateNum = venc_src_num;
        pRcAttr->stH264Cbr.u32SrcFrameRateDen = venc_src_den;
        pRcAttr->stH264Cbr.fr32DstFrameRateNum = venc_dst_num;
        pRcAttr->stH264Cbr.fr32DstFrameRateDen = venc_dst_den;
        if (is_vbr) {
            pRcAttr->stH264Vbr.u32BitRate = bitrate_kbps;
            pRcAttr->stH264Vbr.u32MaxBitRate = bitrate_kbps * 3 / 2;
            pRcAttr->stH264Vbr.u32MinBitRate = bitrate_kbps / 2;
        } else {
            pRcAttr->stH264Cbr.u32BitRate = bitrate_kbps;
        }
    } else {
        // H265: GOP + 帧率
        pRcAttr->stH265Cbr.u32Gop = gop;
        pRcAttr->stH265Cbr.u32SrcFrameRateNum = venc_src_num;
        pRcAttr->stH265Cbr.u32SrcFrameRateDen = venc_src_den;
        pRcAttr->stH265Cbr.fr32DstFrameRateNum = venc_dst_num;
        pRcAttr->stH265Cbr.fr32DstFrameRateDen = venc_dst_den;
        if (is_vbr) {
            pRcAttr->stH265Vbr.u32BitRate = bitrate_kbps;
            pRcAttr->stH265Vbr.u32MaxBitRate = bitrate_kbps * 3 / 2;
            pRcAttr->stH265Vbr.u32MinBitRate = bitrate_kbps / 2;
        } else {
            pRcAttr->stH265Cbr.u32BitRate = bitrate_kbps;
        }
    }

    // GOP 属性
    stAttr.stGopAttr.enGopMode = gop_mode;
    stAttr.stGopAttr.s32VirIdrLen = smartp_viridrlen;

    int ret = RK_MPI_VENC_CreateChn(chn_id, &stAttr);
    if (ret != RK_SUCCESS) return ret;

    // 设置 RC quality (对标 rkipc video.c)
    if (rc_quality >= 0) {
        VENC_RC_PARAM_S rc_param;
        memset(&rc_param, 0, sizeof(rc_param));
        RK_MPI_VENC_GetRcParam(chn_id, &rc_param);
        // quality 0(lowest)..6(highest) → minQp 40..10 (每档差 5)
        int min_qp = 40 - rc_quality * 5;
        if (codec == RK_VIDEO_ID_AVC) {
            rc_param.stParamH264.u32MinQp = min_qp;
        } else {
            rc_param.stParamH265.u32MinQp = min_qp;
        }
        RK_MPI_VENC_SetRcParam(chn_id, &rc_param);
        printf("[rk_camera] chn[%d] rc_quality=%d -> u32MinQp=%d\n",
               chn_id, rc_quality, min_qp);
    }

    VENC_RECV_PIC_PARAM_S stRecvParam;
    memset(&stRecvParam, 0, sizeof(stRecvParam));
    stRecvParam.s32RecvPicNum = -1;
    ret = RK_MPI_VENC_StartRecvFrame(chn_id, &stRecvParam);
    return ret;
}

// ---- VENC 取流线程 (每个通道一个) ----

static void *get_stream_thread(void *arg) {
    int chn_id = (int)(intptr_t)arg;
    VENC_STREAM_S stFrame;
    stFrame.pstPack = (VENC_PACK_S *)malloc(sizeof(VENC_PACK_S));

    printf("[rk_camera] stream thread[%d] started\n", chn_id);

    while (!g_quit) {
        int ret = RK_MPI_VENC_GetStream(chn_id, &stFrame, -1);
        if (ret == RK_SUCCESS) {
            void *pData = RK_MPI_MB_Handle2VirAddr(stFrame.pstPack->pMbBlk);
            uint32_t u32Len = stFrame.pstPack->u32Len;
            uint64_t u64PTS = stFrame.pstPack->u64PTS;

            // 注意: 不再在此扫描 NAL 判断关键帧(cam 侧不计算, 省 RV1106 每帧 CPU),
            // 关键帧判定改由 viewer 侧字节扫描完成。回调只透传裸数据与 PTS。
            if (g_callback && pData && u32Len > 0) {
                g_callback(chn_id, (const uint8_t *)pData, u32Len, u64PTS);
            }

            RK_MPI_VENC_ReleaseStream(chn_id, &stFrame);
        } else {
            usleep(10 * 1000);
        }
    }

    free(stFrame.pstPack);
    return NULL;
}

// ---- 绑定管线: VI → VPSS → 3× VENC ----

static int bind_vi_to_vpss() {
    MPP_CHN_S stSrcChn, stDestChn;
    stSrcChn.enModId = RK_ID_VI;
    stSrcChn.s32DevId = VI_DEV_ID;
    stSrcChn.s32ChnId = VI_CHN_ID;
    stDestChn.enModId = RK_ID_VPSS;
    stDestChn.s32DevId = VPSS_GRP_ID;
    stDestChn.s32ChnId = 0;  // VPSS group 绑定用 devId=grp, chnId=0
    return RK_MPI_SYS_Bind(&stSrcChn, &stDestChn);
}

static int bind_vpss_to_venc(int vpss_chn, int venc_chn) {
    MPP_CHN_S stSrcChn, stDestChn;
    stSrcChn.enModId = RK_ID_VPSS;
    stSrcChn.s32DevId = VPSS_GRP_ID;
    stSrcChn.s32ChnId = vpss_chn;
    stDestChn.enModId = RK_ID_VENC;
    stDestChn.s32DevId = 0;
    stDestChn.s32ChnId = venc_chn;
    return RK_MPI_SYS_Bind(&stSrcChn, &stDestChn);
}




// ---- LCD 显示后端 B: VO 模块 (VOP 硬件 CSC, 零 CPU 转换) ----
// 官方示例参考:
//   RV1106_Linux_SDK/media/samples/simple_test/simple_vi_get_frame_send_vo_rv1106.c
// 管线: VI selfpath(chn1) --RK_MPI_VI_GetChnFrame--> RK_MPI_VO_SendFrame --> VOP(显示控制器)
// ---- LCD 显示代码已迁移至 lcd_preview.c 模块 (见 lcd_preview.h) ----
// 模块自管理: g_lcd_use_vo / g_lcd_vo_layer / g_lcd_vo_dev / g_lcd_vo_chn /
// g_lcd_vi_selfpath_chn / g_lcd_width / g_lcd_height / g_lcd_fps / g_sensor_fps /
// g_lcd_thread / g_lcd_quit 均已移入模块, 由 lcd_preview_start/stop 控制。

// lcd_vo_init / lcd_vo_deinit / lcd_vo_thread 已迁移至 lcd_preview.c
// (见 lcd_preview.h 的 lcd_preview_start / lcd_preview_stop)。




// ---- 公开 API ----

// rk_camera_init: 三码流模式初始化
// 参数 main_w/h 仅用于 VI 捕获分辨率 (必须 >= 所有码流的最大分辨率)
// 每个码流的具体参数通过 rk_camera_set_chn_config 提前设置
int rk_camera_init(int main_w, int main_h, int fps, int bitrate, int sensor_fps) {
    if (g_initialized) return 0;

    // C 打印缓冲策略 (device-cam 经 run_device_cam.sh 的 `tee` 同时打到串口 115200≈11KB/s 和文件):
    // - 不能 _IONBF: 每条 printf 同步 write() 到慢串口会冲爆管道→会话硬锁死 (已踩坑, 见上轮)。
    // - 也不能用默认 4096B 全缓冲: 低频 C 打印要攒满 4KB 才刷 => "满屏才输出"。
    // - 折中: 用 512B 小全缓冲。C 输出每满 512B 即经 tee 刷一次, 既及时可见, 又批量写不压垮串口。
    //   (Rust 侧 println! 本就按行刷不受影响; C 无每帧打印, 不会形成洪流。LCD_STAT 仍单独 fflush。)
    //   注: glibc 在管道下会忽略 _IOLBF, 故用小块 _IOFBF 才是可预期的行为。
    static char s_stdout_buf[512];
    setvbuf(stdout, s_stdout_buf, _IOFBF, sizeof(s_stdout_buf));

    if (sensor_fps <= 0) sensor_fps = fps;  // 回退: sensor_fps 未知时使用 target fps

    // [FIX] 必须在 VI/VPSS/VENC 初始化之前恢复 INI 持久化参数。
    // 否则: toml 解析的 main 分辨率(如 1920x1080)已用于初始化 VI/VPSS, 之后
    // rk_video_set_from_ini 又把 g_chn_attr[0] 覆盖为 INI 值(如 2304x1296), 导致
    // VPSS 主通道输出(1920x1080) 与 VENC chn0 请求分辨率(2304x1296) 不一致,
    // 该码流 VENC 产出 0 帧 → cam 端不发数据 (viewer 卡黑屏)。
    // 根因: 本次引入的 INI 持久化 (set_encoder_config 写盘 + 此处恢复)。
    rk_video_set_from_ini();

    // VI/VPSS 主分辨率采用 INI 覆盖后的 main 通道实际分辨率(回退到入参 main_w/h)。
    int main_res_w = (g_chn_enabled[VENC_CHN_MAIN] && g_chn_attr[VENC_CHN_MAIN].width > 0)
                         ? g_chn_attr[VENC_CHN_MAIN].width : main_w;
    int main_res_h = (g_chn_enabled[VENC_CHN_MAIN] && g_chn_attr[VENC_CHN_MAIN].height > 0)
                         ? g_chn_attr[VENC_CHN_MAIN].height : main_h;

    printf("[rk_camera] init: VI=%dx%d @%dfps sensor_fps=%d\n", main_res_w, main_res_h, fps, sensor_fps);

    if (ensure_sys_init() != 0) return -1;

    isp_init();

    // 1. VI 初始化 (捕获主码流分辨率)
    // VI 全速运行在 sensor 原生帧率; 各码流的目标帧率由各自 VENC 的 fr32DstFrameRate 丢帧实现
    // (对标 rkipc: 单 VI 共享时无法按单路控速, 改由 VENC 控速, 见 venc_init_single)。
    int ret = vi_dev_init();
    if (ret != 0) { printf("[rk_camera] vi_dev_init failed\n"); return -1; }
    ret = vi_chn_init(main_res_w, main_res_h, sensor_fps, sensor_fps);
    if (ret != 0) { printf("[rk_camera] vi_chn_init failed\n"); return -1; }

    // 2. VPSS 初始化 — 从 g_chn_attr 读取每通道的分辨率
    {
        int sub_w = g_chn_enabled[VENC_CHN_SUB] ? g_chn_attr[VENC_CHN_SUB].width : 704;
        int sub_h = g_chn_enabled[VENC_CHN_SUB] ? g_chn_attr[VENC_CHN_SUB].height : 576;
        int third_w = g_chn_enabled[VENC_CHN_THIRD] ? g_chn_attr[VENC_CHN_THIRD].width : 960;
        int third_h = g_chn_enabled[VENC_CHN_THIRD] ? g_chn_attr[VENC_CHN_THIRD].height : 540;

        printf("[rk_camera] VPSS: main=%dx%d sub=%dx%d third=%dx%d\n",
               main_res_w, main_res_h, sub_w, sub_h, third_w, third_h);

        ret = vpss_init(main_res_w, main_res_h, sub_w, sub_h, third_w, third_h);
        if (ret != 0) { printf("[rk_camera] vpss_init failed\n"); return -1; }
    }

    // 3. 绑定 VI → VPSS
    ret = bind_vi_to_vpss();
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] bind VI->VPSS failed: %x\n", ret);
        return -1;
    }

    // 4. 初始化已启用的 VENC 通道并绑定 (从 g_chn_attr 读取全部参数)
    int venc_chns[] = {VENC_CHN_MAIN, VENC_CHN_SUB, VENC_CHN_THIRD};
    int vpss_chns[] = {VPSS_CHN_MAIN, VPSS_CHN_SUB, VPSS_CHN_THIRD};
    const char *names[] = {"main", "sub", "third"};

    for (int i = 0; i < VENC_MAX_CHN; i++) {
        if (!g_chn_enabled[i]) {
            printf("[rk_camera] %s stream disabled\n", names[i]);
            continue;
        }

        ChnEncAttr *a = &g_chn_attr[i];
        int ch_fps  = a->dst_fps_den > 0 ? a->dst_fps_num / a->dst_fps_den : 25;
        int ch_gop  = a->gop > 0 ? a->gop : ch_fps * 2;
        int ch_br   = a->bitrate_kbps > 0 ? a->bitrate_kbps : bitrate;

        printf("[rk_camera] %s stream: VENC_chn=%d %dx%d @%dfps %dkbps gop=%d\n",
               names[i], venc_chns[i], a->width, a->height,
               ch_fps, ch_br, ch_gop);

        ret = venc_init_single(
            venc_chns[i],
            a->width, a->height,
            a->dst_fps_num, a->dst_fps_den > 0 ? a->dst_fps_den : 1,
            sensor_fps,
            ch_br, ch_gop,
            a->codec ? a->codec : RK_VIDEO_ID_HEVC,
            a->rc_mode,
            a->rc_quality,
            a->profile,
            a->gop_mode,
            a->mirror,
            a->smartp_viridrlen,
            a->stream_buf_cnt > 0 ? a->stream_buf_cnt : 2
        );
        if (ret != RK_SUCCESS) {
            printf("[rk_camera] venc_init[%s] failed: %x\n", names[i], ret);
            return -1;
        }

        // 绑定 VPSS channel → VENC channel
        ret = bind_vpss_to_venc(vpss_chns[i], venc_chns[i]);
        if (ret != RK_SUCCESS) {
            printf("[rk_camera] bind VPSS->VENC[%s] failed: %x\n", names[i], ret);
            return -1;
        }

        // 启动取流线程
        g_quit = 0;
        ret = pthread_create(&g_stream_threads[i], NULL, get_stream_thread,
                            (void *)(intptr_t)venc_chns[i]);
        if (ret != 0) return -1;
    }

    // 5. LCD 显示 — 交由 lcd_preview 模块 (VOP 硬件 CSC, 零 CPU, 不碰 RGA 单例)
    //    模块自管理 selfpath VI 通道 + VO video plane + 送帧线程 (见 lcd_preview.h)
    if (g_fb_enabled) {
        if (lcd_preview_start() == 0) {
            printf("[rk_camera] LCD display started (VO hardware CSC)\n");
        } else {
            printf("[rk_camera] LCD preview start failed, LCD disabled\n");
            g_fb_enabled = 0;
        }
    }

    // 从 INI 恢复上次持久化的 ISP 图像参数 (控制通道 set 时写入)
    rk_isp_set_from_ini(0);

    g_initialized = 1;
    printf("[rk_camera] initialized, %d stream threads started%s\n",
           (g_chn_enabled[0]?1:0) + (g_chn_enabled[1]?1:0) + (g_chn_enabled[2]?1:0),
           g_fb_enabled ? " + LCD" : "");
    return 0;
}

// 设置单个编码通道的扩展参数 (在 rk_camera_init 之前调用)
// chn_id: 0=main, 1=sub, 2=third
void rk_camera_set_chn_config(int chn_id,
                               const char *codec,      // "H265" or "H264"
                               int width, int height,
                               int src_fps_num, int src_fps_den,
                               int dst_fps_num, int dst_fps_den,
                               int bitrate_kbps,
                               const char *rc_mode,     // "CBR" or "VBR"
                               const char *rc_quality,  // "highest"..."lowest"
                               int gop,
                               const char *gop_mode,    // "normalP" or "smartP"
                               const char *h264_profile, // "baseline"/"main"/"high"
                               int smartp_viridrlen,
                               int stream_buf_cnt,
                               const char *mirror       // "none"/"horizontal"/"vertical"/"both"
) {
    if (chn_id < 0 || chn_id >= VENC_MAX_CHN) return;

    int codec_type = get_codec_type(codec);

    // 存储分辨率/帧率/码率基本参数
    g_chn_attr[chn_id].width = width;
    g_chn_attr[chn_id].height = height;
    g_chn_attr[chn_id].src_fps_num = src_fps_num;
    g_chn_attr[chn_id].src_fps_den = src_fps_den;
    g_chn_attr[chn_id].dst_fps_num = dst_fps_num;
    g_chn_attr[chn_id].dst_fps_den = dst_fps_den;
    g_chn_attr[chn_id].bitrate_kbps = bitrate_kbps;
    g_chn_attr[chn_id].gop = gop;
    // 编码扩展参数
    g_chn_attr[chn_id].codec = codec_type;
    g_chn_attr[chn_id].rc_mode = get_rc_mode(rc_mode, codec_type);
    g_chn_attr[chn_id].rc_quality = get_rc_quality(rc_quality);
    g_chn_attr[chn_id].profile = (codec_type == RK_VIDEO_ID_AVC)
                                  ? get_profile(h264_profile) : 0;
    g_chn_attr[chn_id].gop_mode = get_gop_mode(gop_mode);
    g_chn_attr[chn_id].mirror = get_mirror(mirror);
    g_chn_attr[chn_id].smartp_viridrlen = smartp_viridrlen;
    g_chn_attr[chn_id].stream_buf_cnt = stream_buf_cnt;

    g_chn_enabled[chn_id] = 1;

    printf("[rk_camera] chn[%d] config: %s %dx%d %d/%dfps %dkbps rc=%s q=%s gop=%d gop_mode=%s\n",
           chn_id, codec, width, height, dst_fps_num, dst_fps_den,
           bitrate_kbps, rc_mode, rc_quality, gop, gop_mode);
    if (codec_type == RK_VIDEO_ID_AVC) {
        printf("[rk_camera] chn[%d] h264_profile=%s\n", chn_id, h264_profile);
    }
}

// 设置 LCD 参数 (在 rk_camera_init 之前调用)
// width/height: LCD 分辨率 (0 表示使用默认分辨率)
// fps: 显示帧率
void rk_camera_set_vo_config(int width, int height, int fps) {
    lcd_preview_set_config(width, height, fps);
    g_fb_enabled = 1;
    printf("[rk_camera] LCD(VO) enabled: %dx%d @%dfps\n", width, height, fps);
}

// 设置 LCD 预览 video plane 子矩形位置 (局部显示)
void rk_camera_set_vo_rect(int x, int y) {
    lcd_preview_set_rect(x, y);
}

// ---- rknn 推理 (独立模块 rknn_infer.c, 复用 LCD selfpath 通道) ----
extern int rknn_infer_init(const char *model_path);
extern int rknn_infer_start(void);
extern void rknn_infer_stop(void);

// 启用 rknn 目标检测: 加载模型 + 启动推理 (消费 lcd_preview 的 selfpath 帧)。
//   须在 rk_camera_init() 之前调用 (与 set_vo_config 同级)。
int rk_camera_enable_rknn(const char *model_path) {
    if (rknn_infer_init(model_path) != 0) {
        printf("[rk_camera] rknn init failed\n");
        return -1;
    }
    if (rknn_infer_start() != 0) {
        printf("[rk_camera] rknn start failed\n");
        return -1;
    }
    printf("[rk_camera] rknn enabled: %s\n", model_path);
    return 0;
}

// ---- preview-only 封装 (供 standalone lcd-preview 使用, 不触发编码/VPSS) ----
// 仅做 SYS_Init + ISP + VI dev 最小初始化后启动 LCD 预览。
int rk_camera_preview_only_init(int w, int h, int fps) {
    if (ensure_sys_init() != 0) return -1;
    isp_init();
    if (vi_dev_init() != 0) { isp_deinit(); maybe_sys_exit(); return -1; }
    lcd_preview_set_config(w, h, fps);
    return lcd_preview_start();
}

void rk_camera_preview_only_deinit(void) {
    lcd_preview_stop();
    RK_MPI_VI_DisableDev(VI_DEV_ID);
    isp_deinit();
    maybe_sys_exit();
}

// 设置帧回调
void rk_camera_set_callback(frame_callback_t cb) {
    g_callback = cb;
}

// 请求特定通道的 IDR 关键帧 (best-effort)
// 对标 rkipc: 官方从不调用 RK_MPI_VENC_RequestIDR, 而是完全依赖 GOP 自然出 IDR
// (ini 中 refresh_time_s 周期刷新)。本函数保留 RequestIDR 作为尽力而为的加速手段,
// 但真正保证 viewer 快速拿到首帧 IDR 的是短 GOP(配置 gop=15, 约 0.75s 一个 IDR)——
// 即使 RequestIDR 在该 SDK 构建下不立即生效, viewer 也只需等一个 GOP 即可。
// 注意: 重试一次无意义(若当前构建不支持, 立即重试同样无效), 故只调一次。
int rk_camera_request_idr(int chn_id) {
    if (chn_id < 0 || chn_id >= VENC_MAX_CHN) return -1;
    return RK_MPI_VENC_RequestIDR(chn_id, RK_TRUE);
}

// 请求所有通道的 IDR (向后兼容)
int rk_camera_request_idr_all() {
    int ret = 0;
    for (int i = 0; i < VENC_MAX_CHN; i++) {
        if (g_chn_enabled[i]) {
            ret |= RK_MPI_VENC_RequestIDR(i, RK_TRUE);
        }
    }
    return ret;
}

// 热更新单个编码通道参数 (对标 rkipc video.c 运行时改参)
// 内部: StopRecvFrame; 若分辨率/codec 变更则 DestroyChn+venc_init_single 重建,
//       否则 GetChnAttr+SetChnAttr 改码率/GOP/帧率; 最后 StartRecvFrame 恢复取流。
// SDK 约束 (mpi.txt): RK_MPI_VENC_StartRecvFrame 之后成员不可改, 必须先 StopRecvFrame 才能 SetChnAttr。
// 短暂停顿(丢若干帧)。参数中 NULL/<0 表示沿用当前 g_chn_attr 中的值。
int rk_camera_update_chn_config(int chn_id,
                                const char *codec,
                                int width, int height,
                                int dst_fps_num, int dst_fps_den,
                                int bitrate_kbps,
                                const char *rc_mode,
                                const char *rc_quality,
                                int gop,
                                const char *gop_mode,
                                const char *h264_profile) {
    if (chn_id < 0 || chn_id >= VENC_MAX_CHN) return -1;
    if (!g_chn_enabled[chn_id]) return -1;

    ChnEncAttr *old = &g_chn_attr[chn_id];
    int sensor_fps = lcd_preview_get_sensor_fps();

    int new_codec = (codec && codec[0]) ? get_codec_type(codec) : old->codec;
    int new_w = width > 0 ? width : old->width;
    int new_h = height > 0 ? height : old->height;
    int new_dst_num = dst_fps_num > 0 ? dst_fps_num : old->dst_fps_num;
    int new_dst_den = dst_fps_den > 0 ? dst_fps_den : old->dst_fps_den;
    int new_br = bitrate_kbps > 0 ? bitrate_kbps : old->bitrate_kbps;
    int new_rc_mode = (rc_mode && rc_mode[0]) ? get_rc_mode(rc_mode, new_codec) : old->rc_mode;
    int new_rc_quality = (rc_quality && rc_quality[0]) ? get_rc_quality(rc_quality) : old->rc_quality;
    int new_gop = gop > 0 ? gop : old->gop;
    int new_gop_mode = (gop_mode && gop_mode[0]) ? get_gop_mode(gop_mode) : old->gop_mode;
    int new_profile = (h264_profile && h264_profile[0]) ? get_profile(h264_profile) : old->profile;
    if (new_codec != RK_VIDEO_ID_AVC) new_profile = 0;

    // 沿用当前未暴露字段 (mirror/smartp_viridrlen/stream_buf_cnt)
    int new_smartp = old->smartp_viridrlen;
    int new_stream_buf = old->stream_buf_cnt > 0 ? old->stream_buf_cnt : 2;
    int new_mirror = old->mirror;

    int need_rebuild = (new_codec != old->codec) || (new_w != old->width) || (new_h != old->height);

    // 写回全局记录
    old->codec = new_codec;
    old->width = new_w;
    old->height = new_h;
    old->dst_fps_num = new_dst_num;
    old->dst_fps_den = new_dst_den;
    old->bitrate_kbps = new_br;
    old->rc_mode = new_rc_mode;
    old->rc_quality = new_rc_quality;
    old->gop = new_gop;
    old->gop_mode = new_gop_mode;
    old->profile = new_profile;
    old->smartp_viridrlen = new_smartp;
    old->stream_buf_cnt = new_stream_buf;
    old->mirror = new_mirror;

    RK_MPI_VENC_StopRecvFrame(chn_id);

    int ret = 0;
    if (need_rebuild) {
        RK_MPI_VENC_DestroyChn(chn_id);
        ret = venc_init_single(chn_id, new_w, new_h, new_dst_num, new_dst_den, sensor_fps,
                               new_br, new_gop, new_codec, new_rc_mode, new_rc_quality,
                               new_profile, new_gop_mode, new_mirror, new_smartp, new_stream_buf);
        printf("[rk_camera] chn[%d] hot-updated (rebuilt): %dx%d @%d/%dfps %dkbps gop=%d rc=%s q=%d\n",
               chn_id, new_w, new_h, new_dst_num, new_dst_den, new_br, new_gop,
               rc_mode ? rc_mode : "-", new_rc_quality);
        return ret;
    }

    // 非重建: GetChnAttr + SetChnAttr 热改码率/GOP/帧率 (StartRecvFrame 之后成员已锁定)
    VENC_CHN_ATTR_S stAttr;
    memset(&stAttr, 0, sizeof(stAttr));
    if (RK_MPI_VENC_GetChnAttr(chn_id, &stAttr) == RK_SUCCESS) {
        VENC_RC_ATTR_S *pRcAttr = &stAttr.stRcAttr;
        int is_vbr = (new_rc_mode == VENC_RC_MODE_H264VBR || new_rc_mode == VENC_RC_MODE_H265VBR);
        int venc_src_num = (sensor_fps > 0) ? sensor_fps : new_dst_num;
        int venc_src_den = 1;
        int venc_dst_num = new_dst_num;
        int venc_dst_den = (new_dst_den > 0) ? new_dst_den : 1;
        pRcAttr->enRcMode = new_rc_mode;
        if (new_codec == RK_VIDEO_ID_AVC) {
            pRcAttr->stH264Cbr.u32Gop = new_gop;
            pRcAttr->stH264Cbr.u32SrcFrameRateNum = venc_src_num;
            pRcAttr->stH264Cbr.u32SrcFrameRateDen = venc_src_den;
            pRcAttr->stH264Cbr.fr32DstFrameRateNum = venc_dst_num;
            pRcAttr->stH264Cbr.fr32DstFrameRateDen = venc_dst_den;
            if (is_vbr) {
                pRcAttr->stH264Vbr.u32BitRate = new_br;
                pRcAttr->stH264Vbr.u32MaxBitRate = new_br * 3 / 2;
                pRcAttr->stH264Vbr.u32MinBitRate = new_br / 2;
            } else {
                pRcAttr->stH264Cbr.u32BitRate = new_br;
            }
        } else {
            pRcAttr->stH265Cbr.u32Gop = new_gop;
            pRcAttr->stH265Cbr.u32SrcFrameRateNum = venc_src_num;
            pRcAttr->stH265Cbr.u32SrcFrameRateDen = venc_src_den;
            pRcAttr->stH265Cbr.fr32DstFrameRateNum = venc_dst_num;
            pRcAttr->stH265Cbr.fr32DstFrameRateDen = venc_dst_den;
            if (is_vbr) {
                pRcAttr->stH265Vbr.u32BitRate = new_br;
                pRcAttr->stH265Vbr.u32MaxBitRate = new_br * 3 / 2;
                pRcAttr->stH265Vbr.u32MinBitRate = new_br / 2;
            } else {
                pRcAttr->stH265Cbr.u32BitRate = new_br;
            }
        }
        stAttr.stGopAttr.enGopMode = new_gop_mode;
        stAttr.stGopAttr.s32VirIdrLen = new_smartp;

        if (RK_MPI_VENC_SetChnAttr(chn_id, &stAttr) == RK_SUCCESS) {
            // rc_quality 为动态属性, 可独立 SetRcParam
            if (new_rc_quality >= 0) {
                VENC_RC_PARAM_S rc_param;
                memset(&rc_param, 0, sizeof(rc_param));
                RK_MPI_VENC_GetRcParam(chn_id, &rc_param);
                int min_qp = 40 - new_rc_quality * 5;
                if (new_codec == RK_VIDEO_ID_AVC) rc_param.stParamH264.u32MinQp = min_qp;
                else rc_param.stParamH265.u32MinQp = min_qp;
                RK_MPI_VENC_SetRcParam(chn_id, &rc_param);
            }
            VENC_RECV_PIC_PARAM_S stRecvParam;
            memset(&stRecvParam, 0, sizeof(stRecvParam));
            stRecvParam.s32RecvPicNum = -1;
            RK_MPI_VENC_StartRecvFrame(chn_id, &stRecvParam);
            printf("[rk_camera] chn[%d] hot-updated (no rebuild): %dx%d @%d/%dfps %dkbps gop=%d rc=%s q=%d\n",
                   chn_id, new_w, new_h, new_dst_num, new_dst_den, new_br, new_gop,
                   rc_mode ? rc_mode : "-", new_rc_quality);
            return 0;
        }
    }

    // 回退: SetChnAttr 失败, 重建通道
    RK_MPI_VENC_DestroyChn(chn_id);
    ret = venc_init_single(chn_id, new_w, new_h, new_dst_num, new_dst_den, sensor_fps,
                           new_br, new_gop, new_codec, new_rc_mode, new_rc_quality,
                           new_profile, new_gop_mode, new_mirror, new_smartp, new_stream_buf);
    printf("[rk_camera] chn[%d] hot-updated (rebuilt fallback): %dx%d @%d/%dfps %dkbps gop=%d rc=%s q=%d\n",
           chn_id, new_w, new_h, new_dst_num, new_dst_den, new_br, new_gop,
           rc_mode ? rc_mode : "-", new_rc_quality);
    return ret;
}

// 读取当前运行时真实生效的编码参数 (从 g_chn_attr, 而非 INI)。
// g_chn_attr 在 init 时由 toml 填充, 再由 rk_video_set_from_ini 覆盖(set 过的通道),
// 最后由 rk_camera_update_chn_config 热改回写 —— 始终为真实生效配置。
// GetEncoderConfig 必须用此函数, 否则从未被 set 过的 sub/third 在 INI 中不存在,
// 从 INI 读会回退到固定默认值, 导致三个码流读出来"都一样"。
// 返回 0 成功; -1 通道未启用/无效。
int rk_video_get_config(int chn_id,
                        char *codec_buf, int codec_buf_len,
                        int *width, int *height,
                        int *dst_fps_num, int *dst_fps_den,
                        int *bitrate_kbps,
                        int *gop,
                        char *rc_mode_buf, int rc_mode_buf_len,
                        char *rc_quality_buf, int rc_quality_buf_len,
                        char *gop_mode_buf, int gop_mode_buf_len,
                        char *h264_profile_buf, int h264_profile_buf_len,
                        char *smart_buf, int smart_buf_len,
                        int *rotation) {
    if (chn_id < 0 || chn_id >= VENC_MAX_CHN) return -1;
    if (!g_chn_enabled[chn_id]) return -1;
    ChnEncAttr *a = &g_chn_attr[chn_id];

    const char *codec_str = (a->codec == RK_VIDEO_ID_AVC) ? "H.264" : "H.265";
    snprintf(codec_buf, codec_buf_len, "%s", codec_str);

    *width = a->width;
    *height = a->height;
    *dst_fps_num = a->dst_fps_num > 0 ? a->dst_fps_num : 1;
    *dst_fps_den = a->dst_fps_den > 0 ? a->dst_fps_den : 1;
    *bitrate_kbps = a->bitrate_kbps;
    *gop = a->gop;

    int is_vbr = (a->rc_mode == VENC_RC_MODE_H264VBR || a->rc_mode == VENC_RC_MODE_H265VBR);
    snprintf(rc_mode_buf, rc_mode_buf_len, "%s", is_vbr ? "VBR" : "CBR");

    static const char *rc_q_str[] = {"lowest","lower","low","medium","high","higher","highest"};
    int qi = a->rc_quality;
    if (qi < 0) qi = 4;
    if (qi > 6) qi = 6;
    snprintf(rc_quality_buf, rc_quality_buf_len, "%s", rc_q_str[qi]);

    snprintf(gop_mode_buf, gop_mode_buf_len, "%s",
             (a->gop_mode == VENC_GOPMODE_SMARTP) ? "smartP" : "normalP");

    // h264_profile: 仅 AVC 有意义; HEVC 固定 high (与 set 路径默认一致)
    if (a->codec == RK_VIDEO_ID_AVC) {
        const char *p = (a->profile == 100) ? "high" : (a->profile == 0 ? "main" : "baseline");
        snprintf(h264_profile_buf, h264_profile_buf_len, "%s", p);
    } else {
        snprintf(h264_profile_buf, h264_profile_buf_len, "%s", "high");
    }

    // smart/rotation 当前未接入运行时 (g_chn_attr 不保存), 返回与 set 路径一致的默认值
    snprintf(smart_buf, smart_buf_len, "%s", "close");
    *rotation = 0;

    return 0;
}

// rk_param_* 定义在文件末尾, 此处前向声明供本函数调用 (避免隐式声明)
int rk_param_get_int(const char *key, int default_value);
char *rk_param_get_string(const char *key, const char *default_value);

// 从 INI 恢复 encoder 参数 (对标 rkipc video.c rk_video_set_from_ini)
// 在 VENC 初始化前调用, 用上次持久化的值覆盖 g_chn_attr。
// 读取的 key 与 rk_video_source.rs::set_encoder_config 写入的 INI key 完全一致。
static void rk_video_set_from_ini(void) {
    const char *prefix[VENC_MAX_CHN] = {"video.0", "video.1", "video.2"};
    char key[64];
    int v;
    const char *s;

    for (int i = 0; i < VENC_MAX_CHN; i++) {
        if (!g_chn_enabled[i]) continue;
        ChnEncAttr *a = &g_chn_attr[i];

        snprintf(key, sizeof(key), "%s.width", prefix[i]);
        v = rk_param_get_int(key, -1); if (v > 0) a->width = v;
        snprintf(key, sizeof(key), "%s.height", prefix[i]);
        v = rk_param_get_int(key, -1); if (v > 0) a->height = v;
        snprintf(key, sizeof(key), "%s.dst_frame_rate_num", prefix[i]);
        v = rk_param_get_int(key, -1); if (v > 0) a->dst_fps_num = v;
        snprintf(key, sizeof(key), "%s.dst_frame_rate_den", prefix[i]);
        v = rk_param_get_int(key, -1); if (v > 0) a->dst_fps_den = v;
        snprintf(key, sizeof(key), "%s.max_rate", prefix[i]);
        v = rk_param_get_int(key, -1); if (v > 0) a->bitrate_kbps = v;
        snprintf(key, sizeof(key), "%s.gop", prefix[i]);
        v = rk_param_get_int(key, -1); if (v > 0) a->gop = v;

        snprintf(key, sizeof(key), "%s.output_data_type", prefix[i]);
        s = rk_param_get_string(key, ""); if (s && s[0]) a->codec = get_codec_type(s);
        snprintf(key, sizeof(key), "%s.rc_mode", prefix[i]);
        s = rk_param_get_string(key, ""); if (s && s[0]) a->rc_mode = get_rc_mode(s, a->codec);
        snprintf(key, sizeof(key), "%s.rc_quality", prefix[i]);
        s = rk_param_get_string(key, ""); if (s && s[0]) a->rc_quality = get_rc_quality(s);
        snprintf(key, sizeof(key), "%s.gop_mode", prefix[i]);
        s = rk_param_get_string(key, ""); if (s && s[0]) a->gop_mode = get_gop_mode(s);
        snprintf(key, sizeof(key), "%s.h264_profile", prefix[i]);
        s = rk_param_get_string(key, ""); if (s && s[0]) a->profile = (a->codec == RK_VIDEO_ID_AVC) ? get_profile(s) : 0;

        printf("[rk_camera] chn[%d] restored from INI: %dx%d @%d/%dfps %dkbps gop=%d\n",
               i, a->width, a->height, a->dst_fps_num, a->dst_fps_den, a->bitrate_kbps, a->gop);
    }
}

// 反初始化
void rk_camera_deinit() {
    if (!g_initialized) return;

    g_quit = 1;
    for (int i = 0; i < VENC_MAX_CHN; i++) {
        if (g_chn_enabled[i]) {
            pthread_join(g_stream_threads[i], NULL);
        }
    }

    // 停止 rknn 推理 (注销消费者 + 释放帧源引用)
    rknn_infer_stop();

    // 停止 LCD 预览 (模块自管理线程与 VO 清理)
    lcd_preview_stop();

    // 解绑: VENC ← VPSS ← VI
    for (int i = 0; i < VENC_MAX_CHN; i++) {
        if (g_chn_enabled[i]) {
            MPP_CHN_S src, dest;
            src.enModId = RK_ID_VPSS;
            src.s32DevId = VPSS_GRP_ID;
            src.s32ChnId = (i == 0) ? VPSS_CHN_MAIN :
                           (i == 1) ? VPSS_CHN_SUB : VPSS_CHN_THIRD;
            dest.enModId = RK_ID_VENC;
            dest.s32DevId = 0;
            dest.s32ChnId = i;
            RK_MPI_SYS_UnBind(&src, &dest);
        }
    }

    {
        MPP_CHN_S src, dest;
        src.enModId = RK_ID_VI;
        src.s32DevId = VI_DEV_ID;
        src.s32ChnId = VI_CHN_ID;
        dest.enModId = RK_ID_VPSS;
        dest.s32DevId = VPSS_GRP_ID;
        dest.s32ChnId = 0;
        RK_MPI_SYS_UnBind(&src, &dest);
    }

    for (int i = 0; i < VENC_MAX_CHN; i++) {
        if (g_chn_enabled[i]) {
            RK_MPI_VENC_StopRecvFrame(i);
            RK_MPI_VENC_DestroyChn(i);
        }
    }

    // 停止 VPSS (仅 3 个编码通道, 不再有 LCD 通道)
    for (int i = 0; i < VENC_MAX_CHN; i++) {
        if (g_chn_enabled[i]) {
            RK_MPI_VPSS_DisableChn(VPSS_GRP_ID,
                (i == 0) ? VPSS_CHN_MAIN : (i == 1) ? VPSS_CHN_SUB : VPSS_CHN_THIRD);
        }
    }
    RK_MPI_VPSS_StopGrp(VPSS_GRP_ID);
    RK_MPI_VPSS_DestroyGrp(VPSS_GRP_ID);

    RK_MPI_VI_DisableChn(VI_DEV_ID, VI_CHN_ID);
    RK_MPI_VI_DisableDev(VI_DEV_ID);

    isp_deinit();
    maybe_sys_exit();

    // 重置状态
    for (int i = 0; i < VENC_MAX_CHN; i++) {
        g_chn_enabled[i] = 0;
        memset(&g_chn_attr[i], 0, sizeof(ChnEncAttr));
    }
    g_fb_enabled = 0;

    g_initialized = 0;
    printf("[rk_camera] deinitialized\n");
}

// ============== 音频采集 (AI) + 编码 (AENC) ==============
//
// 管线:
//   PCM 模式:   AI → raw PCM → callback → MediaPacket(audio_pcm)
//   编码模式:   AI → bind → AENC → encoded stream → callback → MediaPacket(audio_g711a/etc)
//
// AENC 支持: G711A / G711U / MP2 (Rockchip HW encoder)

#define AI_DEV_ID   0
#define AI_CHN_ID   0
#define AENC_DEV_ID 0
#define AENC_CHN_ID 0

static pthread_t g_audio_thread;
static volatile int g_audio_quit = 0;
static volatile int g_audio_initialized = 0;
static volatile int g_aenc_enabled = 0;

typedef void (*audio_callback_t)(const uint8_t *data, uint32_t len, uint64_t pts_us);
static audio_callback_t g_audio_callback = NULL;

// ---- AI direct (PCM mode, no encoding) ----

static void *audio_get_stream_thread(void *arg) {
    (void)arg;
    AUDIO_FRAME_S frame;

    printf("[rk_camera] audio PCM thread started\n");

    while (!g_audio_quit) {
        int ret = RK_MPI_AI_GetFrame(AI_DEV_ID, AI_CHN_ID, &frame, RK_NULL, -1);
        if (ret == RK_SUCCESS) {
            void *pData = RK_MPI_MB_Handle2VirAddr(frame.pMbBlk);
            uint32_t u32Len = frame.u32Len;

            if (g_audio_callback && pData && u32Len > 0) {
                g_audio_callback((const uint8_t *)pData, u32Len, 0);
            }

            RK_MPI_AI_ReleaseFrame(AI_DEV_ID, AI_CHN_ID, &frame, RK_NULL);
        } else {
            usleep(10 * 1000);
        }
    }

    return NULL;
}

// ---- AENC stream thread (encoding mode) ----

static void *aenc_get_stream_thread(void *arg) {
    (void)arg;
    AUDIO_STREAM_S stream;

    printf("[rk_camera] audio AENC thread started\n");

    while (!g_audio_quit) {
        int ret = RK_MPI_AENC_GetStream(AENC_CHN_ID, &stream, -1);
        if (ret == RK_SUCCESS) {
            void *pData = RK_MPI_MB_Handle2VirAddr(stream.pMbBlk);
            uint32_t u32Len = stream.u32Len;

            if (g_audio_callback && pData && u32Len > 0) {
                g_audio_callback((const uint8_t *)pData, u32Len, stream.u64TimeStamp);
            }

            RK_MPI_AENC_ReleaseStream(AENC_CHN_ID, &stream);
        } else {
            usleep(10 * 1000);
        }
    }

    return NULL;
}

// ---- Audio init helper: configure AI device ----

static int audio_ai_init(int sample_rate, const char *card_name,
                          int channels, int frame_size, const char *format,
                          int volume) {
    AIO_ATTR_S aiAttr;
    AI_CHN_PARAM_S pstParams;
    int ret;

    memset(&aiAttr, 0, sizeof(AIO_ATTR_S));
    snprintf((char *)aiAttr.u8CardName, sizeof(aiAttr.u8CardName), "%s", card_name);

    aiAttr.soundCard.channels = channels;
    aiAttr.soundCard.sampleRate = sample_rate;

    // Configure bit width from format string
    if (strcmp(format, "U8") == 0) {
        aiAttr.soundCard.bitWidth = AUDIO_BIT_WIDTH_8;
        aiAttr.enBitwidth = AUDIO_BIT_WIDTH_8;
    } else {
        aiAttr.soundCard.bitWidth = AUDIO_BIT_WIDTH_16;
        aiAttr.enBitwidth = AUDIO_BIT_WIDTH_16;
    }

    aiAttr.enSamplerate = (AUDIO_SAMPLE_RATE_E)sample_rate;
    // 声道模式: 根据 channels 动态选择 (对标 rkipc audio.c)
    aiAttr.enSoundmode = (channels == 2) ? AUDIO_SOUND_MODE_STEREO : AUDIO_SOUND_MODE_MONO;
    aiAttr.u32PtNumPerFrm = frame_size;
    aiAttr.u32FrmNum = 4;
    aiAttr.u32EXFlag = 0;
    aiAttr.u32ChnCnt = channels;

    ret = RK_MPI_AI_SetPubAttr(AI_DEV_ID, &aiAttr);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_AI_SetPubAttr failed: %x\n", ret);
        return -1;
    }

    ret = RK_MPI_AI_Enable(AI_DEV_ID);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_AI_Enable failed: %x\n", ret);
        return -1;
    }

    memset(&pstParams, 0, sizeof(AI_CHN_PARAM_S));
    pstParams.s32UsrFrmDepth = 4;
    ret = RK_MPI_AI_SetChnParam(AI_DEV_ID, AI_CHN_ID, &pstParams);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_AI_SetChnParam failed: %x\n", ret);
        return -1;
    }

    ret = RK_MPI_AI_EnableChn(AI_DEV_ID, AI_CHN_ID);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_AI_EnableChn failed: %x\n", ret);
        return -1;
    }

    // 设置硬件音量 (对标 rkipc audio.c: RK_MPI_AI_SetVolume)
    if (volume >= 0 && volume <= 100) {
        ret = RK_MPI_AI_SetVolume(AI_DEV_ID, volume);
        if (ret != RK_SUCCESS) {
            printf("[rk_camera] WARN: RK_MPI_AI_SetVolume(%d) failed: %x\n", volume, ret);
        } else {
            printf("[rk_camera] AI volume set to %d\n", volume);
        }
    }

    // 单声道时设置 TrackMode (对标 rkipc audio.c: RK_MPI_AI_SetTrackMode)
    if (channels == 1) {
        ret = RK_MPI_AI_SetTrackMode(AI_DEV_ID, AUDIO_TRACK_FRONT_LEFT);
        if (ret != RK_SUCCESS) {
            printf("[rk_camera] WARN: RK_MPI_AI_SetTrackMode(FRONT_LEFT) failed: %x\n", ret);
        }
    }

    return 0;
}

// ---- VQE init (Voice Quality Enhancement) ----

static int audio_vqe_init(int sample_rate, int frame_size, const char *vqe_cfg) {
    int ret;
    AI_VQE_CONFIG_S stAiVqeConfig;
    int vqe_gap_ms = 16;

    if (vqe_gap_ms != 16 && vqe_gap_ms != 10) return -1;

    memset(&stAiVqeConfig, 0, sizeof(AI_VQE_CONFIG_S));
    stAiVqeConfig.enCfgMode = AIO_VQE_CONFIG_LOAD_FILE;
    snprintf(stAiVqeConfig.aCfgFile, sizeof(stAiVqeConfig.aCfgFile), "%s", vqe_cfg);
    stAiVqeConfig.s32WorkSampleRate = sample_rate;
    stAiVqeConfig.s32FrameSample = sample_rate * vqe_gap_ms / 1000;

    ret = RK_MPI_AI_SetVqeAttr(AI_DEV_ID, AI_CHN_ID, 0, 0, &stAiVqeConfig);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_AI_SetVqeAttr failed: %x\n", ret);
        return -1;
    }

    ret = RK_MPI_AI_EnableVqe(AI_DEV_ID, AI_CHN_ID);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_AI_EnableVqe failed: %x\n", ret);
        return -1;
    }
    printf("[rk_camera] AIVQE enabled (cfg=%s)\n", vqe_cfg);
    return 0;
}

// ---- Get AENC codec ID from string ----

static RK_CODEC_ID_E get_aenc_codec(const char *encode_type) {
    if (strcmp(encode_type, "G711A") == 0) return RK_AUDIO_ID_PCM_ALAW;
    if (strcmp(encode_type, "G711U") == 0) return RK_AUDIO_ID_PCM_MULAW;
    if (strcmp(encode_type, "MP2") == 0)   return RK_AUDIO_ID_MP2;
    return RK_AUDIO_ID_PCM_ALAW; // default
}

// ---- Public API ----

int rk_audio_init(int sample_rate, const char *card_name, int channels, int frame_size, int volume,
                  const char *encode_type, const char *format, int bit_rate,
                  int enable_vqe, const char *vqe_cfg) {
    if (g_audio_initialized) return 0;

    int use_aenc = (strcmp(encode_type, "PCM") != 0);

    printf("[rk_camera] audio init: %dHz card=%s ch=%d frame=%d vol=%d encode=%s fmt=%s br=%d vqe=%d\n",
           sample_rate, card_name, channels, frame_size, volume,
           encode_type, format, bit_rate, enable_vqe);

    if (ensure_sys_init() != 0) return -1;

    // Step 1: AI device init (common for PCM and AENC modes)
    int ret = audio_ai_init(sample_rate, card_name, channels, frame_size, format, volume);
    if (ret != 0) return -1;

    // Step 2: VQE (if enabled, before AENC binding)
    if (enable_vqe) {
        audio_vqe_init(sample_rate, frame_size, vqe_cfg);
    }

    if (use_aenc) {
        // Step 3: AENC channel init
        AENC_CHN_ATTR_S stAencAttr;
        memset(&stAencAttr, 0, sizeof(stAencAttr));

        RK_CODEC_ID_E codec_id = get_aenc_codec(encode_type);
        stAencAttr.enType = codec_id;
        stAencAttr.stCodecAttr.enType = codec_id;
        stAencAttr.stCodecAttr.u32Channels = channels;
        stAencAttr.stCodecAttr.u32SampleRate = sample_rate;

        if (strcmp(format, "U8") == 0) {
            stAencAttr.stCodecAttr.enBitwidth = AUDIO_BIT_WIDTH_8;
        } else {
            stAencAttr.stCodecAttr.enBitwidth = AUDIO_BIT_WIDTH_16;
        }

        if (bit_rate > 0) {
            stAencAttr.stCodecAttr.u32Bitrate = bit_rate;
        }

        stAencAttr.u32BufCount = 4;

        ret = RK_MPI_AENC_CreateChn(AENC_CHN_ID, &stAencAttr);
        if (ret != RK_SUCCESS) {
            printf("[rk_camera] RK_MPI_AENC_CreateChn failed: %x\n", ret);
            return -1;
        }
        printf("[rk_camera] AENC channel %d created (codec=%s)\n", AENC_CHN_ID, encode_type);

        // Step 4: Bind AI → AENC
        MPP_CHN_S aiChn, aencChn;
        aiChn.enModId = RK_ID_AI;
        aiChn.s32DevId = AI_DEV_ID;
        aiChn.s32ChnId = AI_CHN_ID;
        aencChn.enModId = RK_ID_AENC;
        aencChn.s32DevId = AENC_DEV_ID;
        aencChn.s32ChnId = AENC_CHN_ID;

        ret = RK_MPI_SYS_Bind(&aiChn, &aencChn);
        if (ret != RK_SUCCESS) {
            printf("[rk_camera] RK_MPI_SYS_Bind AI->AENC failed: %x\n", ret);
            RK_MPI_AENC_DestroyChn(AENC_CHN_ID);
            return -1;
        }
        printf("[rk_camera] AI→AENC bound\n");

        // Step 5: Start AENC stream thread
        g_aenc_enabled = 1;
        g_audio_quit = 0;
        ret = pthread_create(&g_audio_thread, NULL, aenc_get_stream_thread, NULL);
        if (ret != 0) return -1;

    } else {
        // PCM mode: start AI stream thread directly
        g_aenc_enabled = 0;
        g_audio_quit = 0;
        ret = pthread_create(&g_audio_thread, NULL, audio_get_stream_thread, NULL);
        if (ret != 0) return -1;
    }

    g_audio_initialized = 1;
    printf("[rk_camera] audio initialized (%s mode)\n", use_aenc ? encode_type : "PCM");
    return 0;
}

void rk_audio_set_callback(audio_callback_t cb) {
    g_audio_callback = cb;
}

void rk_audio_deinit() {
    if (!g_audio_initialized) return;

    g_audio_quit = 1;
    pthread_join(g_audio_thread, NULL);

    if (g_aenc_enabled) {
        // Unbind AI → AENC
        MPP_CHN_S aiChn, aencChn;
        aiChn.enModId = RK_ID_AI;
        aiChn.s32DevId = AI_DEV_ID;
        aiChn.s32ChnId = AI_CHN_ID;
        aencChn.enModId = RK_ID_AENC;
        aencChn.s32DevId = AENC_DEV_ID;
        aencChn.s32ChnId = AENC_CHN_ID;
        RK_MPI_SYS_UnBind(&aiChn, &aencChn);

        RK_MPI_AENC_DestroyChn(AENC_CHN_ID);
        printf("[rk_camera] AENC channel destroyed\n");
    }

    RK_MPI_AI_DisableChn(AI_DEV_ID, AI_CHN_ID);
    RK_MPI_AI_Disable(AI_DEV_ID);

    maybe_sys_exit();

    g_audio_initialized = 0;
    g_aenc_enabled = 0;
    printf("[rk_camera] audio deinitialized\n");
}

// ---- 控制通道: 编码器参数转换 helpers ----
// 参考 rkipc video.c 中 output_data_type / rc_mode / rc_quality / profile / gop_mode 的取值语义

static int get_codec_type(const char *codec_str) {
    // 兼容 rkipc INI 格式 "H.264"/"H.265" 和简化格式 "H264"/"h264"
    if (strcmp(codec_str, "H264") == 0 || strcmp(codec_str, "h264") == 0 ||
        strcmp(codec_str, "H.264") == 0 || strcmp(codec_str, "h.264") == 0)
        return RK_VIDEO_ID_AVC;
    return RK_VIDEO_ID_HEVC;  // default H.265 (对标 rkipc)
}

static int get_rc_mode(const char *rc_str, int codec) {
    if (codec == RK_VIDEO_ID_AVC) {
        if (strcmp(rc_str, "VBR") == 0) return VENC_RC_MODE_H264VBR;
        return VENC_RC_MODE_H264CBR;
    } else {
        if (strcmp(rc_str, "VBR") == 0) return VENC_RC_MODE_H265VBR;
        return VENC_RC_MODE_H265CBR;
    }
}

static int get_rc_quality(const char *q_str) {
    // 对标 rkipc: lowest→0 .. highest→6, 调节 MinQp
    if (strcmp(q_str, "highest") == 0) return 6;
    if (strcmp(q_str, "higher") == 0) return 5;
    if (strcmp(q_str, "high") == 0) return 4;
    if (strcmp(q_str, "medium") == 0) return 3;
    if (strcmp(q_str, "low") == 0) return 2;
    if (strcmp(q_str, "lower") == 0) return 1;
    if (strcmp(q_str, "lowest") == 0) return 0;
    return -1;  // 不设置
}

static int get_profile(const char *p_str) {
    // H.264 profile: high=100, main=77, baseline=66 (对标 rkipc video.c)
    if (strcmp(p_str, "high") == 0) return 100;
    if (strcmp(p_str, "main") == 0) return 77;
    if (strcmp(p_str, "baseline") == 0) return 66;
    return 0;  // default
}

static int get_gop_mode(const char *g_str) {
    if (strcmp(g_str, "smartP") == 0) return VENC_GOPMODE_SMARTP;
    return VENC_GOPMODE_NORMALP;
}

static int get_mirror(const char *m_str) {
    // VENC 层镜像 (对标 rkipc MIRROR 枚举)
    if (strcmp(m_str, "horizontal") == 0) return MIRROR_HORIZONTAL;
    if (strcmp(m_str, "vertical") == 0) return MIRROR_VERTICAL;
    if (strcmp(m_str, "both") == 0) return MIRROR_HORIZONTAL | MIRROR_VERTICAL;
    return MIRROR_NONE;
}


// ---- 控制通道: ISP 图像参数 C shim ----
// 供 Rust FFI 调用，利用已有的 g_aiq_ctx 操作 rkaiq SDK
// 参考 rkipc common/isp/rv1106/isp.c 的 rk_isp_set/get_* 实现

// ACP 参数类型枚举 (对应 acp_attrib_t 的不同字段)
#define RK_CAM_ACP_CONTRAST     0
#define RK_CAM_ACP_BRIGHTNESS   1
#define RK_CAM_ACP_SATURATION   2
#define RK_CAM_ACP_HUE          3

// 设置 ACP 参数 (contrast/brightness/saturation/hue)
// value: 0-100 (对标 rkipc UI 范围), 内部映射到 0-255 (rkaiq SDK: value * 2.55)
int rk_camera_set_acp_param(int param_type, int value) {
    if (!g_aiq_ctx) {
        printf("[rk_camera] ERROR: set_acp_param: ISP not initialized\n");
        return -1;
    }

    acp_attrib_t attrib;
    memset(&attrib, 0, sizeof(attrib));

    if (rk_aiq_user_api2_acp_GetAttrib(g_aiq_ctx, &attrib) != 0) {
        printf("[rk_camera] ERROR: acp_GetAttrib failed\n");
        return -1;
    }

    float scaled = value * 2.55f;
    switch (param_type) {
        case RK_CAM_ACP_CONTRAST:   attrib.contrast = scaled; break;
        case RK_CAM_ACP_BRIGHTNESS: attrib.brightness = scaled; break;
        case RK_CAM_ACP_SATURATION: attrib.saturation = scaled; break;
        case RK_CAM_ACP_HUE:        attrib.hue = scaled; break;
        default:
            printf("[rk_camera] ERROR: unknown acp param type %d\n", param_type);
            return -1;
    }

    if (rk_aiq_user_api2_acp_SetAttrib(g_aiq_ctx, &attrib) != 0) {
        printf("[rk_camera] ERROR: acp_SetAttrib failed\n");
        return -1;
    }

    return 0;
}

// 获取 ACP 参数 (contrast/brightness/saturation/hue)
// 返回值: 0-100 范围 (从 rkaiq SDK 的 0-255 映射回来: value / 2.55)
int rk_camera_get_acp_param(int param_type) {
    if (!g_aiq_ctx) {
        return -1;
    }

    acp_attrib_t attrib;
    memset(&attrib, 0, sizeof(attrib));

    if (rk_aiq_user_api2_acp_GetAttrib(g_aiq_ctx, &attrib) != 0) {
        return -1;
    }

    float value = 0;
    switch (param_type) {
        case RK_CAM_ACP_CONTRAST:   value = attrib.contrast; break;
        case RK_CAM_ACP_BRIGHTNESS: value = attrib.brightness; break;
        case RK_CAM_ACP_SATURATION: value = attrib.saturation; break;
        case RK_CAM_ACP_HUE:        value = attrib.hue; break;
        default: return -1;
    }

    // 映射 0-255 → 0-100
    int result = (int)(value / 2.55f + 0.5f);
    if (result < 0) result = 0;
    if (result > 100) result = 100;
    return result;
}

// 设置锐度 (使用 asharpV33 API, 对标 rkipc rk_isp_set_sharpness)
// value: 0-100 (百分比)
int rk_camera_set_sharpness(int value) {
    if (!g_aiq_ctx) {
        printf("[rk_camera] ERROR: set_sharpness: ISP not initialized\n");
        return -1;
    }

    rk_aiq_sharp_strength_v33_t strength;
    memset(&strength, 0, sizeof(strength));
    strength.sync.sync_mode = RK_AIQ_UAPI_MODE_SYNC;
    strength.percent = value / 100.0f;
    strength.strength_enable = (value > 0) ? 1 : 0;

    if (rk_aiq_user_api2_asharpV33_SetStrength(g_aiq_ctx, &strength) != 0) {
        printf("[rk_camera] ERROR: asharpV33_SetStrength failed\n");
        return -1;
    }

    return 0;
}

// 获取锐度 (从 rkaiq SDK 读取)
int rk_camera_get_sharpness() {
    if (!g_aiq_ctx) {
        return -1;
    }

    rk_aiq_sharp_strength_v33_t strength;
    memset(&strength, 0, sizeof(strength));

    if (rk_aiq_user_api2_asharpV33_GetStrength(g_aiq_ctx, &strength) != 0) {
        return -1;
    }

    // percent 是 0.0-1.0, 映射到 0-100
    int result = (int)(strength.percent * 100.0f + 0.5f);
    if (result < 0) result = 0;
    if (result > 100) result = 100;
    return result;
}

// ---- rk_param_* 前向声明 (定义在文件末尾) ----
int rk_param_get_int(const char *key, int default_value);
int rk_param_set_int(const char *key, int value);
char *rk_param_get_string(const char *key, const char *default_value);
int rk_param_set_string(const char *key, const char *value);

// ============================================================
// ISP 图像参数接口 (rk_isp_*) — 供 rk_video_source.rs 控制通道调用
// cam_id 当前忽略 (单摄像头)
// 对标 rkipc common/isp/rv1106/isp.c:
//   GET 从 INI 读取 (fallback 读 hardware), 保证返回上次 set 的值
//   SET 先写 hardware 再持久化到 INI
// ============================================================

int rk_isp_get_contrast(int cam_id) {
    (void)cam_id;
    int val = rk_param_get_int("isp.0.adjustment:contrast", -1);
    if (val >= 0) return val;
    return rk_camera_get_acp_param(RK_CAM_ACP_CONTRAST);
}
int rk_isp_set_contrast(int cam_id, int value) {
    (void)cam_id;
    int ret = rk_camera_set_acp_param(RK_CAM_ACP_CONTRAST, value);
    rk_param_set_int("isp.0.adjustment:contrast", value);
    return ret;
}
int rk_isp_get_brightness(int cam_id) {
    (void)cam_id;
    int val = rk_param_get_int("isp.0.adjustment:brightness", -1);
    if (val >= 0) return val;
    return rk_camera_get_acp_param(RK_CAM_ACP_BRIGHTNESS);
}
int rk_isp_set_brightness(int cam_id, int value) {
    (void)cam_id;
    int ret = rk_camera_set_acp_param(RK_CAM_ACP_BRIGHTNESS, value);
    rk_param_set_int("isp.0.adjustment:brightness", value);
    return ret;
}
int rk_isp_get_saturation(int cam_id) {
    (void)cam_id;
    int val = rk_param_get_int("isp.0.adjustment:saturation", -1);
    if (val >= 0) return val;
    return rk_camera_get_acp_param(RK_CAM_ACP_SATURATION);
}
int rk_isp_set_saturation(int cam_id, int value) {
    (void)cam_id;
    int ret = rk_camera_set_acp_param(RK_CAM_ACP_SATURATION, value);
    rk_param_set_int("isp.0.adjustment:saturation", value);
    return ret;
}
int rk_isp_get_hue(int cam_id) {
    (void)cam_id;
    int val = rk_param_get_int("isp.0.adjustment:hue", -1);
    if (val >= 0) return val;
    return rk_camera_get_acp_param(RK_CAM_ACP_HUE);
}
int rk_isp_set_hue(int cam_id, int value) {
    (void)cam_id;
    int ret = rk_camera_set_acp_param(RK_CAM_ACP_HUE, value);
    rk_param_set_int("isp.0.adjustment:hue", value);
    return ret;
}
int rk_isp_get_sharpness(int cam_id) {
    (void)cam_id;
    int val = rk_param_get_int("isp.0.adjustment:sharpness", -1);
    if (val >= 0) return val;
    return rk_camera_get_sharpness();
}
int rk_isp_set_sharpness(int cam_id, int value) {
    (void)cam_id;
    int ret = rk_camera_set_sharpness(value);
    rk_param_set_int("isp.0.adjustment:sharpness", value);
    return ret;
}

// ---- ISP 图像翻转 (对标 rkipc rk_isp_set_image_flip) ----
// 使用 rkaiq 内置 mirror/flip API (rk_aiq_uapi2_setMirrorFlip),
// 与 VENC 层 enMirror 独立 (VENC mirror 在编码级裁剪, ISP flip 在 sensor 级翻转)。

int rk_isp_get_image_flip(int cam_id, const char **value) {
    (void)cam_id;
    if (!value) return -1;
    *value = rk_param_get_string("isp.0.video_adjustment:image_flip", "close");
    return 0;
}

int rk_isp_set_image_flip(int cam_id, const char *value) {
    (void)cam_id;
    if (!g_aiq_ctx || !value) return -1;

    int mirror = 0, flip = 0;

    if (strcmp(value, "close") == 0) {
        mirror = 0; flip = 0;
    } else if (strcmp(value, "flip") == 0) {
        mirror = 0; flip = 1;
    } else if (strcmp(value, "mirror") == 0) {
        mirror = 1; flip = 0;
    } else if (strcmp(value, "centrosymmetric") == 0) {
        mirror = 1; flip = 1;
    } else {
        printf("[rk_camera] ERROR: unknown image_flip value: %s\n", value);
        return -1;
    }

    // skip 4 frames (对标 rkipc: 等待管线稳定)
    int ret = rk_aiq_uapi2_setMirrorFlip(g_aiq_ctx, mirror, flip, 4);
    if (ret != 0) {
        printf("[rk_camera] ERROR: rk_aiq_uapi2_setMirrorFlip failed: %d\n", ret);
        return -1;
    }

    rk_param_set_string("isp.0.video_adjustment:image_flip", value);
    printf("[rk_camera] image_flip set to %s (mirror=%d flip=%d)\n", value, mirror, flip);
    return 0;
}

// ---- ISP 参数从 INI 恢复 (对标 rkipc rk_isp_set_from_ini) ----
// 在 ISP 初始化后调用, 将上次持久化的 ISP 参数重新应用到硬件。
// 默认值对标 rkipc: contrast/brightness/saturation/sharpness/hue=50, image_flip=close。
int rk_isp_set_from_ini(int cam_id) {
    (void)cam_id;
    if (!g_aiq_ctx) return -1;

    printf("[rk_camera] isp_set_from_ini: restoring ISP parameters from INI\n");

    // image adjustment (默认 50, 对标 rkipc isp.c rk_isp_set_from_ini)
    rk_isp_set_contrast(cam_id,    rk_param_get_int("isp.0.adjustment:contrast", 50));
    rk_isp_set_brightness(cam_id,  rk_param_get_int("isp.0.adjustment:brightness", 50));
    rk_isp_set_saturation(cam_id,  rk_param_get_int("isp.0.adjustment:saturation", 50));
    rk_isp_set_sharpness(cam_id,   rk_param_get_int("isp.0.adjustment:sharpness", 50));
    rk_isp_set_hue(cam_id,         rk_param_get_int("isp.0.adjustment:hue", 50));

    // video_adjustment (image_flip, 默认为 close)
    {
        const char *flip_val = rk_param_get_string("isp.0.video_adjustment:image_flip", "close");
        rk_isp_set_image_flip(cam_id, flip_val);
    }

    printf("[rk_camera] isp_set_from_ini done\n");
    return 0;
}

// ============================================================
// INI 参数持久化 (rk_param_*) — 简易 key=value 文件存储
// 参考 rkipc rk_param_* 接口语义 (common/param/param.c)。
// 控制通道单任务串行调用, get_string 返回静态缓冲区指针
// (Rust 侧调用后立即拷贝, 无需释放)。
// ============================================================

#define RK_PARAM_FILE       "/userdata/device-cam.ini"
#define RK_PARAM_MAX_LINE   512
#define RK_PARAM_MAX_LINES  512

static pthread_mutex_t g_param_lock = PTHREAD_MUTEX_INITIALIZER;

// 读取 key 的原始字符串值到 out; 返回 0=找到, -1=未找到
static int param_read_raw(const char *key, char *out, size_t out_size) {
    FILE *fp = fopen(RK_PARAM_FILE, "r");
    if (!fp) return -1;

    char line[RK_PARAM_MAX_LINE];
    size_t klen = strlen(key);
    int found = -1;
    while (fgets(line, sizeof(line), fp)) {
        char *p = line;
        while (*p == ' ' || *p == '\t') p++;
        if (strncmp(p, key, klen) == 0 && p[klen] == '=') {
            char *val = p + klen + 1;
            char *nl = strpbrk(val, "\r\n");
            if (nl) *nl = '\0';
            strncpy(out, val, out_size - 1);
            out[out_size - 1] = '\0';
            found = 0;
            break;
        }
    }
    fclose(fp);
    return found;
}

// 写入/更新 key=value (整文件重写); 返回 0=成功, -1=失败
static int param_write_raw(const char *key, const char *value) {
    static char buf[RK_PARAM_MAX_LINES][RK_PARAM_MAX_LINE];
    int n = 0;
    size_t klen = strlen(key);
    int replaced = 0;

    FILE *fp = fopen(RK_PARAM_FILE, "r");
    if (fp) {
        while (n < RK_PARAM_MAX_LINES && fgets(buf[n], RK_PARAM_MAX_LINE, fp)) {
            char *p = buf[n];
            while (*p == ' ' || *p == '\t') p++;
            if (!replaced && strncmp(p, key, klen) == 0 && p[klen] == '=') {
                snprintf(buf[n], RK_PARAM_MAX_LINE, "%s=%s\n", key, value);
                replaced = 1;
            }
            n++;
        }
        fclose(fp);
    }
    if (!replaced && n < RK_PARAM_MAX_LINES) {
        snprintf(buf[n], RK_PARAM_MAX_LINE, "%s=%s\n", key, value);
        n++;
    }

    FILE *out = fopen(RK_PARAM_FILE, "w");
    if (!out) {
        printf("[rk_camera] ERROR: cannot write param file %s\n", RK_PARAM_FILE);
        return -1;
    }
    for (int i = 0; i < n; i++) fputs(buf[i], out);
    fflush(out);
    fclose(out);
    sync();
    return 0;
}

int rk_param_get_int(const char *key, int default_value) {
    pthread_mutex_lock(&g_param_lock);
    char val[RK_PARAM_MAX_LINE];
    int ret = default_value;
    if (param_read_raw(key, val, sizeof(val)) == 0) {
        ret = atoi(val);
    }
    pthread_mutex_unlock(&g_param_lock);
    return ret;
}

int rk_param_set_int(const char *key, int value) {
    char val[32];
    snprintf(val, sizeof(val), "%d", value);
    pthread_mutex_lock(&g_param_lock);
    int ret = param_write_raw(key, val);
    pthread_mutex_unlock(&g_param_lock);
    return ret;
}

char *rk_param_get_string(const char *key, const char *default_value) {
    static char s_buf[RK_PARAM_MAX_LINE];
    pthread_mutex_lock(&g_param_lock);
    if (param_read_raw(key, s_buf, sizeof(s_buf)) != 0) {
        strncpy(s_buf, default_value ? default_value : "", sizeof(s_buf) - 1);
        s_buf[sizeof(s_buf) - 1] = '\0';
    }
    pthread_mutex_unlock(&g_param_lock);
    return s_buf;
}

int rk_param_set_string(const char *key, const char *value) {
    pthread_mutex_lock(&g_param_lock);
    int ret = param_write_raw(key, value ? value : "");
    pthread_mutex_unlock(&g_param_lock);
    return ret;
}

// ============================================================
// 系统操作 (rk_system_*)
// 对标 rkipc common/system/system.c
// ============================================================

int rk_system_reboot(void) {
    // 对标 rkipc: sync 后 reboot (rkipc 用 reboot -f)
    sync();
    return system("reboot");
}

int rk_system_factory_reset(void) {
    // 删除持久化配置, 恢复所有默认后重启
    // 对标 rkipc: cp /tmp/rkipc-factory-config.ini → sync → reboot -f
    // 本实现无出厂默认 INI, 直接删除等价于复位到编译期默认值
    remove(RK_PARAM_FILE);
    sync();
    return system("reboot");
}

// ---- Stubs for glibc functions missing in uclibc ----
unsigned long getauxval(unsigned long type) {
    (void)type;
    return 0;
}
