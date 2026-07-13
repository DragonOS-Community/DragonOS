/*
 * packet_ring_test — TPACKET V2 mmap RX ring buffer 功能测试
 *
 * 验证 DragonOS AF_PACKET socket 的 TPACKET mmap ring buffer 实现：
 *   1. socket(AF_PACKET, SOCK_RAW, ETH_P_ALL)
 *   2. setsockopt(PACKET_VERSION, TPACKET_V2)
 *   3. setsockopt(PACKET_RX_RING, ...)
 *   4. mmap ring buffer
 *   5. poll 等待数据
 *   6. 遍历帧读取 tp_status==TP_STATUS_USER 的帧
 *   7. 翻回帧到内核 (TP_STATUS_KERNEL)
 *   8. getsockopt(PACKET_STATISTICS)
 *
 * 运行: 在 DragonOS 中执行 /bin/packet_ring_test
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>

#include <arpa/inet.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/mman.h>
#include <poll.h>

#include <linux/if_packet.h>
#include <linux/if_ether.h>

/* ---- Ring 配置 ---- */
#define BLOCK_SIZE    4096
#define BLOCK_NR      1
#define FRAME_SIZE    2048
#define FRAME_NR      2
#define RING_SIZE     (BLOCK_SIZE * BLOCK_NR)   /* 4096 字节 */

#define POLL_TIMEOUT_MS  5000
#define DUMP_BYTES       32

