// SPDX-License-Identifier: MIT
// bbox_shm 实现 — 见 bbox_shm.h 说明。
//
// 同步模型: 单生产者 / 单消费者, 无锁。
//   生产者: 写 slot[(write_idx+1)%N] (即"下一个"), 写完原子自增 write_idx。
//   消费者: 读 slot[write_idx] (生产者最近一次提交完成者), 因生产者
//            先写完内容再提交序号, 消费者读到的必是完整帧 (最多读到上一帧, 不撕裂)。

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <unistd.h>

#include <stdatomic.h>

#include "bbox_shm.h"

#ifndef BBOX_SHM_SIZE
#define BBOX_SHM_SIZE (sizeof(bbox_shm_t))
#endif

bbox_shm_t *bbox_shm_open(const char *name, int create) {
    int fd = shm_open(name, create ? (O_CREAT | O_RDWR) : O_RDWR, 0666);
    if (fd < 0) {
        printf("[bbox_shm] shm_open(%s) failed\n", name);
        return NULL;
    }

    if (create) {
        if (ftruncate(fd, (off_t)BBOX_SHM_SIZE) != 0) {
            printf("[bbox_shm] ftruncate failed\n");
            close(fd);
            return NULL;
        }
    }

    void *addr = mmap(NULL, BBOX_SHM_SIZE, PROT_READ | PROT_WRITE,
                       MAP_SHARED, fd, 0);
    if (addr == MAP_FAILED) {
        printf("[bbox_shm] mmap failed\n");
        close(fd);
        return NULL;
    }
    close(fd);  // mmap 后仍有效

    bbox_shm_t *s = (bbox_shm_t *)addr;
    if (create) {
        memset(s, 0, BBOX_SHM_SIZE);
        s->slot_count = BBOX_SHM_SLOTS;
        s->max_boxes  = BBOX_SHM_MAX_BOXES;
        atomic_store(&s->write_idx, 0);
        atomic_store(&s->seq, 0);
    }
    return s;
}

void bbox_shm_close(bbox_shm_t *s, int free) {
    if (!s) return;
    munmap((void *)s, BBOX_SHM_SIZE);
    (void)free;  // 生产者 unlink 由调用方用原 name 处理, 保持简单
}

void bbox_shm_publish(bbox_shm_t *s, const bbox_t *boxes, int n) {
    if (!s || n < 0) return;
    if (n > BBOX_SHM_MAX_BOXES) n = BBOX_SHM_MAX_BOXES;

    uint32_t idx = atomic_load(&s->write_idx);
    uint32_t next = (idx + 1) % BBOX_SHM_SLOTS;

    bbox_snapshot_t *slot = &s->slots[next];
    slot->seq = atomic_load(&s->seq) + 1;
    slot->count = n;
    for (int i = 0; i < n; i++) {
        slot->boxes[i] = boxes[i];
    }
    // 写完内容后再提交 write_idx, 保证消费者读到的 slot 内容完整
    atomic_thread_fence(memory_order_release);
    atomic_store(&s->write_idx, next);
    atomic_fetch_add(&s->seq, 1);
}

int bbox_shm_acquire(bbox_shm_t *s, bbox_snapshot_t *snap) {
    if (!s || !snap) return 0;
    uint32_t idx = atomic_load(&s->write_idx);
    atomic_thread_fence(memory_order_acquire);
    *snap = s->slots[idx];  // 拷贝最近一次提交完成的快照
    return (snap->count >= 0) ? 1 : 0;
}
