// SPDX-License-Identifier: MIT
// RV1106 Camera SDK C shim — 封装 VI + VENC 初始化为简单接口
// 编译: armv7l-linux-musleabihf-gcc -shared -fPIC -o librk_camera.so rk_camera.c \
//       -I<rockit_sdk_include_dir> -lrockit_full -lrkaiq
//
// 这个 shim 封装了复杂的 VI/VENC 初始化逻辑, Rust 侧只需调用:
//   rk_camera_init(width, height, fps, bitrate)
//   rk_camera_get_frame(buf, max_len) → actual_len
//   rk_camera_deinit()

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <pthread.h>
#include <unistd.h>
#include <time.h>

#include "rk_mpi_sys.h"
#include "rk_mpi_vi.h"
#include "rk_mpi_venc.h"
#include "rk_mpi_mb.h"
#include "rk_mpi_ai.h"
#include "rk_common.h"
#include "rk_comm_video.h"
#include "rk_comm_venc.h"
#include "rk_comm_vi.h"
#include "rk_comm_aio.h"

// ISP (rkaiq) 头文件
#include "rk_aiq_user_api2_sysctl.h"

#define VENC_CHN_ID 0
#define VI_DEV_ID   0
#define VI_CHN_ID   0  // 0: rkisp_mainpath (与 simple_vi_bind_venc 一致)
#define CAM_ID      0
#define IQ_FILE_DIR "/etc/iqfiles"  // ISP IQ 参数文件目录

static pthread_t g_get_stream_thread;
static volatile int g_quit = 0;
static volatile int g_initialized = 0;
static rk_aiq_sys_ctx_t *g_aiq_ctx = NULL;  // ISP AIQ 上下文

// RK_MPI_SYS_Init/Exit 引用计数 — camera 和 audio 各自 init 时 +1, deinit 时 -1
// 当计数归零时才真正调用 RK_MPI_SYS_Exit
// 解决: 当 audio 先于 camera 初始化时 (例如 gateway 启动后还没有 viewer 连接,
//       视频 spawn 等待 start_trigger, 但音频立即 spawn), MPP 系统未初始化导致段错误
static volatile int g_sys_init_count = 0;

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

// 帧回调: Rust 侧通过 rk_camera_set_callback 设置
typedef void (*frame_callback_t)(const uint8_t *data, uint32_t len, uint64_t pts, int is_keyframe);
static frame_callback_t g_callback = NULL;

// 获取当前时间 (微秒)
static uint64_t get_now_us() {
    struct timespec ts = {0, 0};
    clock_gettime(CLOCK_MONOTONIC, &ts);
    return (uint64_t)ts.tv_sec * 1000000 + (uint64_t)ts.tv_nsec / 1000;
}

// VENC 取流线程
static void *get_stream_thread(void *arg) {
    (void)arg;
    VENC_STREAM_S stFrame;
    stFrame.pstPack = (VENC_PACK_S *)malloc(sizeof(VENC_PACK_S));
    int loopCount = 0;

    printf("[rk_camera] get_stream_thread started\n");

    while (!g_quit) {
        int ret = RK_MPI_VENC_GetStream(VENC_CHN_ID, &stFrame, -1);
        if (ret == RK_SUCCESS) {
            void *pData = RK_MPI_MB_Handle2VirAddr(stFrame.pstPack->pMbBlk);
            uint32_t u32Len = stFrame.pstPack->u32Len;
            uint64_t u64PTS = stFrame.pstPack->u64PTS;

            // 判断是否关键帧 (H265E_NALU_IDRSLICE = 19)
            int is_keyframe = 0;
            if (stFrame.pstPack->DataType.enH265EType == 19 ||
                stFrame.pstPack->DataType.enH265EType == 20) {
                is_keyframe = 1;
            }

            if (g_callback && pData && u32Len > 0) {
                g_callback((const uint8_t *)pData, u32Len, u64PTS, is_keyframe);
            }

            if (loopCount == 0) {
                printf("[rk_camera] first frame: len=%u pts=%llu keyframe=%d\n",
                       u32Len, (unsigned long long)u64PTS, is_keyframe);
            }
            loopCount++;

            RK_MPI_VENC_ReleaseStream(VENC_CHN_ID, &stFrame);
        } else {
            usleep(10 * 1000);  // 10ms
        }
    }

    printf("[rk_camera] get_stream_thread exit, total frames=%d\n", loopCount);
    free(stFrame.pstPack);
    return NULL;
}

