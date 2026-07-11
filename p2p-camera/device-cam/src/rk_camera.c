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

#include "rk_mpi_sys.h"
#include "rk_mpi_vi.h"
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

// ISP (rkaiq) 头文件
#include "rk_aiq_user_api2_sysctl.h"

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

// VPSS 三路输出通道
#define VPSS_CHN_MAIN       VPSS_CHN0
#define VPSS_CHN_SUB        VPSS_CHN1
#define VPSS_CHN_THIRD      VPSS_CHN2

// ---- 全局状态 ----

static volatile int g_quit = 0;
static volatile int g_initialized = 0;
static rk_aiq_sys_ctx_t *g_aiq_ctx = NULL;

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

// 帧回调: fn(chn_id, data, len, pts, is_keyframe)
typedef void (*frame_callback_t)(int chn_id, const uint8_t *data, uint32_t len,
                                  uint64_t pts, int is_keyframe);
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

static int vi_chn_init(int width, int height, int fps) {
    VI_CHN_ATTR_S vi_chn_attr;
    memset(&vi_chn_attr, 0, sizeof(vi_chn_attr));
    vi_chn_attr.stIspOpt.u32BufCount = 3;
    vi_chn_attr.stIspOpt.enMemoryType = VI_V4L2_MEMORY_TYPE_DMABUF;
    vi_chn_attr.stSize.u32Width = width;
    vi_chn_attr.stSize.u32Height = height;
    vi_chn_attr.enPixelFormat = RK_FMT_YUV420SP;
    vi_chn_attr.enCompressMode = COMPRESS_MODE_NONE;
    vi_chn_attr.u32Depth = 0;
    // VI 帧率: src = dst = sensor 帧率, 由 VENC 各通道独立控制编码帧率
    vi_chn_attr.stFrameRate.s32SrcFrameRate = fps;
    vi_chn_attr.stFrameRate.s32DstFrameRate = fps;

    int ret = RK_MPI_VI_SetChnAttr(VI_DEV_ID, VI_CHN_ID, &vi_chn_attr);
    ret |= RK_MPI_VI_EnableChn(VI_DEV_ID, VI_CHN_ID);
    return ret;
}

// ---- VPSS 初始化 (3 路缩放输出) ----

static int vpss_init(int main_w, int main_h,
                     int sub_w, int sub_h,
                     int third_w, int third_h,
                     int fps_num, int fps_den) {
    int ret;
    VPSS_GRP_ATTR_S stGrpAttr;
    VPSS_CHN_ATTR_S stChnAttr;
    VPSS_CHN vpss_chns[] = {VPSS_CHN_MAIN, VPSS_CHN_SUB, VPSS_CHN_THIRD};
    int widths[] = {main_w, sub_w, third_w};
    int heights[] = {main_h, sub_h, third_h};

    memset(&stGrpAttr, 0, sizeof(stGrpAttr));
    stGrpAttr.u32MaxW = 4096;
    stGrpAttr.u32MaxH = 4096;
    stGrpAttr.enPixelFormat = RK_FMT_YUV420SP;
    stGrpAttr.stFrameRate.s32SrcFrameRate = fps_num;
    stGrpAttr.stFrameRate.s32DstFrameRate = fps_num;
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

    // 创建 3 个 VPSS channel
    for (int i = 0; i < 3; i++) {
        memset(&stChnAttr, 0, sizeof(stChnAttr));
        stChnAttr.enChnMode = VPSS_CHN_MODE_USER;
        stChnAttr.enDynamicRange = DYNAMIC_RANGE_SDR8;
        stChnAttr.enPixelFormat = RK_FMT_YUV420SP;
        stChnAttr.stFrameRate.s32SrcFrameRate = fps_num;
        stChnAttr.stFrameRate.s32DstFrameRate = fps_num;
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

    // 启动 VPSS Group
    ret = RK_MPI_VPSS_StartGrp(VPSS_GRP_ID);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_VPSS_StartGrp failed: %x\n", ret);
        return ret;
    }

    printf("[rk_camera] VPSS init: group=%d, outputs: %dx%d / %dx%d / %dx%d\n",
           VPSS_GRP_ID, main_w, main_h, sub_w, sub_h, third_w, third_h);
    return 0;
}