int main(void)
{
    int fd = -1;
    void *ring = MAP_FAILED;
    int rc = 1;

    /* ============================================================
     * Step 1: 创建 AF_PACKET raw socket
     * ============================================================ */
    fd = socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL));
    if (fd < 0) {
        perror("[FAIL] socket(AF_PACKET, SOCK_RAW, ETH_P_ALL)");
        return 1;
    }
    printf("[OK] Step 1: socket(AF_PACKET, SOCK_RAW, ETH_P_ALL) = %d\n", fd);

    /* ============================================================
     * Step 2: 设置 TPACKET 版本为 V2
     * ============================================================ */
    int version = TPACKET_V2;
    if (setsockopt(fd, SOL_PACKET, PACKET_VERSION,
                   &version, sizeof(version)) < 0) {
        perror("[FAIL] setsockopt(PACKET_VERSION, TPACKET_V2)");
        goto cleanup;
    }
    printf("[OK] Step 2: setsockopt(PACKET_VERSION, TPACKET_V2)\n");

    /* ============================================================
     * Step 3 + 4: 构造 tpacket_req 并设置 PACKET_RX_RING
     *
     * block_size=4096, block_nr=1, frame_size=2048, frame_nr=2
     * frames_per_block = 4096/2048 = 2, 2*1 = 2 ✓
     * ============================================================ */
    struct tpacket_req req;
    memset(&req, 0, sizeof(req));
    req.tp_block_size = BLOCK_SIZE;
    req.tp_block_nr   = BLOCK_NR;
    req.tp_frame_size = FRAME_SIZE;
    req.tp_frame_nr   = FRAME_NR;

    if (setsockopt(fd, SOL_PACKET, PACKET_RX_RING,
                   &req, sizeof(req)) < 0) {
        perror("[FAIL] setsockopt(PACKET_RX_RING)");
        goto cleanup;
    }
    printf("[OK] Step 3-4: setsockopt(PACKET_RX_RING) "
           "bs=%u bn=%u fs=%u fn=%u\n",
           req.tp_block_size, req.tp_block_nr,
           req.tp_frame_size, req.tp_frame_nr);

    /* ============================================================
     * Step 5: mmap ring buffer
     * ============================================================ */
    ring = mmap(NULL, RING_SIZE, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0);
    if (ring == MAP_FAILED) {
        perror("[FAIL] mmap");
        goto cleanup;
    }
    printf("[OK] Step 5: mmap(%d, PROT_READ|PROT_WRITE, MAP_SHARED) = %p\n",
           RING_SIZE, ring);

    /* ============================================================
     * Step 6: poll 等待数据到达 (EPOLLIN / POLLIN)
     * ============================================================ */
    struct pollfd pfd;
    pfd.fd = fd;
    pfd.events = POLLIN;
    pfd.revents = 0;

    printf("... poll() waiting for packets (timeout %d ms) ...\n",
           POLL_TIMEOUT_MS);
    int pr = poll(&pfd, 1, POLL_TIMEOUT_MS);
    if (pr < 0) {
        perror("[FAIL] poll");
        goto cleanup;
    }
    if (pr == 0) {
        printf("[WARN] Step 6: poll() timed out — no packets in %d ms\n",
               POLL_TIMEOUT_MS);
    } else {
        printf("[OK] Step 6: poll() = %d, revents = 0x%hx (POLLIN=%d)\n",
               pr, pfd.revents, (pfd.revents & POLLIN) != 0);
    }

    /* ============================================================
     * Step 7: 遍历 ring 中所有帧，处理 TP_STATUS_USER 帧
     *
     * V1/V2 ring 是平坦帧数组。frame i 起始地址 = base + i * tp_frame_size。
     * 每帧以 struct tpacket2_hdr 开头。
     * tp_status==TP_STATUS_USER 表示内核已填好数据，用户可读。
     * tp_mac 给出 MAC header 在帧内的偏移，数据从该偏移开始。
     * ============================================================ */
    int frames_read = 0;

    for (unsigned int i = 0; i < FRAME_NR; i++) {
        unsigned char *frame_base = (unsigned char *)ring + i * FRAME_SIZE;
        struct tpacket2_hdr *hdr = (struct tpacket2_hdr *)frame_base;

        /* tp_status: V2 是 __u32 */
        if (hdr->tp_status != TP_STATUS_USER)
            continue;

        frames_read++;

        printf("[OK] Step 7: Frame %u: tp_status=USER "
               "tp_len=%u tp_snaplen=%u tp_mac=%u tp_net=%u "
               "tp_sec=%u tp_nsec=%u",
               i, hdr->tp_len, hdr->tp_snaplen,
               hdr->tp_mac, hdr->tp_net,
               hdr->tp_sec, hdr->tp_nsec);

        if (hdr->tp_vlan_tci || hdr->tp_vlan_tpid) {
            printf(" vlan_tci=0x%04x vlan_tpid=0x%04x",
                   hdr->tp_vlan_tci, hdr->tp_vlan_tpid);
        }
        printf("\n");

        /* 用 tp_mac 定位数据，打印前 DUMP_BYTES 字节 */
        if (hdr->tp_mac > 0 && hdr->tp_mac + DUMP_BYTES <= (i + 1) * FRAME_SIZE) {
            unsigned char *data = frame_base + hdr->tp_mac;
            unsigned int to_print = hdr->tp_snaplen;
            if (to_print > DUMP_BYTES)
                to_print = DUMP_BYTES;
            printf("  Data (%u/%u bytes at tp_mac=%u):",
                   to_print, hdr->tp_snaplen, hdr->tp_mac);
            for (unsigned int j = 0; j < to_print; j++)
                printf(" %02x", data[j]);
            printf("\n");
        }

        /* ============================================================
         * Step 8: 将帧翻回内核 (TP_STATUS_KERNEL)
         *
         * 内核扫描 tp_status==KERNEL 的帧来写入新数据。
         * 写入前加 compiler barrier 确保 header 字段读取已完成。
         * ============================================================ */
        __sync_synchronize();
        hdr->tp_status = TP_STATUS_KERNEL;
        printf("  -> Frame %u returned to kernel\n", i);
    }

    if (frames_read == 0 && pr > 0)
        printf("[WARN] Step 7: poll returned but no USER frames found\n");

    /* ============================================================
     * Step 9: getsockopt(PACKET_STATISTICS)
     *
     * Linux 语义: 读取后计数器重置。
     * ============================================================ */
    struct tpacket_stats stats;
    socklen_t optlen = sizeof(stats);
    memset(&stats, 0, sizeof(stats));

    if (getsockopt(fd, SOL_PACKET, PACKET_STATISTICS,
                   &stats, &optlen) < 0) {
        perror("[FAIL] getsockopt(PACKET_STATISTICS)");
        goto cleanup;
    }
    printf("[OK] Step 9: PACKET_STATISTICS: tp_packets=%u tp_drops=%u\n",
           stats.tp_packets, stats.tp_drops);

    if (stats.tp_packets > 0) {
        printf("[PASS] tp_packets > 0 — ring buffer captured traffic\n");
    } else {
        printf("[INFO] tp_packets == 0 (no traffic during test window)\n");
    }

    rc = 0;

cleanup:
    /* ============================================================
     * Step 10: 清理 — munmap + close
     * ============================================================ */
    if (ring != MAP_FAILED) {
        munmap(ring, RING_SIZE);
        printf("[OK] Step 10: munmap(%d)\n", RING_SIZE);
    }
    if (fd >= 0) {
        close(fd);
        printf("[OK] Step 10: close(%d)\n", fd);
    }

    printf("\n=== packet_ring_test %s ===\n", rc ? "FAILED" : "PASSED");
    return rc;
}