// ---- ISP 初始化 ----
// 参考 rkipc/common/isp/rv1106/isp.c 的 sample_comm_isp_init
// 流程: enumStaticMetas → preInit_devBufCnt → preInit_scene → init → prepare → start
static int isp_init() {
    int ret;
    rk_aiq_working_mode_t wdr_mode = RK_AIQ_WORKING_MODE_NORMAL;

    // 设置 HDR_MODE 环境变量 (AIQ 需要)
    char hdr_str[16];
    snprintf(hdr_str, sizeof(hdr_str), "%d", (int)wdr_mode);
    setenv("HDR_MODE", hdr_str, 1);

    // 1. 枚举传感器信息
    rk_aiq_static_info_t aiq_static_info;
    ret = rk_aiq_uapi2_sysctl_enumStaticMetasByPhyId(CAM_ID, &aiq_static_info);
    if (ret < 0 || aiq_static_info.sensor_info.phyId == -1) {
        printf("[rk_camera] WARN: sensor not found (phyId=%d), ISP disabled\n",
               aiq_static_info.sensor_info.phyId);
        return 0;  // 不算错误, 继续不使用 ISP
    }
    printf("[rk_camera] sensor: %s\n", aiq_static_info.sensor_info.sensor_name);

    // 2. 预初始化 buf 数量
    rk_aiq_uapi2_sysctl_preInit_devBufCnt(
        aiq_static_info.sensor_info.sensor_name, "rkraw_rx", 2);

    // 3. 预设场景 (normal = 非 HDR)
    ret = rk_aiq_uapi2_sysctl_preInit_scene(
        aiq_static_info.sensor_info.sensor_name, "normal", NULL);
    if (ret < 0) {
        printf("[rk_camera] WARN: preInit_scene failed\n");
    }

    // 4. 初始化 AIQ (加载 IQ 文件)
    g_aiq_ctx = rk_aiq_uapi2_sysctl_init(
        aiq_static_info.sensor_info.sensor_name, IQ_FILE_DIR, NULL, NULL);
    if (!g_aiq_ctx) {
        printf("[rk_camera] WARN: sysctl_init failed, ISP disabled\n");
        return 0;
    }

    // 5. 准备 + 启动
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

// VI 设备初始化
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

// VI 通道初始化
static int vi_chn_init(int width, int height) {
    VI_CHN_ATTR_S vi_chn_attr;
    memset(&vi_chn_attr, 0, sizeof(vi_chn_attr));
    vi_chn_attr.stIspOpt.u32BufCount = 3;
    vi_chn_attr.stIspOpt.enMemoryType = VI_V4L2_MEMORY_TYPE_DMABUF;
    vi_chn_attr.stSize.u32Width = width;
    vi_chn_attr.stSize.u32Height = height;
    vi_chn_attr.enPixelFormat = RK_FMT_YUV420SP;
    vi_chn_attr.enCompressMode = COMPRESS_MODE_NONE;
    vi_chn_attr.u32Depth = 0;

    int ret = RK_MPI_VI_SetChnAttr(VI_DEV_ID, VI_CHN_ID, &vi_chn_attr);
    ret |= RK_MPI_VI_EnableChn(VI_DEV_ID, VI_CHN_ID);
    return ret;
}

// VENC 初始化
static int venc_init(int width, int height, int fps, int bitrate_kbps) {
    VENC_CHN_ATTR_S stAttr;
    memset(&stAttr, 0, sizeof(VENC_CHN_ATTR_S));

    // H.265 CBR
    stAttr.stRcAttr.enRcMode = VENC_RC_MODE_H265CBR;
    stAttr.stRcAttr.stH265Cbr.u32BitRate = bitrate_kbps;
    stAttr.stRcAttr.stH265Cbr.u32Gop = fps * 2;  // 2秒一个GOP

    stAttr.stVencAttr.enType = RK_VIDEO_ID_HEVC;
    stAttr.stVencAttr.enPixelFormat = RK_FMT_YUV420SP;
    stAttr.stVencAttr.u32Profile = 0;  // Main Profile
    stAttr.stVencAttr.u32PicWidth = width;
    stAttr.stVencAttr.u32PicHeight = height;
    stAttr.stVencAttr.u32VirWidth = width;
    stAttr.stVencAttr.u32VirHeight = height;
    stAttr.stVencAttr.u32StreamBufCnt = 2;
    stAttr.stVencAttr.u32BufSize = width * height * 3 / 2;
    stAttr.stVencAttr.enMirror = MIRROR_NONE;

    int ret = RK_MPI_VENC_CreateChn(VENC_CHN_ID, &stAttr);
    if (ret != RK_SUCCESS) return ret;

    VENC_RECV_PIC_PARAM_S stRecvParam;
    memset(&stRecvParam, 0, sizeof(stRecvParam));
    stRecvParam.s32RecvPicNum = -1;
    ret = RK_MPI_VENC_StartRecvFrame(VENC_CHN_ID, &stRecvParam);
    return ret;
}

// ---- 公开 API (Rust FFI 调用) ----

// 初始化摄像头 (VI + VENC + 绑定 + 取流线程)
// 返回 0 成功, 非0 失败
int rk_camera_init(int width, int height, int fps, int bitrate_kbps) {
    if (g_initialized) return 0;
    int ret;

    printf("[rk_camera] init %dx%d @%dfps, bitrate=%dkbps\n",
           width, height, fps, bitrate_kbps);

    // 确保 MPP 系统已初始化 (与 rk_audio_init 共享, 引用计数)
    if (ensure_sys_init() != 0) {
        return -1;
    }

    // ISP 初始化 (必须在 VI 之前, 否则传感器无数据)
    isp_init();

    ret = vi_dev_init();
    if (ret != 0) {
        printf("[rk_camera] vi_dev_init failed: %d\n", ret);
        return -1;
    }

    ret = vi_chn_init(width, height);
    if (ret != 0) {
        printf("[rk_camera] vi_chn_init failed: %d\n", ret);
        return -1;
    }

    ret = venc_init(width, height, fps, bitrate_kbps);
    if (ret != 0) {
        printf("[rk_camera] venc_init failed: %x\n", ret);
        return -1;
    }

    // 绑定 VI → VENC
    MPP_CHN_S stSrcChn, stDestChn;
    stSrcChn.enModId = RK_ID_VI;
    stSrcChn.s32DevId = VI_DEV_ID;
    stSrcChn.s32ChnId = VI_CHN_ID;
    stDestChn.enModId = RK_ID_VENC;
    stDestChn.s32DevId = 0;
    stDestChn.s32ChnId = VENC_CHN_ID;

    ret = RK_MPI_SYS_Bind(&stSrcChn, &stDestChn);
    if (ret != RK_SUCCESS) {
        printf("[rk_camera] RK_MPI_SYS_Bind failed: %x\n", ret);
        return -1;
    }

    // 启动取流线程
    g_quit = 0;
    ret = pthread_create(&g_get_stream_thread, NULL, get_stream_thread, NULL);
    if (ret != 0) return -1;

    g_initialized = 1;
    printf("[rk_camera] initialized, stream thread started\n");
    return 0;
}

// 设置帧回调 (在取流线程中调用, 非阻塞)
void rk_camera_set_callback(frame_callback_t cb) {
    g_callback = cb;
}

// 请求 IDR 关键帧
int rk_camera_request_idr() {
    return RK_MPI_VENC_RequestIDR(VENC_CHN_ID, RK_TRUE);
}

// 反初始化
void rk_camera_deinit() {
    if (!g_initialized) return;

    g_quit = 1;
    pthread_join(g_get_stream_thread, NULL);

    // 解绑
    MPP_CHN_S stSrcChn, stDestChn;
    stSrcChn.enModId = RK_ID_VI;
    stSrcChn.s32DevId = VI_DEV_ID;
    stSrcChn.s32ChnId = VI_CHN_ID;
    stDestChn.enModId = RK_ID_VENC;
    stDestChn.s32DevId = 0;
    stDestChn.s32ChnId = VENC_CHN_ID;
    RK_MPI_SYS_UnBind(&stSrcChn, &stDestChn);

    RK_MPI_VI_DisableChn(VI_DEV_ID, VI_CHN_ID);
    RK_MPI_VENC_StopRecvFrame(VENC_CHN_ID);
    RK_MPI_VENC_DestroyChn(VENC_CHN_ID);
    RK_MPI_VI_DisableDev(VI_DEV_ID);

    // ISP 反初始化 (在 VI 销毁之后)
    isp_deinit();

    // 释放 MPP 系统引用 (与 rk_audio_deinit 共享, 引用计数归零时才真正 Exit)
    maybe_sys_exit();

    g_initialized = 0;
    printf("[rk_camera] deinitialized\n");
}

// ============== 音频采集 (AI) ==============

#define AI_DEV_ID   0
#define AI_CHN_ID   0

static pthread_t g_audio_thread;
static volatile int g_audio_quit = 0;
static volatile int g_audio_initialized = 0;

// 音频帧回调: fn(data, len, pts_us)
typedef void (*audio_callback_t)(const uint8_t *data, uint32_t len, uint64_t pts_us);
static audio_callback_t g_audio_callback = NULL;

// 音频取流线程
static void *audio_get_stream_thread(void *arg) {
    (void)arg;
    AUDIO_FRAME_S frame;
    int loopCount = 0;

    printf("[rk_camera] audio thread started\n");

    while (!g_audio_quit) {
        int ret = RK_MPI_AI_GetFrame(AI_DEV_ID, AI_CHN_ID, &frame, RK_NULL, -1);
        if (ret == RK_SUCCESS) {
            void *pData = RK_MPI_MB_Handle2VirAddr(frame.pMbBlk);
            uint32_t u32Len = frame.u32Len;

            if (g_audio_callback && pData && u32Len > 0) {
                g_audio_callback((const uint8_t *)pData, u32Len, 0);
            }

            if (loopCount == 0) {
                printf("[rk_camera] first audio frame: len=%u\n", u32Len);
            }
            loopCount++;

            RK_MPI_AI_ReleaseFrame(AI_DEV_ID, AI_CHN_ID, &frame, RK_NULL);
        } else {
            usleep(10 * 1000);
        }
    }

    printf("[rk_camera] audio thread exit, total frames=%d\n", loopCount);
    return NULL;
}

// 初始化音频采集 (AI)
// sample_rate: 8000/16000/48000
// 返回 0 成功
int rk_audio_init(int sample_rate) {
    if (g_audio_initialized) return 0;

    printf("[rk_camera] audio init: %dHz\n", sample_rate);

    // 确保 MPP 系统已初始化 (与 rk_camera_init 共享, 引用计数)
    // 修复: 视频源 spawn 是延迟的 (等第一个 viewer 连接), 但音频 spawn 立即执行,
    //       如果不调用 ensure_sys_init, RK_MPI_AI_* 会因 MPP 系统未初始化而段错误
    if (ensure_sys_init() != 0) {
        return -1;
    }

    AIO_ATTR_S aiAttr;
    AI_CHN_PARAM_S pstParams;
    int ret;

    memset(&aiAttr, 0, sizeof(AIO_ATTR_S));
    sprintf((char *)aiAttr.u8CardName, "%s", "hw:0,0");
    aiAttr.soundCard.channels = 2;
    aiAttr.soundCard.sampleRate = sample_rate;
    aiAttr.soundCard.bitWidth = AUDIO_BIT_WIDTH_16;
    aiAttr.enBitwidth = AUDIO_BIT_WIDTH_16;
    aiAttr.enSamplerate = (AUDIO_SAMPLE_RATE_E)sample_rate;
    aiAttr.enSoundmode = AUDIO_SOUND_MODE_MONO;
    aiAttr.u32PtNumPerFrm = 1024;  // 每帧采样点数 (约 64ms @16kHz)
    aiAttr.u32FrmNum = 4;
    aiAttr.u32EXFlag = 0;
    aiAttr.u32ChnCnt = 2;

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

    // 启动音频取流线程
    g_audio_quit = 0;
    ret = pthread_create(&g_audio_thread, NULL, audio_get_stream_thread, NULL);
    if (ret != 0) return -1;

    g_audio_initialized = 1;
    printf("[rk_camera] audio initialized\n");
    return 0;
}

// 设置音频回调
void rk_audio_set_callback(audio_callback_t cb) {
    g_audio_callback = cb;
}

// 音频反初始化
void rk_audio_deinit() {
    if (!g_audio_initialized) return;

    g_audio_quit = 1;
    pthread_join(g_audio_thread, NULL);

    RK_MPI_AI_DisableChn(AI_DEV_ID, AI_CHN_ID);
    RK_MPI_AI_Disable(AI_DEV_ID);

    // 释放 MPP 系统引用 (与 rk_camera_deinit 共享, 引用计数归零时才真正 Exit)
    maybe_sys_exit();

    g_audio_initialized = 0;
    printf("[rk_camera] audio deinitialized\n");
}

// ---- Stubs for glibc functions missing in uclibc ----
// getauxval: 读取 ELF auxiliary vector, Rust std + ring crate 用于 CPU 特性检测
// uclibc 没有此函数, 返回 0 表示未知 (ring 会回退到软件实现)
unsigned long getauxval(unsigned long type) {
    (void)type;
    return 0;
}