// ---- VENC 单通道初始化 ----

static int get_codec_type(const char *codec_str) {
    if (strcmp(codec_str, "H264") == 0 || strcmp(codec_str, "h264") == 0)
        return RK_VIDEO_ID_AVC;
    return RK_VIDEO_ID_HEVC;  // default H.265
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
    if (strcmp(p_str, "high") == 0) return 100;
    if (strcmp(p_str, "main") == 0) return 77;
    if (strcmp(p_str, "baseline") == 0) return 66;
    return 0;  // default main
}

static int get_gop_mode(const char *g_str) {
    if (strcmp(g_str, "smartP") == 0) return VENC_GOPMODE_SMARTP;
    return VENC_GOPMODE_NORMALP;
}

static int get_mirror(const char *m_str) {
    if (strcmp(m_str, "horizontal") == 0) return MIRROR_HORIZONTAL;
    if (strcmp(m_str, "vertical") == 0) return MIRROR_VERTICAL;
    if (strcmp(m_str, "both") == 0) return MIRROR_HORIZONTAL | MIRROR_VERTICAL;
    return MIRROR_NONE;
}

static int venc_init_single(int chn_id, int width, int height,
                             int fps_num, int fps_den, int bitrate_kbps, int gop,
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

    if (codec == RK_VIDEO_ID_AVC) {
        // H264: GOP + 帧率 (CBR/VBR 字段布局相同, base 通用)
        pRcAttr->stH264Cbr.u32Gop = gop;
        pRcAttr->stH264Cbr.u32SrcFrameRateNum = fps_num;
        pRcAttr->stH264Cbr.u32SrcFrameRateDen = fps_den;
        pRcAttr->stH264Cbr.fr32DstFrameRateNum = fps_num;
        pRcAttr->stH264Cbr.fr32DstFrameRateDen = fps_den;
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
        pRcAttr->stH265Cbr.u32SrcFrameRateNum = fps_num;
        pRcAttr->stH265Cbr.u32SrcFrameRateDen = fps_den;
        pRcAttr->stH265Cbr.fr32DstFrameRateNum = fps_num;
        pRcAttr->stH265Cbr.fr32DstFrameRateDen = fps_den;
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

static int is_keyframe_h265(int nal_type) {
    // H265E_NALU_BLA_W_LP=16, BLA_W_RADL=17, BLA_N_LP=18
    // H265E_NALU_IDRSLICE=19, IDRSLICE_RADL=20
    // H265E_NALU_CRA=21 (GOP boundary in normalP mode)
    return (nal_type >= 16 && nal_type <= 21);
}

// 从 Annex B raw buffer 扫描 H.265 IRAP NAL
static int is_keyframe_h265_from_buf(const uint8_t *data, uint32_t len) {
    uint32_t i = 0;
    while (i + 4 < len) {
        // 4-byte start code: 0x00 0x00 0x00 0x01
        if (data[i] == 0 && data[i+1] == 0 && data[i+2] == 0 && data[i+3] == 1) {
            if (i + 4 < len) {
                int nal_type = (data[i+4] >> 1) & 0x3F;
                if (nal_type >= 16 && nal_type <= 21) return 1;
            }
            i += 5;
        }
        // 3-byte start code: 0x00 0x00 0x01
        else if (data[i] == 0 && data[i+1] == 0 && data[i+2] == 1) {
            if (i + 3 < len) {
                int nal_type = (data[i+3] >> 1) & 0x3F;
                if (nal_type >= 16 && nal_type <= 21) return 1;
            }
            i += 4;
        } else {
            i++;
        }
    }
    return 0;
}

static int is_keyframe_h264(int nal_type) {
    // H264E_NALU_IDRSLICE = 5
    return (nal_type == 5);
}

// 从 Annex B raw buffer 扫描 H.264 IDR
static int is_keyframe_h264_from_buf(const uint8_t *data, uint32_t len) {
    uint32_t i = 0;
    while (i + 4 < len) {
        if (data[i] == 0 && data[i+1] == 0 && data[i+2] == 0 && data[i+3] == 1) {
            if (i + 4 < len && (data[i+4] & 0x1F) == 5) return 1;
            i += 5;
        } else if (data[i] == 0 && data[i+1] == 0 && data[i+2] == 1) {
            if (i + 3 < len && (data[i+3] & 0x1F) == 5) return 1;
            i += 4;
        } else {
            i++;
        }
    }
    return 0;
}

static void *get_stream_thread(void *arg) {
    int chn_id = (int)(intptr_t)arg;
    VENC_STREAM_S stFrame;
    stFrame.pstPack = (VENC_PACK_S *)malloc(sizeof(VENC_PACK_S));
    int loopCount = 0;

    printf("[rk_camera] stream thread[%d] started\n", chn_id);

    while (!g_quit) {
        int ret = RK_MPI_VENC_GetStream(chn_id, &stFrame, -1);
        if (ret == RK_SUCCESS) {
            void *pData = RK_MPI_MB_Handle2VirAddr(stFrame.pstPack->pMbBlk);
            uint32_t u32Len = stFrame.pstPack->u32Len;
            uint64_t u64PTS = stFrame.pstPack->u64PTS;

            // pack_mode=0 时所有 NAL 合并在一个 pack 中，DataType 只反映第一个 NAL 类型
            // 因此从 raw buffer 扫描 NAL header 判断关键帧，不依赖 DataType
            int is_kf = 0;
            const uint8_t *buf = (const uint8_t *)pData;
            if (g_chn_attr[chn_id].codec == RK_VIDEO_ID_HEVC) {
                is_kf = is_keyframe_h265_from_buf(buf, u32Len);
            } else {
                is_kf = is_keyframe_h264_from_buf(buf, u32Len);
            }

            if (g_callback && pData && u32Len > 0) {
                g_callback(chn_id, (const uint8_t *)pData, u32Len, u64PTS, is_kf);
            }

            if (loopCount == 0) {
                printf("[rk_camera] first frame[%d]: len=%u pts=%llu keyframe=%d\n",
                       chn_id, u32Len, (unsigned long long)u64PTS, is_kf);
            }
            loopCount++;

            RK_MPI_VENC_ReleaseStream(chn_id, &stFrame);
        } else {
            usleep(10 * 1000);
        }
    }

    printf("[rk_camera] stream thread[%d] exit, total frames=%d\n", chn_id, loopCount);
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

// ---- 公开 API ----

// rk_camera_init: 三码流模式初始化
// 参数 main_w/h 仅用于 VI 捕获分辨率 (必须 >= 所有码流的最大分辨率)
// 每个码流的具体参数通过 rk_camera_set_chn_config 提前设置
int rk_camera_init(int main_w, int main_h, int fps, int bitrate) {
    if (g_initialized) return 0;

    printf("[rk_camera] init: VI=%dx%d @%dfps\n", main_w, main_h, fps);

    if (ensure_sys_init() != 0) return -1;

    isp_init();

    // 1. VI 初始化 (捕获主码流分辨率)
    int ret = vi_dev_init();
    if (ret != 0) { printf("[rk_camera] vi_dev_init failed\n"); return -1; }
    ret = vi_chn_init(main_w, main_h, fps);
    if (ret != 0) { printf("[rk_camera] vi_chn_init failed\n"); return -1; }

    // 2. VPSS 初始化 — 从 g_chn_attr 读取每通道的分辨率
    {
        int sub_w = g_chn_enabled[VENC_CHN_SUB] ? g_chn_attr[VENC_CHN_SUB].width : 704;
        int sub_h = g_chn_enabled[VENC_CHN_SUB] ? g_chn_attr[VENC_CHN_SUB].height : 576;
        int third_w = g_chn_enabled[VENC_CHN_THIRD] ? g_chn_attr[VENC_CHN_THIRD].width : 960;
        int third_h = g_chn_enabled[VENC_CHN_THIRD] ? g_chn_attr[VENC_CHN_THIRD].height : 540;

        printf("[rk_camera] VPSS: main=%dx%d sub=%dx%d third=%dx%d\n",
               main_w, main_h, sub_w, sub_h, third_w, third_h);

        ret = vpss_init(main_w, main_h, sub_w, sub_h, third_w, third_h, fps, 1);
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

    g_initialized = 1;
    printf("[rk_camera] initialized, %d stream threads started\n",
           (g_chn_enabled[0]?1:0) + (g_chn_enabled[1]?1:0) + (g_chn_enabled[2]?1:0));
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

// 设置帧回调
void rk_camera_set_callback(frame_callback_t cb) {
    g_callback = cb;
}

// 请求特定通道的 IDR 关键帧
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

// 反初始化
void rk_camera_deinit() {
    if (!g_initialized) return;

    g_quit = 1;
    for (int i = 0; i < VENC_MAX_CHN; i++) {
        if (g_chn_enabled[i]) {
            pthread_join(g_stream_threads[i], NULL);
        }
    }

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

    // 停止 VPSS
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
    int loopCount = 0;

    printf("[rk_camera] audio PCM thread started\n");

    while (!g_audio_quit) {
        int ret = RK_MPI_AI_GetFrame(AI_DEV_ID, AI_CHN_ID, &frame, RK_NULL, -1);
        if (ret == RK_SUCCESS) {
            void *pData = RK_MPI_MB_Handle2VirAddr(frame.pMbBlk);
            uint32_t u32Len = frame.u32Len;

            if (g_audio_callback && pData && u32Len > 0) {
                g_audio_callback((const uint8_t *)pData, u32Len, 0);
            }

            if (loopCount == 0) {
                printf("[rk_camera] first audio PCM frame: len=%u\n", u32Len);
            }
            loopCount++;

            RK_MPI_AI_ReleaseFrame(AI_DEV_ID, AI_CHN_ID, &frame, RK_NULL);
        } else {
            usleep(10 * 1000);
        }
    }

    printf("[rk_camera] audio PCM thread exit, total frames=%d\n", loopCount);
    return NULL;
}

// ---- AENC stream thread (encoding mode) ----

static void *aenc_get_stream_thread(void *arg) {
    (void)arg;
    AUDIO_STREAM_S stream;
    int loopCount = 0;

    printf("[rk_camera] audio AENC thread started\n");

    while (!g_audio_quit) {
        int ret = RK_MPI_AENC_GetStream(AENC_CHN_ID, &stream, -1);
        if (ret == RK_SUCCESS) {
            void *pData = RK_MPI_MB_Handle2VirAddr(stream.pMbBlk);
            uint32_t u32Len = stream.u32Len;

            if (g_audio_callback && pData && u32Len > 0) {
                g_audio_callback((const uint8_t *)pData, u32Len, stream.u64TimeStamp);
            }

            if (loopCount == 0) {
                printf("[rk_camera] first audio AENC frame: len=%u pts=%llu\n",
                       u32Len, (unsigned long long)stream.u64TimeStamp);
            }
            loopCount++;

            RK_MPI_AENC_ReleaseStream(AENC_CHN_ID, &stream);
        } else {
            usleep(10 * 1000);
        }
    }

    printf("[rk_camera] audio AENC thread exit, total frames=%d\n", loopCount);
    return NULL;
}

// ---- Audio init helper: configure AI device ----

static int audio_ai_init(int sample_rate, const char *card_name,
                          int channels, int frame_size, const char *format) {
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
    aiAttr.enSoundmode = AUDIO_SOUND_MODE_MONO;
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
    int ret = audio_ai_init(sample_rate, card_name, channels, frame_size, format);
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

// ---- Stubs for glibc functions missing in uclibc ----
unsigned long getauxval(unsigned long type) {
    (void)type;
    return 0;
}
