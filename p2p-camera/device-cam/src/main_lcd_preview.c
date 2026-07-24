// SPDX-License-Identifier: MIT
// standalone `lcd-preview` 入口 — 仅做本地 LCD 预览, 不依赖 p2p / LVGL / 编码。
//
// 用途:
//   - 离线取景器 (device-cam 不运行时单独预览摄像头)
//   - 上板验证 R1: VOP 层序/alpha 使 "video plane 在 graphic 之下" (LVGL 框在其上)
//
// 编译见同目录 Makefile.lcd_preview (需在 RV1106 SDK 环境下交叉编译)。
//
// 注: 本文件直接调用 rk_camera.c 中定义的公开 preview-only 封装
//      (rk_camera_preview_only_init / rk_camera_preview_only_deinit),
//      它们负责 SYS_Init + ISP + VI dev 的最小初始化后调用 lcd_preview_start。

#include <stdio.h>
#include <stdlib.h>
#include <signal.h>
#include <unistd.h>

// 由 rk_camera.c 提供的 preview-only 封装 (不触发编码/VPSS)
extern int  rk_camera_preview_only_init(int w, int h, int fps);
extern void rk_camera_preview_only_deinit(void);

static volatile sig_atomic_t g_stop = 0;

static void on_sigint(int sig) {
    (void)sig;
    g_stop = 1;
}

int main(int argc, char **argv) {
    int w   = (argc > 1) ? atoi(argv[1]) : 720;
    int h   = (argc > 2) ? atoi(argv[2]) : 720;
    int fps = (argc > 3) ? atoi(argv[3]) : 20;

    signal(SIGINT, on_sigint);
    signal(SIGTERM, on_sigint);

    printf("[lcd-preview] starting: %dx%d @%dfps (Ctrl+C to stop)\n", w, h, fps);
    if (rk_camera_preview_only_init(w, h, fps) != 0) {
        printf("[lcd-preview] init failed\n");
        return -1;
    }

    printf("[lcd-preview] running... (VO video plane 硬件预览)\n");
    while (!g_stop) {
        sleep(1);
    }

    printf("[lcd-preview] stopping...\n");
    rk_camera_preview_only_deinit();
    printf("[lcd-preview] done\n");
    return 0;
}
