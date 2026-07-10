/**
 * test_af_packet_mcast.c - AF_PACKET multicast membership 功能测试
 *
 * 验证 setsockopt(SOL_PACKET, PACKET_ADD/DROP_MEMBERSHIP, ...) 对各种
 * mr_type (PROMISC / MULTICAST / ALLMULTI) 的处理，以及无效 mr_type /
 * ifindex 的错误返回。
 *
 * 对应内核实现: kernel/src/net/socket/packet/mod.rs set_membership()
 *   - PACKET_MR_PROMISC   -> 设置/清除 InterfaceFlags::PROMISC
 *   - PACKET_MR_ALLMULTI  -> 设置/清除 InterfaceFlags::ALLMULTI
 *   - PACKET_MR_MULTICAST -> 接受 (暂不做硬件过滤)
 *   - PACKET_MR_UNICAST   -> 接受
 *   - 其它 mr_type        -> EINVAL
 *   - ifindex 找不到      -> ENODEV (find_iface)
 *
 * DragonOS musl 可能没有 <linux/if_packet.h>，故手动定义 packet_mreq。
 * 获取 ifindex 用 ioctl(SIOCGIFINDEX)。
 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/ioctl.h>
#include <net/if.h>
#include <arpa/inet.h>

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

/* packet_mreq 的 mr_type 常量 (对应 Linux if_packet.h) */
#define PACKET_MR_PROMISC 0
#define PACKET_MR_MULTICAST 1
#define PACKET_MR_ALLMULTI 2
#define PACKET_MR_UNICAST 3

/* SIOCGIFINDEX 在 <linux/sockios.h>，musl <net/if.h> 不一定暴露 */
#ifndef SIOCGIFINDEX
#define SIOCGIFINDEX 0x8933
#endif

#ifndef IFNAMSIZ
#define IFNAMSIZ 16
#endif

/*
 * 手动定义 struct packet_mreq (对应 Linux struct packet_mreq)。
 * 布局: mr_ifindex(u32) + mr_type(u32) + mr_alen(u16) + mr_address[8]。
 * 用 packed 之外的标准对齐即可 (内核用 #[repr(C)])。
 */
struct packet_mreq_manual {
    unsigned int mr_ifindex;
    unsigned int mr_type;
    unsigned short mr_alen;
    unsigned char mr_address[8];
};

/* 暴力枚举 eth0-eth20，用 ioctl(SIOCGIFINDEX) 验证存在性。
 * DragonOS 没有 /proc/net/dev，且接口名可能不稳定。 */
static const char *discover_ifname(void) {
    static char name[32] = "eth0";
    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock < 0) return name;
    for (int i = 0; i <= 20; i++) {
        snprintf(name, sizeof(name), "eth%d", i);
        struct ifreq ifr;
        memset(&ifr, 0, sizeof(ifr));
        strncpy(ifr.ifr_name, name, IFNAMSIZ - 1);
        if (ioctl(sock, SIOCGIFINDEX, &ifr) == 0) {
            close(sock);
            printf("[INFO] discover_ifname: found %s (ifindex=%d)\n", name, ifr.ifr_ifindex);
            return name;
        }
    }
    close(sock);
    strcpy(name, "eth0");
    return name;
}

/* ---- 测试辅助宏 (与 test_af_packet.c 风格一致) ---- */

static int fail_count = 0;
static int pass_count = 0;

#define TEST_PASS(name) do { printf("[PASS] %s\n", name); pass_count++; } while (0)
#define TEST_FAIL(name, msg) \
    do { printf("[FAIL] %s: %s (errno=%d)\n", name, msg, errno); fail_count++; } while (0)

/* ---- 工具函数 ---- */

/*
 * 获取网卡 ifindex: ioctl(SIOCGIFINDEX)。
 * 用一个临时 AF_INET/DGRAM 控制套接字发送 ioctl。
 * 成功返回 ifindex (>=1)，失败返回 -1。
 */
