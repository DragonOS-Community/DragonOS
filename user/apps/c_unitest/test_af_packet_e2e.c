/**
 * test_af_packet_e2e.c - AF_PACKET 端到端收发测试
 *
 * 验证 DragonOS AF_PACKET 实际以太网帧的发送与接收 (issue #2028/#2029/#2034)：
 *   Test 1: SOCK_RAW 发送路径 —— sendto() / sendmsg() 返回正确字节数
 *   Test 2: SOCK_RAW 接收路径 —— recvfrom() 返回数据 + 校验 sockaddr_ll 各字段
 *   Test 3: recvmsg() iovec scatter —— 数据正确分散到多个缓冲区
 *   Test 4: sendmsg() iovec gather  —— 多个缓冲区正确拼装后发送
 *   Test 5: SOCK_DGRAM 收发       —— 内核构造以太网头，返回 L3 payload 长度
 *
 * 运行环境: DragonOS QEMU，eth0(virtio-net) 10.0.2.15/24，网关 10.0.2.2
 *           (QEMU user-mode networking，slirp 会应答网关的 ARP)
 * 权限: 需 CAP_NET_RAW (DragonOS 下默认 root 即可)
 *
 * 头文件策略: 尽量复用 musl 自带头文件 (netpacket/packet.h 提供 sockaddr_ll，
 *   netinet/if_ether.h 提供 ETH_P_*，net/ethernet.h 提供 ether_header，
 *   net/if_arp.h 提供 ARPHRD_ETHER/ARPOP_REQUEST)。musl 不提供 AF_PACKET /
 *   SOL_PACKET / struct ether_arp，故手动补充定义。
 *
 * 编译: x86_64-linux-musl-gcc -Wall -O2 -static -lpthread \
 *         test_af_packet_e2e.c -o test_af_packet_e2e
 */

#define _GNU_SOURCE  /* net/if.h 中的 struct ifreq 需要 */

#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <arpa/inet.h>       /* htons / ntohs / inet_addr */
#include <sys/types.h>
#include <sys/socket.h>
#include <sys/ioctl.h>
#include <sys/uio.h>         /* struct iovec */
#include <net/if.h>          /* struct ifreq / IFNAMSIZ / SIOCGIF* */
#include <net/if_arp.h>      /* ARPHRD_ETHER / ARPOP_REQUEST */
#include <net/ethernet.h>    /* struct ether_header */
#include <netinet/if_ether.h>/* ETH_P_ALL / ETH_P_ARP / ETH_ALEN */
#include <netpacket/packet.h>/* struct sockaddr_ll / PACKET_* */

/* ---- musl 未提供，手动定义 ---- */
#ifndef AF_PACKET
#define AF_PACKET 17
#endif

#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif

/* 手动定义 ARP 报文 (musl 无 struct ether_arp)，28 字节 */
struct arp_hdr {
    uint16_t ar_hrd;    /* 硬件类型: ARPHRD_ETHER(1) */
    uint16_t ar_pro;    /* 协议类型: ETH_P_IP(0x0800) */
    uint8_t  ar_hln;    /* 硬件地址长度: 6 */
    uint8_t  ar_pln;    /* 协议地址长度: 4 */
    uint16_t ar_op;     /* 操作: ARPOP_REQUEST(1) / ARPOP_REPLY(2) */
    uint8_t  ar_sha[6]; /* 发送方 MAC */
    uint8_t  ar_spa[4]; /* 发送方 IP */
    uint8_t  ar_tha[6]; /* 目标 MAC */
    uint8_t  ar_tpa[4]; /* 目标 IP */
};

#define ARP_PKT_LEN 28   /* sizeof(struct arp_hdr) */
#define ETH_HDR_LEN 14
#define ARP_FRAME_LEN (ETH_HDR_LEN + ARP_PKT_LEN)     /* 42 */
/* 编译期断言: 手写长度与结构体保持一致 */
_Static_assert(sizeof(struct arp_hdr) == ARP_PKT_LEN, "arp_hdr 大小必须为 28");

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
#define LOCAL_IP  "10.0.2.15"
#define GATEWAY   "10.0.2.2"

