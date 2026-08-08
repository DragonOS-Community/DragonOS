/*
 * packet_ring_test — TPACKET V1/V2 mmap RX ring buffer 功能测试
 *
 * 验证 DragonOS AF_PACKET socket 的 TPACKET mmap ring buffer 实现：
 *   1. socket(AF_PACKET, SOCK_RAW, ETH_P_ALL)
 *   2. setsockopt(PACKET_VERSION, TPACKET_V1 或 V2)
 *   3. setsockopt(PACKET_RX_RING, ...)
 *   4. mmap ring buffer
 *   5. 通过 sendto 向 loopback 接口发送一帧以太网数据，主动触发接收
 *      （AF_PACKET raw socket bind ETH_P_ALL 会收到自己发出的包）
 *   6. poll 等待数据
 *   7. 遍历帧读取 tp_status==TP_STATUS_USER 的帧，校验数据与发送一致
 *   8. 翻回帧到内核 (TP_STATUS_KERNEL)
 *   9. getsockopt(PACKET_STATISTICS)
 *
 * 测试自包含：不依赖环境外部流量，通过 loopback 自发自收验证抓包功能。
 *
 * 运行: 在 DragonOS 中执行 /bin/packet_ring_test [v1|v2] (默认 v2)
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
#include <sys/ioctl.h>
#include <poll.h>
#include <net/if.h>

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

/* ---- 自发测试帧 ----
 * 14 字节以太网头 (dst=broadcast, src=00:..:00, proto=ETH_P_IP)
 * + 4 字节 payload "TEST" => 共 18 字节
 */
#define SENT_PAYLOAD     "TEST"
#define SENT_PAYLOAD_LEN 4
#define SENT_FRAME_LEN   (ETH_HLEN + SENT_PAYLOAD_LEN)