static int get_ifindex(const char *ifname)
{
    struct ifreq ifr;
    memset(&ifr, 0, sizeof(ifr));
    strncpy(ifr.ifr_name, ifname, IFNAMSIZ - 1);
    ifr.ifr_name[IFNAMSIZ - 1] = '\0';

    int ctrl_fd = socket(AF_INET, SOCK_DGRAM, 0);
    if (ctrl_fd < 0) {
        return -1;
    }
    int rc = ioctl(ctrl_fd, SIOCGIFINDEX, &ifr);
    close(ctrl_fd);
    if (rc < 0) {
        return -1;
    }
    return ifr.ifr_ifindex;
}

int main(void)
{
    const char *ifname = discover_ifname();
    printf("===== AF_PACKET Multicast Membership 测试 =====\n\n");

    /* 获取网卡 ifindex */
    int ifindex = get_ifindex(ifname);
    if (ifindex < 0) {
        printf("[FAIL] 获取 %s ifindex: %s (errno=%d)\n", ifname, strerror(errno), errno);
        printf("\n无法继续: 测试需要网卡且内核支持 SIOCGIFINDEX ioctl。\n");
        printf("通过: 0, 失败: 1\n");
        return 1;
    }
    printf("%s ifindex = %d\n", ifname, ifindex);

    /* 创建 AF_PACKET SOCK_RAW socket */
    int fd = socket(AF_PACKET, SOCK_RAW, htons(MY_ETH_P_ALL));
    if (fd < 0) {
        printf("[FAIL] socket(AF_PACKET, SOCK_RAW): %s (errno=%d)\n", strerror(errno), errno);
        printf("\n注意: 创建 AF_PACKET socket 需要 CAP_NET_RAW 权限，请在 root 下运行。\n");
        printf("通过: 0, 失败: 1\n");
        return 1;
    }
    TEST_PASS("socket(AF_PACKET, SOCK_RAW, ETH_P_ALL)");

    struct packet_mreq_manual mreq;
    int rc;

    /* ===== Test 1: ADD_MEMBERSHIP PROMISC ===== */
    printf("\n--- Test 1: ADD_MEMBERSHIP (PACKET_MR_PROMISC) ---\n");
    memset(&mreq, 0, sizeof(mreq));
    mreq.mr_ifindex = (unsigned int)ifindex;
    mreq.mr_type = PACKET_MR_PROMISC;
    errno = 0;
    rc = setsockopt(fd, SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq));
    if (rc == 0) {
        TEST_PASS("ADD_MEMBERSHIP PROMISC 返回 0");
    } else {
        TEST_FAIL("ADD_MEMBERSHIP PROMISC", strerror(errno));
    }

    /* ===== Test 2: DROP_MEMBERSHIP PROMISC ===== */
    printf("\n--- Test 2: DROP_MEMBERSHIP (PACKET_MR_PROMISC) ---\n");
    /* 复用 Test 1 的 mreq (同 ifindex + PROMISC) */
    errno = 0;
    rc = setsockopt(fd, SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq));
    if (rc == 0) {
        TEST_PASS("DROP_MEMBERSHIP PROMISC 返回 0 (恢复接口标志)");
    } else {
        TEST_FAIL("DROP_MEMBERSHIP PROMISC", strerror(errno));
    }

    /* ===== Test 3: ADD_MEMBERSHIP ALLMULTI ===== */
    printf("\n--- Test 3: ADD/DROP MEMBERSHIP (PACKET_MR_ALLMULTI) ---\n");
    memset(&mreq, 0, sizeof(mreq));
    mreq.mr_ifindex = (unsigned int)ifindex;
    mreq.mr_type = PACKET_MR_ALLMULTI;
    errno = 0;
    rc = setsockopt(fd, SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq));
    if (rc == 0) {
        TEST_PASS("ADD_MEMBERSHIP ALLMULTI 返回 0");
    } else {
        TEST_FAIL("ADD_MEMBERSHIP ALLMULTI", strerror(errno));
    }
    /* DROP 清除 */
    errno = 0;
    rc = setsockopt(fd, SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq));
    if (rc == 0) {
        TEST_PASS("DROP_MEMBERSHIP ALLMULTI 返回 0 (恢复)");
    } else {
        TEST_FAIL("DROP_MEMBERSHIP ALLMULTI", strerror(errno));
    }

    /* ===== Test 4: 无效 mr_type ===== */
    printf("\n--- Test 4: 无效 mr_type (999) 应返回 EINVAL ---\n");
    memset(&mreq, 0, sizeof(mreq));
    mreq.mr_ifindex = (unsigned int)ifindex;
    mreq.mr_type = 999; /* 非法类型 */
    errno = 0;
    rc = setsockopt(fd, SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq));
    if (rc == -1 && errno == EINVAL) {
        TEST_PASS("无效 mr_type=999 返回 -1/EINVAL");
    } else {
        char buf[64];
        snprintf(buf, sizeof(buf), "期望 rc=-1 errno=EINVAL(22), 实际 rc=%d errno=%d", rc, errno);
        TEST_FAIL("无效 mr_type", buf);
    }

    /* ===== Test 5: 无效 ifindex ===== */
    printf("\n--- Test 5: 无效 ifindex (99999) 应返回错误 ---\n");
    memset(&mreq, 0, sizeof(mreq));
    mreq.mr_ifindex = 99999; /* 不存在的接口 */
    mreq.mr_type = PACKET_MR_PROMISC;
    errno = 0;
    rc = setsockopt(fd, SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq));
    /* 内核 find_iface 返回 ENODEV; 容忍 ENXIO/EINVAL 作为合理错误码 */
    if (rc == -1 && (errno == ENODEV || errno == ENXIO || errno == EINVAL)) {
        char buf[64];
        snprintf(buf, sizeof(buf), "无效 ifindex 返回 -1/errno=%d (%s) 合理", errno, strerror(errno));
        TEST_PASS(buf);
    } else {
        char buf[64];
        snprintf(buf, sizeof(buf), "期望 -1/ENODEV, 实际 rc=%d errno=%d", rc, errno);
        TEST_FAIL("无效 ifindex", buf);
    }

    /* ===== Test 6: MULTICAST 类型 (特定多播 MAC) ===== */
    printf("\n--- Test 6: ADD/DROP MEMBERSHIP (PACKET_MR_MULTICAST) ---\n");
    memset(&mreq, 0, sizeof(mreq));
    mreq.mr_ifindex = (unsigned int)ifindex;
    mreq.mr_type = PACKET_MR_MULTICAST;
    mreq.mr_alen = 6; /* 以太网 MAC 长度 */
    /* 多播 MAC: 01:00:5e:00:00:01 (IGMP/多播组映射) */
    mreq.mr_address[0] = 0x01;
    mreq.mr_address[1] = 0x00;
    mreq.mr_address[2] = 0x5e;
    mreq.mr_address[3] = 0x00;
    mreq.mr_address[4] = 0x00;
    mreq.mr_address[5] = 0x01;
    errno = 0;
    rc = setsockopt(fd, SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq));
    if (rc == 0) {
        TEST_PASS("ADD_MEMBERSHIP MULTICAST (01:00:5e:00:00:01) 返回 0");
    } else {
        TEST_FAIL("ADD_MEMBERSHIP MULTICAST", strerror(errno));
    }
    /* DROP 清除 */
    errno = 0;
    rc = setsockopt(fd, SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq));
    if (rc == 0) {
        TEST_PASS("DROP_MEMBERSHIP MULTICAST 返回 0");
    } else {
        TEST_FAIL("DROP_MEMBERSHIP MULTICAST", strerror(errno));
    }

    close(fd);

    /* ===== 汇总 ===== */
    printf("\n===== AF_PACKET Multicast Membership 测试结果 =====\n");
    printf("通过: %d, 失败: %d\n", pass_count, fail_count);
    return fail_count > 0 ? 1 : 0;
}