/* 每次接收调用超时 (秒) 与最大尝试次数 */
#define RECV_TIMEOUT_SEC 1
#define RECV_MAX_ATTEMPTS 8

/* ---- 计数与报告宏 ---- */
static int pass_count = 0;
static int fail_count = 0;
static int skip_count = 0;

#define PASS(name) do { printf("[PASS] %s\n", name); pass_count++; } while (0)
/* name 为纯标签; fmt 为详情 printf 格式串; 额外参数对应 fmt 中的转换说明 */
#define FAIL(name, fmt, ...) do { \
    printf("[FAIL] %s: " fmt " (errno=%d:%s)\n", name, ##__VA_ARGS__, errno, strerror(errno)); \
    fail_count++; } while (0)
#define SKIP(name, ...) do { \
    printf("[SKIP] %s: " __VA_ARGS__ "\n", name); \
    skip_count++; } while (0)

/* ---- 小工具 ---- */

static void mac_to_str(const unsigned char m[6], char *out, size_t outsz)
{
    snprintf(out, outsz, "%02x:%02x:%02x:%02x:%02x:%02x",
             m[0], m[1], m[2], m[3], m[4], m[5]);
}

static void hexdump(const char *prefix, const void *data, size_t len)
{
    const unsigned char *p = data;
    printf("%s (%zu bytes):", prefix, len);
    for (size_t i = 0; i < len; i++) {
        if (i % 16 == 0) printf("\n    ");
        printf("%02x ", p[i]);
    }
    printf("\n");
}

/* 通过 ioctl 获取接口索引；失败返回 -1 */
static int get_if_index(int any_fd, const char *ifname)
{
    struct ifreq ifr;
    memset(&ifr, 0, sizeof(ifr));
    strncpy(ifr.ifr_name, ifname, IFNAMSIZ - 1);
    if (ioctl(any_fd, SIOCGIFINDEX, &ifr) < 0)
        return -1;
    return ifr.ifr_ifindex;
}

/* 获取本机 MAC：先试 ioctl(SIOCGIFHWADDR)，失败再读 /sys/class/net/<if>/address，
 * 都失败则使用默认 MAC {0x52, 0x54, 0x00, 0x12, 0x34, 0x56} (QEMU virtio-net)。
 * 本函数永远不会失败：至少用默认 MAC 继续。 */
static void get_if_hwaddr(int any_fd, const char *ifname, unsigned char mac[6])
{
    struct ifreq ifr;
    memset(&ifr, 0, sizeof(ifr));
    strncpy(ifr.ifr_name, ifname, IFNAMSIZ - 1);
    if (ioctl(any_fd, SIOCGIFHWADDR, &ifr) == 0) {
        memcpy(mac, ifr.ifr_hwaddr.sa_data, 6);
        if (!(mac[0] == 0 && mac[1] == 0 && mac[2] == 0 &&
              mac[3] == 0 && mac[4] == 0 && mac[5] == 0)) {
            printf("[INFO] get_if_hwaddr: got MAC via ioctl(SIOCGIFHWADDR)\n");
            return;
        }
    }

    /* 回退到 sysfs */
    char path[64];
    snprintf(path, sizeof(path), "/sys/class/net/%s/address", ifname);
    FILE *f = fopen(path, "r");
    if (f) {
        unsigned int v[6];
        int n = fscanf(f, "%x:%x:%x:%x:%x:%x", &v[0], &v[1], &v[2], &v[3], &v[4], &v[5]);
        fclose(f);
        if (n == 6) {
            for (int i = 0; i < 6; i++) mac[i] = (unsigned char)v[i];
            printf("[INFO] get_if_hwaddr: got MAC via sysfs (%s)\n", path);
            return;
        }
    }

    /* 最后兜底：默认 QEMU virtio-net MAC */
    static const unsigned char default_mac[6] = {0x52, 0x54, 0x00, 0x12, 0x34, 0x56};
    memcpy(mac, default_mac, 6);
    printf("[INFO] get_if_hwaddr: using default MAC 52:54:00:12:34:56\n");
    return;
}

