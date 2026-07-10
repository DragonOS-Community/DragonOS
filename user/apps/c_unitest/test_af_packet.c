/**
 * test_af_packet.c - AF_PACKET socket 功能测试
 *
 * 用于验证 DragonOS AF_PACKET 实现 (issue #2034/#2028/#2029)：
 *   1. socket 创建 (SOCK_RAW / SOCK_DGRAM)
 *   2. setsockopt/getsockopt 各 option 往返
 *   3. 无效 option 返回 ENOPROTOOPT
 *   4. PACKET_VERSION 非法值返回 EINVAL
 *   5. PACKET_STATISTICS 返回正确结构 (两个 u32 = 8 字节)
 *
 * 手动定义所有常量，不依赖 <linux/if_packet.h>，保证 musl 静态编译可用。
 *
 * 编译: x86_64-linux-musl-gcc -Wall -O2 -static -lpthread test_af_packet.c -o test_af_packet
 * 运行需 CAP_NET_RAW 权限 (DragonOS 下默认 root 即可)。
 */

#include <stdio.h>
#include <string.h>
#include <errno.h>
#include <stdlib.h>
#include <stdint.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/uio.h>   /* struct iovec / recvmsg */
#include <arpa/inet.h> /* htons */
#include <unistd.h>

#ifndef AF_PACKET
#define AF_PACKET 17
#endif

#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif

/* 以太网协议: ETH_P_ALL = 0x0003 (接收所有协议) */
#define MY_ETH_P_ALL 0x0003

/* SOL_PACKET 级别 socket 选项 (对应 Linux if_packet.h) */
#define PACKET_ADD_MEMBERSHIP 1
#define PACKET_DROP_MEMBERSHIP 2
#define PACKET_STATISTICS 6
#define PACKET_COPY_THRESH 7
#define PACKET_AUXDATA 8
#define PACKET_ORIGDEV 9
#define PACKET_VERSION 10
#define PACKET_RESERVE 12
#define PACKET_VNET_HDR 15
#define PACKET_TX_TIMESTAMP 16
#define PACKET_TIMESTAMP 17
#define PACKET_QDISC_BYPASS 20

/* TPACKET 版本 (PACKET_VERSION 取值) */
#define TPACKET_V1 0
#define TPACKET_V2 1
#define TPACKET_V3 2

/* Linux errno 数值 (用于显式断言) */
#define MY_ENOPROTOOPT 92
#define MY_EINVAL 22
#define MY_ENOSYS 38

/* 测试辅助宏 */
static int fail_count = 0;
static int pass_count = 0;

#define TEST_PASS(name) do { printf("[PASS] %s\n", name); pass_count++; } while (0)
#define TEST_FAIL(name, msg) \
    do { printf("[FAIL] %s: %s (errno=%d)\n", name, msg, errno); fail_count++; } while (0)

/* ---- setsockopt/getsockopt 整型辅助函数 ---- */

static int set_int_opt(int fd, int opt, int val)
{
    int v = val;
    return setsockopt(fd, SOL_PACKET, opt, &v, sizeof(v));
}

static int get_int_opt(int fd, int opt, int *val)
{
    socklen_t len = sizeof(int);
    return getsockopt(fd, SOL_PACKET, opt, val, &len);
}