int main(int argc, char *argv[])
{
    int fd = -1;
    void *ring = MAP_FAILED;
    int rc = 1;
    int verified = 0;
    unsigned char sent_frame[SENT_FRAME_LEN];
    int lo_ifindex = -1;

    /* ---- 选择 TPACKET 版本 ----
     * argv[1]=="v1" -> TPACKET_V1; 否则(含默认) -> TPACKET_V2 */
    int tpacket_ver = TPACKET_V2;
    int is_v1 = 0;
    if (argc >= 2 && strcmp(argv[1], "v1") == 0) {
        tpacket_ver = TPACKET_V1;
        is_v1 = 1;
    }
    printf("=== packet_ring_test: TPACKET_%s mode ===\n", is_v1 ? "V1" : "V2");

    /* 构造自发帧：dst=broadcast, src=zero, proto=htons(ETH_P_IP), payload="TEST" */
    memset(sent_frame, 0, sizeof(sent_frame));
    memset(sent_frame, 0xff, 6);                /* dst MAC = broadcast */
    /* src MAC 保持 00:00:00:00:00:00 */
    {
        uint16_t proto = htons(ETH_P_IP);
        memcpy(sent_frame + 12, &proto, 2);     /* Ethernet protocol */
    }
    memcpy(sent_frame + ETH_HLEN, SENT_PAYLOAD, SENT_PAYLOAD_LEN);

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
     * Step 2: 设置 TPACKET 版本 (V1 或 V2)
     * ============================================================ */
    int version = tpacket_ver;
    if (setsockopt(fd, SOL_PACKET, PACKET_VERSION,
                   &version, sizeof(version)) < 0) {
        perror(is_v1 ? "[FAIL] setsockopt(PACKET_VERSION, TPACKET_V1)"
                     : "[FAIL] setsockopt(PACKET_VERSION, TPACKET_V2)");
        goto cleanup;
    }
    printf("[OK] Step 2: setsockopt(PACKET_VERSION, TPACKET_%s)\n",
           is_v1 ? "V1" : "V2");

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
     * Step 6: 主动发送一帧到 loopback 接口，触发 AF_PACKET 接收
     *
     * 通过 SIOCGIFINDEX 获取 "lo" 的 ifindex，再用 sendto 指定
     * sockaddr_ll 发送。AF_PACKET raw socket bind ETH_P_ALL 会
     * 收到本 socket 自己发出的包（loopback 时内核会回环）。
     * ============================================================ */
    struct ifreq ifr;
    memset(&ifr, 0, sizeof(ifr));
    strncpy(ifr.ifr_name, "lo", IFNAMSIZ - 1);

    if (ioctl(fd, SIOCGIFINDEX, &ifr) < 0) {
        perror("[FAIL] Step 6: ioctl(SIOCGIFINDEX, \"lo\")");
        goto cleanup;
    }
    lo_ifindex = ifr.ifr_ifindex;
    printf("[OK] Step 6: lo ifindex = %d\n", lo_ifindex);

    struct sockaddr_ll sa;
    memset(&sa, 0, sizeof(sa));
    sa.sll_family   = AF_PACKET;
    sa.sll_protocol = htons(ETH_P_ALL);
    sa.sll_ifindex  = lo_ifindex;
    sa.sll_halen    = 6;
    memcpy(sa.sll_addr, sent_frame, 6);   /* dst MAC = broadcast */

    ssize_t sent = sendto(fd, sent_frame, SENT_FRAME_LEN, 0,
                          (struct sockaddr *)&sa, sizeof(sa));
    if (sent < 0) {
        perror("[FAIL] Step 6: sendto(loopback)");
        goto cleanup;
    }
    if (sent != SENT_FRAME_LEN) {
        printf("[FAIL] Step 6: sendto short write: %zd (expected %d)\n",
               sent, SENT_FRAME_LEN);
        goto cleanup;
    }
    printf("[OK] Step 6: sendto(loopback) sent %zd bytes (broadcast ETH_P_IP + \"TEST\")\n",
           sent);

    /* ============================================================
     * Step 7: poll 等待数据到达 (EPOLLIN / POLLIN)
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
        printf("[WARN] Step 7: poll() timed out — no packets in %d ms\n",
               POLL_TIMEOUT_MS);
    } else {
        printf("[OK] Step 7: poll() = %d, revents = 0x%hx (POLLIN=%d)\n",
               pr, pfd.revents, (pfd.revents & POLLIN) != 0);
    }

    /* ============================================================
     * Step 8: 遍历 ring 中所有帧，处理 TP_STATUS_USER 帧
     *
     * V1/V2 ring 是平坦帧数组。frame i 起始地址 = base + i * tp_frame_size。
     * 每帧以 struct tpacket_hdr(V1) 或 struct tpacket2_hdr(V2) 开头。
     * tp_status==TP_STATUS_USER 表示内核已填好数据，用户可读。
     * tp_mac 给出 MAC header 在帧内的偏移，数据从该偏移开始。
     * ============================================================ */
    int frames_read = 0;

    for (unsigned int i = 0; i < FRAME_NR; i++) {
        unsigned char *frame_base = (unsigned char *)ring + i * FRAME_SIZE;

        /* 根据版本提取公共字段到局部变量。
         * V1: struct tpacket_hdr, tp_status 是 unsigned long, 用 tp_usec, 无 VLAN
         * V2: struct tpacket2_hdr, tp_status 是 unsigned int, 用 tp_nsec, 有 VLAN */
        unsigned long tp_status;
        unsigned int tp_len, tp_snaplen, tp_mac, tp_net, tp_sec, tp_subsec;
        unsigned int tp_vlan_tci = 0, tp_vlan_tpid = 0;

        if (is_v1) {
            struct tpacket_hdr *h = (struct tpacket_hdr *)frame_base;
            tp_status  = h->tp_status;
            tp_len     = h->tp_len;
            tp_snaplen = h->tp_snaplen;
            tp_mac     = h->tp_mac;
            tp_net     = h->tp_net;
            tp_sec     = h->tp_sec;
            tp_subsec  = h->tp_usec;
        } else {
            struct tpacket2_hdr *h = (struct tpacket2_hdr *)frame_base;
            tp_status  = h->tp_status;
            tp_len     = h->tp_len;
            tp_snaplen = h->tp_snaplen;
            tp_mac     = h->tp_mac;
            tp_net     = h->tp_net;
            tp_sec     = h->tp_sec;
            tp_subsec  = h->tp_nsec;
            tp_vlan_tci  = h->tp_vlan_tci;
            tp_vlan_tpid = h->tp_vlan_tpid;
        }

        if (tp_status != TP_STATUS_USER)
            continue;

        frames_read++;

        printf("[OK] Step 8: Frame %u: tp_status=USER "
               "tp_len=%u tp_snaplen=%u tp_mac=%u tp_net=%u "
               "tp_sec=%u %s=%u",
               i, tp_len, tp_snaplen, tp_mac, tp_net,
               tp_sec, is_v1 ? "tp_usec" : "tp_nsec", tp_subsec);

        if (!is_v1 && (tp_vlan_tci || tp_vlan_tpid)) {
            printf(" vlan_tci=0x%04x vlan_tpid=0x%04x",
                   tp_vlan_tci, tp_vlan_tpid);
        }
        printf("\n");

        /* 用 tp_mac 定位数据，打印前 DUMP_BYTES 字节 */
        if (tp_mac > 0 && tp_mac + DUMP_BYTES <= (i + 1) * FRAME_SIZE) {
            unsigned char *data = frame_base + tp_mac;
            unsigned int to_print = tp_snaplen;
            if (to_print > DUMP_BYTES)
                to_print = DUMP_BYTES;
            printf("  Data (%u/%u bytes at tp_mac=%u):",
                   to_print, tp_snaplen, tp_mac);
            for (unsigned int j = 0; j < to_print; j++)
                printf(" %02x", data[j]);
            printf("\n");
        }

        /* ============================================================
         * Step 8a: 校验收到的帧与自发帧一致
         *
         * 比较 tp_mac 起始的 SENT_FRAME_LEN 字节与 sent_frame。
         * 只有数据匹配才视为真正抓到包。
         * ============================================================ */
        if (tp_mac > 0 &&
            tp_mac + SENT_FRAME_LEN <= (i + 1) * FRAME_SIZE &&
            tp_snaplen >= SENT_FRAME_LEN) {
            unsigned char *data = frame_base + tp_mac;
            if (memcmp(data, sent_frame, SENT_FRAME_LEN) == 0) {
                verified = 1;
                printf("  -> Frame %u verified: matches sent frame (%d bytes)\n",
                       i, SENT_FRAME_LEN);
            } else {
                printf("  -> Frame %u data mismatch (expected broadcast ETH_P_IP + \"TEST\")\n",
                       i);
            }
        } else if (tp_snaplen < SENT_FRAME_LEN) {
            printf("  -> Frame %u too short (%u < %d), cannot verify\n",
                   i, tp_snaplen, SENT_FRAME_LEN);
        }

        /* ============================================================
         * Step 9: 将帧翻回内核 (TP_STATUS_KERNEL)
         *
         * tp_status 在两种版本中都位于帧起始(offset 0)，但宽度不同
         * (V1=unsigned long, V2=unsigned int)，须用对应类型回写，
         * 否则 V2 会越界覆盖 tp_len。
         * ============================================================ */
        __sync_synchronize();
        if (is_v1)
            ((struct tpacket_hdr *)frame_base)->tp_status = TP_STATUS_KERNEL;
        else
            ((struct tpacket2_hdr *)frame_base)->tp_status = TP_STATUS_KERNEL;
        printf("  -> Frame %u returned to kernel\n", i);
    }

    if (frames_read == 0 && pr > 0)
        printf("[WARN] Step 8: poll returned but no USER frames found\n");

    /* ============================================================
     * Step 10: getsockopt(PACKET_STATISTICS)
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
    printf("[OK] Step 10: PACKET_STATISTICS: tp_packets=%u tp_drops=%u\n",
           stats.tp_packets, stats.tp_drops);

    if (verified) {
        printf("[PASS] received and verified self-sent frame on ring buffer\n");
        rc = 0;
    } else if (stats.tp_packets > 0) {
        printf("[INFO] tp_packets > 0 but frame data not verified\n");
    } else {
        printf("[INFO] tp_packets == 0 (no traffic captured during test window)\n");
    }

cleanup:
    /* ============================================================
     * Step 11: 清理 — munmap + close
     * ============================================================ */
    if (ring != MAP_FAILED) {
        munmap(ring, RING_SIZE);
        printf("[OK] Step 11: munmap(%d)\n", RING_SIZE);
    }
    if (fd >= 0) {
        close(fd);
        printf("[OK] Step 11: close(%d)\n", fd);
    }

    printf("\n=== packet_ring_test %s ===\n",
           rc ? "FAILED" : "PASSED");
    return rc;
}