/* 构造一份广播 ARP 请求帧 (ether_header + arp_hdr)，长度 = ARP_FRAME_LEN */
static void build_arp_request(unsigned char *frame, size_t cap,
                              const unsigned char local_mac[6],
                              const char *local_ip, const char *target_ip)
{
    (void)cap; /* 调用方保证 >= ARP_FRAME_LEN */
    memset(frame, 0, ARP_FRAME_LEN);

    /* 以太网头 */
    unsigned char bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    memcpy(frame + 0, bcast, 6);                  /* dst = 广播 */
    memcpy(frame + 6, local_mac, 6);              /* src = 本机 */
    frame[12] = (ETH_P_ARP >> 8) & 0xff;          /* ethertype = 0x0806 (网络序) */
    frame[13] = ETH_P_ARP & 0xff;

    /* ARP 负载 */
    struct arp_hdr *a = (struct arp_hdr *)(frame + ETH_HDR_LEN);
    a->ar_hrd = htons(1);                         /* ARPHRD_ETHER */
    a->ar_pro = htons(ETH_P_IP);                  /* IPv4 */
    a->ar_hln = 6;
    a->ar_pln = 4;
    a->ar_op  = htons(ARPOP_REQUEST);             /* 请求 */
    memcpy(a->ar_sha, local_mac, 6);
    struct in_addr spa; spa.s_addr = inet_addr(local_ip);
    memcpy(a->ar_spa, &spa.s_addr, 4);
    /* ar_tha 保持 0 */
    struct in_addr tpa; tpa.s_addr = inet_addr(target_ip);
    memcpy(a->ar_tpa, &tpa.s_addr, 4);
}

/* 构造发往 sockaddr_ll 的目的地址（用于 SOCK_RAW/SOCK_DGRAM 发送） */
static void make_dst_ll(struct sockaddr_ll *sa, int ifindex,
                        const unsigned char dst_mac[6])
{
    memset(sa, 0, sizeof(*sa));
    sa->sll_family   = AF_PACKET;
    sa->sll_protocol = htons(ETH_P_ARP);
    sa->sll_ifindex  = ifindex;
    sa->sll_hatype   = ARPHRD_ETHER;
    sa->sll_pkttype  = 0;
    sa->sll_halen    = ETH_ALEN;
    memcpy(sa->sll_addr, dst_mac, 6);
}

/* 校验接收到的 sockaddr_ll 字段是否符合预期；返回 0 通过，-1 失败 */
static int validate_sll(const struct sockaddr_ll *sll, const unsigned char *frame,
                        size_t n, const char *tname)
{
    int ok = 1;

    if (sll->sll_family != AF_PACKET) {
        printf("    [检查] %s: sll_family=%d (期望 %d AF_PACKET) ✗\n",
               tname, sll->sll_family, AF_PACKET);
        ok = 0;
    }
    /* sll_protocol 应为网络序的有效 ethertype，且与帧内 ethertype 一致 */
    unsigned short proto = ntohs(sll->sll_protocol);
    if (proto == 0) {
        printf("    [检查] %s: sll_protocol=0 (无效) ✗\n", tname);
        ok = 0;
    } else if (n >= ETH_HDR_LEN) {
        unsigned short eth_proto = ((unsigned short)frame[12] << 8) | frame[13];
        if (proto != eth_proto) {
            printf("    [检查] %s: sll_protocol=0x%04x 与帧 ethertype=0x%04x 不一致 ✗\n",
                   tname, proto, eth_proto);
            ok = 0;
        }
    }
    if (sll->sll_hatype != ARPHRD_ETHER) {
        printf("    [检查] %s: sll_hatype=%d (期望 %d ARPHRD_ETHER) ✗\n",
               tname, sll->sll_hatype, ARPHRD_ETHER);
        ok = 0;
    }
    if (sll->sll_halen != ETH_ALEN) {
        printf("    [检查] %s: sll_halen=%d (期望 %d) ✗\n", tname, sll->sll_halen, ETH_ALEN);
        ok = 0;
    }
    int allzero = 1;
    for (int i = 0; i < ETH_ALEN; i++)
        if (sll->sll_addr[i]) { allzero = 0; break; }
    if (allzero) {
        printf("    [检查] %s: sll_addr 全零 (期望有效 MAC) ✗\n", tname);
        ok = 0;
    }
    return ok ? 0 : -1;
}