int main(void)
{
    /* ===== Test 1: 创建 AF_PACKET socket ===== */
    printf("\n--- Test 1: 创建 AF_PACKET socket ---\n");

    int raw_fd = socket(AF_PACKET, SOCK_RAW, htons(MY_ETH_P_ALL));
    if (raw_fd < 0) {
        /* 没有 CAP_NET_RAW 时 socket 会失败 (EPERM)，后续测试无法进行 */
        printf("[FAIL] socket(SOCK_RAW): %s (errno=%d)\n", strerror(errno), errno);
        printf("\n注意: 创建 AF_PACKET socket 需要 CAP_NET_RAW 权限，请在 root 下运行。\n");
        fail_count++;
        printf("\n===== AF_PACKET 测试结果 =====\n");
        printf("通过: %d, 失败: %d\n", pass_count, fail_count);
        return fail_count > 0 ? 1 : 0;
    }
    TEST_PASS("socket(AF_PACKET, SOCK_RAW, ETH_P_ALL)");

    int dgram_fd = socket(AF_PACKET, SOCK_DGRAM, htons(MY_ETH_P_ALL));
    if (dgram_fd < 0) {
        TEST_FAIL("socket(AF_PACKET, SOCK_DGRAM, ETH_P_ALL)", strerror(errno));
    } else {
        TEST_PASS("socket(AF_PACKET, SOCK_DGRAM, ETH_P_ALL)");
        close(dgram_fd);
    }

    /* ===== Test 2: PACKET_AUXDATA 往返 ===== */
    printf("\n--- Test 2: PACKET_AUXDATA 往返 ---\n");
    {
        int val = 1;
        if (setsockopt(raw_fd, SOL_PACKET, PACKET_AUXDATA, &val, sizeof(val)) != 0) {
            TEST_FAIL("setsockopt(PACKET_AUXDATA=1)", strerror(errno));
        } else {
            int got = -1;
            if (get_int_opt(raw_fd, PACKET_AUXDATA, &got) != 0) {
                TEST_FAIL("getsockopt(PACKET_AUXDATA)", strerror(errno));
            } else if (got != 1) {
                char buf[64];
                snprintf(buf, sizeof(buf), "期望 1, 实际 %d", got);
                TEST_FAIL("PACKET_AUXDATA 往返值", buf);
            } else {
                TEST_PASS("PACKET_AUXDATA set=1 -> get=1");
            }
        }
        /* 再测关闭 */
        val = 0;
        if (setsockopt(raw_fd, SOL_PACKET, PACKET_AUXDATA, &val, sizeof(val)) == 0) {
            int got = -1;
            if (get_int_opt(raw_fd, PACKET_AUXDATA, &got) == 0 && got == 0) {
                TEST_PASS("PACKET_AUXDATA set=0 -> get=0");
            } else {
                TEST_FAIL("PACKET_AUXDATA set=0 往返", "返回值非 0");
            }
        }
    }

    /* ===== Test 3: PACKET_VERSION ===== */
    printf("\n--- Test 3: PACKET_VERSION ---\n");
    {
        /* TPACKET_V2 = 1 */
        if (set_int_opt(raw_fd, PACKET_VERSION, TPACKET_V2) != 0) {
            TEST_FAIL("setsockopt(PACKET_VERSION=V2)", strerror(errno));
        } else {
            int got = -1;
            if (get_int_opt(raw_fd, PACKET_VERSION, &got) != 0) {
                TEST_FAIL("getsockopt(PACKET_VERSION V2)", strerror(errno));
            } else if (got != TPACKET_V2) {
                char buf[64];
                snprintf(buf, sizeof(buf), "期望 %d, 实际 %d", TPACKET_V2, got);
                TEST_FAIL("PACKET_VERSION V2 往返值", buf);
            } else {
                TEST_PASS("PACKET_VERSION=TPACKET_V2 往返");
            }
        }

        /* TPACKET_V3 = 2 */
        if (set_int_opt(raw_fd, PACKET_VERSION, TPACKET_V3) != 0) {
            TEST_FAIL("setsockopt(PACKET_VERSION=V3)", strerror(errno));
        } else {
            int got = -1;
            if (get_int_opt(raw_fd, PACKET_VERSION, &got) != 0) {
                TEST_FAIL("getsockopt(PACKET_VERSION V3)", strerror(errno));
            } else if (got != TPACKET_V3) {
                char buf[64];
                snprintf(buf, sizeof(buf), "期望 %d, 实际 %d", TPACKET_V3, got);
                TEST_FAIL("PACKET_VERSION V3 往返值", buf);
            } else {
                TEST_PASS("PACKET_VERSION=TPACKET_V3 往返");
            }
        }

        /* 非法值 999 应返回 EINVAL */
        errno = 0;
        int rc = set_int_opt(raw_fd, PACKET_VERSION, 999);
        if (rc != -1) {
            TEST_FAIL("PACKET_VERSION=999 应失败", "返回非 -1");
        } else if (errno != EINVAL) {
            char buf[64];
            snprintf(buf, sizeof(buf), "期望 EINVAL(%d), 实际 errno=%d", MY_EINVAL, errno);
            TEST_FAIL("PACKET_VERSION=999 errno", buf);
        } else {
            TEST_PASS("PACKET_VERSION=999 返回 EINVAL");
        }
    }

    /* ===== Test 4: COPY_THRESH / RESERVE ===== */
    printf("\n--- Test 4: COPY_THRESH / RESERVE ---\n");
    {
        /* COPY_THRESH = 4096 */
        if (set_int_opt(raw_fd, PACKET_COPY_THRESH, 4096) != 0) {
            TEST_FAIL("setsockopt(PACKET_COPY_THRESH=4096)", strerror(errno));
        } else {
            int got = -1;
            if (get_int_opt(raw_fd, PACKET_COPY_THRESH, &got) != 0) {
                TEST_FAIL("getsockopt(PACKET_COPY_THRESH)", strerror(errno));
            } else if (got != 4096) {
                char buf[64];
                snprintf(buf, sizeof(buf), "期望 4096, 实际 %d", got);
                TEST_FAIL("PACKET_COPY_THRESH 往返值", buf);
            } else {
                TEST_PASS("PACKET_COPY_THRESH=4096 往返");
            }
        }

        /* RESERVE = 128 */
        if (set_int_opt(raw_fd, PACKET_RESERVE, 128) != 0) {
            TEST_FAIL("setsockopt(PACKET_RESERVE=128)", strerror(errno));
        } else {
            int got = -1;
            if (get_int_opt(raw_fd, PACKET_RESERVE, &got) != 0) {
                TEST_FAIL("getsockopt(PACKET_RESERVE)", strerror(errno));
            } else if (got != 128) {
                char buf[64];
                snprintf(buf, sizeof(buf), "期望 128, 实际 %d", got);
                TEST_FAIL("PACKET_RESERVE 往返值", buf);
            } else {
                TEST_PASS("PACKET_RESERVE=128 往返");
            }
        }
    }

    /* ===== Test 5: bool 选项 (ORIGDEV / VNET_HDR / QDISC_BYPASS) ===== */
    printf("\n--- Test 5: bool 选项 (ORIGDEV / VNET_HDR / QDISC_BYPASS) ---\n");
    {
        struct bool_opt {
            const char *name;
            int opt;
        } bools[] = {
            { "PACKET_ORIGDEV", PACKET_ORIGDEV },
            { "PACKET_VNET_HDR", PACKET_VNET_HDR },
            { "PACKET_QDISC_BYPASS", PACKET_QDISC_BYPASS },
        };
        for (size_t i = 0; i < sizeof(bools) / sizeof(bools[0]); i++) {
            /* 设 1 期望返回 1 */
            if (set_int_opt(raw_fd, bools[i].opt, 1) != 0) {
                TEST_FAIL(bools[i].name, "setsockopt 失败");
                continue;
            }
            int got = -1;
            if (get_int_opt(raw_fd, bools[i].opt, &got) != 0) {
                TEST_FAIL(bools[i].name, "getsockopt 失败");
                continue;
            }
            if (got != 1) {
                char buf[64];
                snprintf(buf, sizeof(buf), "set=1 期望 get=1, 实际 %d", got);
                TEST_FAIL(bools[i].name, buf);
                continue;
            }
            /* 设 0 期望返回 0 */
            set_int_opt(raw_fd, bools[i].opt, 0);
            got = -1;
            get_int_opt(raw_fd, bools[i].opt, &got);
            if (got != 0) {
                char buf[64];
                snprintf(buf, sizeof(buf), "set=0 期望 get=0, 实际 %d", got);
                TEST_FAIL(bools[i].name, buf);
                continue;
            }
            char passname[64];
            snprintf(passname, sizeof(passname), "%s 0/1 往返", bools[i].name);
            TEST_PASS(passname);
        }
    }

    /* ===== Test 6: TX_TIMESTAMP / TIMESTAMP ===== */
    printf("\n--- Test 6: TX_TIMESTAMP / TIMESTAMP ---\n");
    {
        /* TX_TIMESTAMP = -1: DragonOS 内核按原始 i32 存储, 往返 -1。
         * 注意: 真实 Linux 不支持将该 option 作为可写整数 (返回 ENOPROTOOPT),
         * 因此本用例仅针对 DragonOS 行为; 在原生 Linux 主机上会失败属预期。 */
        if (set_int_opt(raw_fd, PACKET_TX_TIMESTAMP, -1) != 0) {
            TEST_FAIL("setsockopt(PACKET_TX_TIMESTAMP=-1)", strerror(errno));
        } else {
            int got = 0;
            if (get_int_opt(raw_fd, PACKET_TX_TIMESTAMP, &got) != 0) {
                TEST_FAIL("getsockopt(PACKET_TX_TIMESTAMP)", strerror(errno));
            } else if (got != -1) {
                char buf[64];
                snprintf(buf, sizeof(buf), "期望 -1, 实际 %d", got);
                TEST_FAIL("PACKET_TX_TIMESTAMP 往返值", buf);
            } else {
                TEST_PASS("PACKET_TX_TIMESTAMP=-1 往返");
            }
        }

        /* TIMESTAMP = 1 */
        if (set_int_opt(raw_fd, PACKET_TIMESTAMP, 1) != 0) {
            TEST_FAIL("setsockopt(PACKET_TIMESTAMP=1)", strerror(errno));
        } else {
            int got = 0;
            if (get_int_opt(raw_fd, PACKET_TIMESTAMP, &got) != 0) {
                TEST_FAIL("getsockopt(PACKET_TIMESTAMP)", strerror(errno));
            } else if (got != 1) {
                char buf[64];
                snprintf(buf, sizeof(buf), "期望 1, 实际 %d", got);
                TEST_FAIL("PACKET_TIMESTAMP 往返值", buf);
            } else {
                TEST_PASS("PACKET_TIMESTAMP=1 往返");
            }
        }
    }

    /* ===== Test 7: PACKET_STATISTICS ===== */
    printf("\n--- Test 7: PACKET_STATISTICS ---\n");
    {
        struct {
            uint32_t tp_packets;
            uint32_t tp_drops;
        } stats;
        memset(&stats, 0xff, sizeof(stats));
        socklen_t len = sizeof(stats);
        if (getsockopt(raw_fd, SOL_PACKET, PACKET_STATISTICS, &stats, &len) != 0) {
            TEST_FAIL("getsockopt(PACKET_STATISTICS)", strerror(errno));
        } else if (len != 8) {
            char buf[64];
            snprintf(buf, sizeof(buf), "期望 len=8, 实际 %u", (unsigned)len);
            TEST_FAIL("PACKET_STATISTICS 长度", buf);
        } else {
            printf("    统计: tp_packets=%u, tp_drops=%u\n",
                   (unsigned)stats.tp_packets, (unsigned)stats.tp_drops);
            /* 新建 socket 且未收发数据, 初始应为 0 */
            if (stats.tp_packets == 0 && stats.tp_drops == 0) {
                TEST_PASS("PACKET_STATISTICS 返回 8 字节且初始为 0");
            } else {
                TEST_PASS("PACKET_STATISTICS 返回 8 字节结构 (值非 0 但合理)");
            }
        }
    }

    /* ===== Test 8: 无效 option 返回 ENOPROTOOPT ===== */
    printf("\n--- Test 8: 无效 option 返回 ENOPROTOOPT ---\n");
    {
        int val = 1;
        errno = 0;
        int rc = setsockopt(raw_fd, SOL_PACKET, 9999, &val, sizeof(val));
        if (rc != -1) {
            TEST_FAIL("setsockopt(opt=9999)", "应返回 -1");
        } else if (errno != ENOPROTOOPT) {
            char buf[64];
            snprintf(buf, sizeof(buf), "期望 ENOPROTOOPT(%d), 实际 errno=%d",
                     MY_ENOPROTOOPT, errno);
            TEST_FAIL("setsockopt(opt=9999) errno", buf);
        } else {
            TEST_PASS("setsockopt(opt=9999) 返回 ENOPROTOOPT");
        }

        int got = 0;
        socklen_t len = sizeof(got);
        errno = 0;
        rc = getsockopt(raw_fd, SOL_PACKET, 9999, &got, &len);
        if (rc != -1) {
            TEST_FAIL("getsockopt(opt=9999)", "应返回 -1");
        } else if (errno != ENOPROTOOPT) {
            char buf[64];
            snprintf(buf, sizeof(buf), "期望 ENOPROTOOPT(%d), 实际 errno=%d",
                     MY_ENOPROTOOPT, errno);
            TEST_FAIL("getsockopt(opt=9999) errno", buf);
        } else {
            TEST_PASS("getsockopt(opt=9999) 返回 ENOPROTOOPT");
        }
    }

    /* ===== Test 9: recvmsg 不返回 ENOSYS ===== */
    printf("\n--- Test 9: recvmsg 不返回 ENOSYS ---\n");
    {
        char rbuf[2048];
        struct iovec iov;
        iov.iov_base = rbuf;
        iov.iov_len = sizeof(rbuf);
        struct msghdr msg;
        memset(&msg, 0, sizeof(msg));
        msg.msg_iov = &iov;
        msg.msg_iovlen = 1;

        errno = 0;
        /* MSG_DONTWAIT: 无数据时立即返回 EAGAIN, 不应阻塞 */
        ssize_t n = recvmsg(raw_fd, &msg, MSG_DONTWAIT);
        if (n >= 0) {
            /* 有数据则也算 recvmsg 正常工作 */
            printf("    recvmsg 返回 %zd 字节 (有数据)\n", n);
            TEST_PASS("recvmsg 可调用, 未返回 ENOSYS");
        } else if (errno == ENOSYS) {
            TEST_FAIL("recvmsg", "返回 ENOSYS (功能未实现)");
        } else if (errno == EAGAIN || errno == EWOULDBLOCK) {
            TEST_PASS("recvmsg 无数据返回 EAGAIN/EWOULDBLOCK (未返回 ENOSYS)");
        } else {
            char buf[64];
            snprintf(buf, sizeof(buf), "errno=%d (%s)", errno, strerror(errno));
            TEST_FAIL("recvmsg", buf);
        }
    }

    close(raw_fd);

    /* ===== 汇总 ===== */
    printf("\n===== AF_PACKET 测试结果 =====\n");
    printf("通过: %d, 失败: %d\n", pass_count, fail_count);
    return fail_count > 0 ? 1 : 0;
}