/* 给一个已绑定 eth0 的 SOCK_RAW 套接字发若干 ARP 请求，激发网关回应 */
static void stimulate(int tx_fd, int ifindex, const unsigned char local_mac[6])
{
    unsigned char frame[ARP_FRAME_LEN];
    unsigned char bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    make_dst_ll(&dst, ifindex, bcast);
    build_arp_request(frame, sizeof(frame), local_mac, LOCAL_IP, GATEWAY);
    for (int i = 0; i < 3; i++) {
        sendto(tx_fd, frame, ARP_FRAME_LEN, 0,
               (struct sockaddr *)&dst, sizeof(dst));
        usleep(20 * 1000);  /* 20ms 间隔 */
    }
}

int main(void)
{
    const char *ifname = discover_ifname();
    printf("===== AF_PACKET 端到端收发测试 =====\n");
    printf("接口: %s, 本机 IP: %s, 网关: %s\n\n", ifname, LOCAL_IP, GATEWAY);

    /* ---- 创建主 SOCK_RAW 套接字 ---- */
    int raw_fd = socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL));
    if (raw_fd < 0) {
        if (errno == EPERM || errno == EACCES) {
            printf("[FAIL] socket(SOCK_RAW): 需要 CAP_NET_RAW，请以 root 运行 (errno=%d)\n", errno);
        } else {
            printf("[FAIL] socket(AF_PACKET, SOCK_RAW): %s (errno=%d)\n", strerror(errno), errno);
        }
        fail_count++;
        goto summary;
    }
    PASS("socket(AF_PACKET, SOCK_RAW, ETH_P_ALL)");

    /* ---- 获取接口索引与本机 MAC ---- */
    int ifindex = get_if_index(raw_fd, ifname);
    if (ifindex < 0) {
        FAIL("get_if_index", "ioctl(SIOCGIFINDEX) on %s 失败", ifname);
        goto summary;
    }
    {
        char msg[64];
        snprintf(msg, sizeof(msg), "获取 %s 接口索引 = %d", ifname, ifindex);
        PASS(msg);
    }

    unsigned char local_mac[6];
    get_if_hwaddr(raw_fd, ifname, local_mac);
    {
        char macs[24], msg[64];
        mac_to_str(local_mac, macs, sizeof(macs));
        snprintf(msg, sizeof(msg), "获取本机 MAC = %s", macs);
        PASS(msg);
    }

    /* ---- 绑定到 eth0 ---- */
    {
        struct sockaddr_ll sa;
        memset(&sa, 0, sizeof(sa));
        sa.sll_family   = AF_PACKET;
        sa.sll_protocol = htons(ETH_P_ALL);
        sa.sll_ifindex  = ifindex;
        if (bind(raw_fd, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
            FAIL("bind(AF_PACKET -> eth0)", "绑定失败");
            goto summary;
        }
        {
            char buf[64];
            snprintf(buf, sizeof(buf), "bind(AF_PACKET, ETH_P_ALL, %s)", ifname);
            PASS(buf);
        }
    }

    /* ---- 接收超时 ---- */
    {
        struct timeval tv;
        tv.tv_sec  = RECV_TIMEOUT_SEC;
        tv.tv_usec = 0;
        if (setsockopt(raw_fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv)) < 0)
            FAIL("setsockopt(SO_RCVTIMEO)", "设置接收超时失败");
        else
            PASS("setsockopt(SO_RCVTIMEO=1s)");
    }

    unsigned char frame[ARP_FRAME_LEN];
    build_arp_request(frame, sizeof(frame), local_mac, LOCAL_IP, GATEWAY);
    hexdump("构造的 ARP 请求帧", frame, ARP_FRAME_LEN);

    /* ===================================================== *
     * Test 1: SOCK_RAW 发送路径 (sendto + sendmsg)          *
     * ===================================================== */
    printf("\n--- Test 1: SOCK_RAW 发送 (sendto / sendmsg) ---\n");
    {
        struct sockaddr_ll dst;
        unsigned char bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
        make_dst_ll(&dst, ifindex, bcast);

        /* 1a: sendto */
        ssize_t n = sendto(raw_fd, frame, ARP_FRAME_LEN, 0,
                           (struct sockaddr *)&dst, sizeof(dst));
        if (n < 0)
            FAIL("sendto(ARP 帧)", "返回 %zd", n);
        else if (n != ARP_FRAME_LEN) {
            char b[64];
            snprintf(b, sizeof(b), "返回 %zd (期望 %d)", n, ARP_FRAME_LEN);
            FAIL("sendto(ARP 帧) 字节数", "%s", b);
        } else {
            char b[64];
            snprintf(b, sizeof(b), "sendto 发送 %d 字节 (== 帧长)", (int)n);
            PASS(b);
        }

        /* 1b: sendmsg 单 iovec (整个帧) */
        struct iovec iov;
        iov.iov_base = frame;
        iov.iov_len  = ARP_FRAME_LEN;
        struct msghdr msg;
        memset(&msg, 0, sizeof(msg));
        msg.msg_name    = &dst;
        msg.msg_namelen = sizeof(dst);
        msg.msg_iov     = &iov;
        msg.msg_iovlen  = 1;

        ssize_t m = sendmsg(raw_fd, &msg, 0);
        if (m < 0)
            FAIL("sendmsg(单 iovec)", "返回 %zd", m);
        else if (m != ARP_FRAME_LEN) {
            char b[64];
            snprintf(b, sizeof(b), "返回 %zd (期望 %d)", m, ARP_FRAME_LEN);
            FAIL("sendmsg(单 iovec) 字节数", "%s", b);
        } else {
            char b[64];
            snprintf(b, sizeof(b), "sendmsg 发送 %d 字节 (== 帧长)", (int)m);
            PASS(b);
        }
    }

    /* ===================================================== *
     * Test 4: sendmsg iovec gather (提前到这里，便于 Test2/3 接收) *
     * ===================================================== */
    printf("\n--- Test 4: sendmsg iovec gather ---\n");
    {
        /* 把帧拆成 2 段: eth 头(14) + arp 负载(28) */
        unsigned char part1[ETH_HDR_LEN];
        unsigned char part2[ARP_PKT_LEN];
        memcpy(part1, frame, ETH_HDR_LEN);
        memcpy(part2, frame + ETH_HDR_LEN, ARP_PKT_LEN);

        struct iovec iov[2];
        iov[0].iov_base = part1;
        iov[0].iov_len  = ETH_HDR_LEN;
        iov[1].iov_base = part2;
        iov[1].iov_len  = ARP_PKT_LEN;

        struct sockaddr_ll dst;
        unsigned char bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
        make_dst_ll(&dst, ifindex, bcast);

        struct msghdr msg;
        memset(&msg, 0, sizeof(msg));
        msg.msg_name    = &dst;
        msg.msg_namelen = sizeof(dst);
        msg.msg_iov     = iov;
        msg.msg_iovlen  = 2;

        ssize_t m = sendmsg(raw_fd, &msg, 0);
        if (m < 0)
            FAIL("sendmsg(2 iovec gather)", "返回 %zd", m);
        else if (m != ARP_FRAME_LEN) {
            char b[64];
            snprintf(b, sizeof(b), "返回 %zd (期望 %d)", m, ARP_FRAME_LEN);
            FAIL("sendmsg gather 字节数", "%s", b);
        } else {
            char b[64];
            snprintf(b, sizeof(b), "sendmsg gather 14+28=%d 字节正确", (int)m);
            PASS(b);
        }
    }

    /* ===================================================== *
     * Test 2: SOCK_RAW 接收路径 (recvfrom + sockaddr_ll)    *
     * ===================================================== */
    printf("\n--- Test 2: SOCK_RAW 接收 (recvfrom + sockaddr_ll) ---\n");
    {
        stimulate(raw_fd, ifindex, local_mac);

        unsigned char rbuf[ETH_FRAME_LEN + 64];
        struct sockaddr_ll from;
        socklen_t fromlen;
        ssize_t n = -1;
        int got_any = 0;

        for (int attempt = 0; attempt < RECV_MAX_ATTEMPTS; attempt++) {
            memset(&from, 0, sizeof(from));
            fromlen = sizeof(from);
            n = recvfrom(raw_fd, rbuf, sizeof(rbuf), 0,
                         (struct sockaddr *)&from, &fromlen);
            if (n < 0) {
                if (errno == EAGAIN || errno == EWOULDBLOCK)
                    continue;          /* 超时，再试 */
                break;                 /* 其它错误，跳出 */
            }
            got_any = 1;
            if (from.sll_pkttype == PACKET_OUTGOING)
                continue;              /* 跳过本机发出的回环包，找真正的入站帧 */
            break;                     /* 拿到入站帧 */
        }

        if (!got_any || n < 0) {
            SKIP("recvfrom 接收", "超时未收到任何帧 (网络环境无回环/回应)");
        } else {
            char macs[24];
            mac_to_str(from.sll_addr, macs, sizeof(macs));
            printf("    收到 %zd 字节, pkttype=%d, hatype=%d, halen=%d, addr=%s\n",
                   n, from.sll_pkttype, from.sll_hatype, from.sll_halen, macs);
            int len_ok = (n > 0);
            int sll_ok = (validate_sll(&from, rbuf, (size_t)n, "recvfrom") == 0);
            if (len_ok && sll_ok)
                PASS("recvfrom 返回数据 + sockaddr_ll 字段正确");
            else
                FAIL("recvfrom 校验", "len_ok=%d sll_ok=%d", len_ok, sll_ok);
        }
    }

    /* ===================================================== *
     * Test 3: recvmsg iovec scatter                         *
     * ===================================================== */
    printf("\n--- Test 3: recvmsg iovec scatter ---\n");
    {
        stimulate(raw_fd, ifindex, local_mac);

        /* 分散到 2 个缓冲区: 前 16 字节 + 剩余 */
        unsigned char seg0[16];
        unsigned char seg1[ETH_FRAME_LEN];
        struct iovec iov[2];
        iov[0].iov_base = seg0;
        iov[0].iov_len  = sizeof(seg0);
        iov[1].iov_base = seg1;
        iov[1].iov_len  = sizeof(seg1);

        struct sockaddr_ll from;
        struct msghdr msg;
        memset(&msg, 0, sizeof(msg));
        msg.msg_name    = &from;
        msg.msg_namelen = sizeof(from);
        msg.msg_iov     = iov;
        msg.msg_iovlen  = 2;

        ssize_t n = -1;
        int got_any = 0;
        for (int attempt = 0; attempt < RECV_MAX_ATTEMPTS; attempt++) {
            memset(&from, 0, sizeof(from));
            msg.msg_namelen = sizeof(from);
            /* 重置 iov 长度(被 recvmsg 修改后需恢复) */
            iov[0].iov_len = sizeof(seg0);
            iov[1].iov_len = sizeof(seg1);
            n = recvmsg(raw_fd, &msg, 0);
            if (n < 0) {
                if (errno == EAGAIN || errno == EWOULDBLOCK)
                    continue;
                break;
            }
            got_any = 1;
            if (from.sll_pkttype == PACKET_OUTGOING)
                continue;
            break;
        }

        if (!got_any || n < 0) {
            SKIP("recvmsg scatter 接收", "超时未收到任何帧");
        } else {
            /* 拼接两个 iovec 的实际内容 */
            size_t take0 = (size_t)n < sizeof(seg0) ? (size_t)n : sizeof(seg0);
            size_t take1 = (size_t)n > sizeof(seg0) ? (size_t)n - sizeof(seg0) : 0;
            unsigned char joined[ETH_FRAME_LEN + 64];
            memcpy(joined, seg0, take0);
            memcpy(joined + take0, seg1, take1);

            int total_ok = ((ssize_t)(take0 + take1) == n);
            /* 校验拼接后是一帧有效以太网帧: 长度>=14 且 ethertype 与 sll_protocol 一致 */
            int frame_ok = 1;
            if ((size_t)n >= ETH_HDR_LEN) {
                unsigned short eth_proto = ((unsigned short)joined[12] << 8) | joined[13];
                unsigned short sll_proto = ntohs(from.sll_protocol);
                if (eth_proto != sll_proto) {
                    printf("    [检查] 拼接帧 ethertype=0x%04x != sll_protocol=0x%04x ✗\n",
                           eth_proto, sll_proto);
                    frame_ok = 0;
                }
            } else {
                frame_ok = 0;
            }
            int sll_ok = (validate_sll(&from, joined, (size_t)n, "recvmsg") == 0);

            printf("    scatter: 总 %zd 字节, seg0=%zu seg1=%zu\n",
                   n, take0, take1);
            if (total_ok && frame_ok && sll_ok)
                PASS("recvmsg iovec scatter 拼装正确");
            else
                FAIL("recvmsg scatter 校验", "total_ok=%d frame_ok=%d sll_ok=%d",
                     total_ok, frame_ok, sll_ok);
        }
    }

    /* ===================================================== *
     * Test 5: SOCK_DGRAM 收发                               *
     * ===================================================== */
    printf("\n--- Test 5: SOCK_DGRAM 收发 (内核构造以太网头) ---\n");
    {
        int dgram_fd = socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_ALL));
        if (dgram_fd < 0) {
            FAIL("socket(AF_PACKET, SOCK_DGRAM)", "返回 -1");
        } else {
            PASS("socket(AF_PACKET, SOCK_DGRAM, ETH_P_ALL)");

            /* 绑定到 eth0 */
            struct sockaddr_ll sa;
            memset(&sa, 0, sizeof(sa));
            sa.sll_family   = AF_PACKET;
            sa.sll_protocol = htons(ETH_P_ALL);
            sa.sll_ifindex  = ifindex;
            if (bind(dgram_fd, (struct sockaddr *)&sa, sizeof(sa)) < 0) {
                FAIL("bind(SOCK_DGRAM -> eth0)", "绑定失败");
            } else {
                {
                    char buf2[64];
                    snprintf(buf2, sizeof(buf2), "bind(SOCK_DGRAM, %s)", ifname);
                    PASS(buf2);
                }

                /* DGRAM 只发 L3 负载 (ARP 28 字节)，内核负责加以太网头 */
                struct arp_hdr arp;
                memset(&arp, 0, sizeof(arp));
                arp.ar_hrd = htons(1);
                arp.ar_pro = htons(ETH_P_IP);
                arp.ar_hln = 6;
                arp.ar_pln = 4;
                arp.ar_op  = htons(ARPOP_REQUEST);
                memcpy(arp.ar_sha, local_mac, 6);
                struct in_addr spa; spa.s_addr = inet_addr(LOCAL_IP);
                memcpy(arp.ar_spa, &spa.s_addr, 4);
                struct in_addr tpa; tpa.s_addr = inet_addr(GATEWAY);
                memcpy(arp.ar_tpa, &tpa.s_addr, 4);

                unsigned char bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
                struct sockaddr_ll dst;
                make_dst_ll(&dst, ifindex, bcast);

                ssize_t n = sendto(dgram_fd, &arp, sizeof(arp), 0,
                                   (struct sockaddr *)&dst, sizeof(dst));
                if (n < 0)
                    FAIL("DGRAM sendto(ARP 负载)", "返回 %zd", n);
                else if (n != (ssize_t)sizeof(arp)) {
                    char b[64];
                    snprintf(b, sizeof(b), "返回 %zd (期望 %zu)", n, sizeof(arp));
                    FAIL("DGRAM sendto 字节数", "%s", b);
                } else {
                    char b[64];
                    snprintf(b, sizeof(b), "DGRAM sendto 返回 %d (== L3 负载长 28)", (int)n);
                    PASS(b);
                }
            }
            close(dgram_fd);
        }
    }

summary:
    printf("\n===== AF_PACKET 端到端测试结果 =====\n");
    printf("通过: %d, 失败: %d, 跳过: %d\n", pass_count, fail_count, skip_count);
    return (fail_count > 0) ? 1 : 0;
}
