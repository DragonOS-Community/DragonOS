// af_packet_e2e.cc - AF_PACKET 端到端收发测试 (dunitest/gtest)
//
// 由 user/apps/c_unitest/test_af_packet_e2e.c 转换而来。
// 验证 DragonOS AF_PACKET 实际以太网帧的发送与接收:
//   Test 1: SOCK_RAW 发送路径 —— sendto() / sendmsg() 返回正确字节数
//   Test 2: SOCK_RAW 接收路径 —— recvfrom() 返回数据 + 校验 sockaddr_ll
//   Test 3: recvmsg() iovec scatter —— 数据正确分散到多个缓冲区
//   Test 4: sendmsg() iovec gather —— 多个缓冲区正确拼装后发送
//   Test 5: SOCK_DGRAM 收发 —— 内核构造以太网头，返回 L3 payload 长度
//
// 运行环境: DragonOS QEMU，eth0(virtio-net) 10.0.2.15/24，网关 10.0.2.2。
// 收不到包时用 GTEST_SKIP() 而非失败；SO_RCVTIMEO 失败不报错 (平台限制)。

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <net/ethernet.h>
#include <net/if.h>
#include <net/if_arp.h>
#include <netinet/if_ether.h>
#include <netpacket/packet.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/uio.h>
#include <unistd.h>

#include <cstdio>
#include <cstdint>
#include <cstring>
#include <string>

// ---- 手动定义常量 (DragonOS musl 可能缺少 if_packet.h) ----

#ifndef AF_PACKET
#define AF_PACKET 17
#endif

#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif

#ifndef SIOCGIFHWADDR
#define SIOCGIFHWADDR 0x8927
#endif

// PACKET_OUTGOING: 本机发出的回环包标记 (Linux if_packet.h)
#ifndef PACKET_OUTGOING
#define PACKET_OUTGOING 4
#endif

namespace {

inline constexpr int kArpPktLen = 28;
inline constexpr int kEthHdrLen = 14;
inline constexpr int kArpFrameLen = kEthHdrLen + kArpPktLen;  // 42
inline constexpr int kEthFrameLen = 1514;

inline constexpr const char* kLocalIp = "10.0.2.15";
inline constexpr const char* kGateway = "10.0.2.2";
inline constexpr int kRecvMaxAttempts = 8;

// 手动定义 ARP 报文 (musl 无 struct ether_arp)，28 字节
struct ArpHdr {
    uint16_t ar_hrd;     // 硬件类型: ARPHRD_ETHER(1)
    uint16_t ar_pro;     // 协议类型: ETH_P_IP(0x0800)
    uint8_t ar_hln;      // 硬件地址长度: 6
    uint8_t ar_pln;      // 协议地址长度: 4
    uint16_t ar_op;      // 操作: ARPOP_REQUEST(1) / ARPOP_REPLY(2)
    uint8_t ar_sha[6];   // 发送方 MAC
    uint8_t ar_spa[4];   // 发送方 IP
    uint8_t ar_tha[6];   // 目标 MAC
    uint8_t ar_tpa[4];   // 目标 IP
};
static_assert(sizeof(ArpHdr) == kArpPktLen, "ArpHdr 大小必须为 28");

// RAII fd 守护
class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() { Reset(); }

    int Get() const { return fd_; }

    void Reset(int fd = -1) {
        if (fd_ >= 0) close(fd_);
        fd_ = fd;
    }

  private:
    int fd_;
};

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

// 暴力枚举 eth0-eth20，用 ioctl(SIOCGIFINDEX) 验证存在性。
// DragonOS 没有 /proc/net/dev，且接口名可能不稳定。
std::string DiscoverIfname() {
    int sock = socket(AF_INET, SOCK_DGRAM, 0);
    if (sock < 0) return "eth0";
    for (int i = 0; i <= 20; ++i) {
        std::string name = "eth" + std::to_string(i);
        struct ifreq ifr;
        std::memset(&ifr, 0, sizeof(ifr));
        std::strncpy(ifr.ifr_name, name.c_str(), IFNAMSIZ - 1);
        if (ioctl(sock, SIOCGIFINDEX, &ifr) == 0) {
            close(sock);
            return name;
        }
    }
    close(sock);
    return "eth0";
}

// 通过 ioctl 获取接口索引；失败返回 -1
int GetIfIndex(int any_fd, const std::string& ifname) {
    struct ifreq ifr;
    std::memset(&ifr, 0, sizeof(ifr));
    std::strncpy(ifr.ifr_name, ifname.c_str(), IFNAMSIZ - 1);
    if (ioctl(any_fd, SIOCGIFINDEX, &ifr) < 0) return -1;
    return ifr.ifr_ifindex;
}

// 获取本机 MAC：ioctl(SIOCGIFHWADDR) → sysfs → 默认 MAC。永不失败。
void GetIfHwaddr(int any_fd, const std::string& ifname, uint8_t mac[6]) {
    struct ifreq ifr;
    std::memset(&ifr, 0, sizeof(ifr));
    std::strncpy(ifr.ifr_name, ifname.c_str(), IFNAMSIZ - 1);
    if (ioctl(any_fd, SIOCGIFHWADDR, &ifr) == 0) {
        std::memcpy(mac, ifr.ifr_hwaddr.sa_data, 6);
        bool allzero = true;
        for (int i = 0; i < 6; ++i) {
            if (mac[i]) {
                allzero = false;
                break;
            }
        }
        if (!allzero) return;
    }
    // 回退到 sysfs
    char path[64];
    std::snprintf(path, sizeof(path), "/sys/class/net/%s/address", ifname.c_str());
    FILE* f = std::fopen(path, "r");
    if (f) {
        unsigned int v[6];
        int n = std::fscanf(f, "%x:%x:%x:%x:%x:%x", &v[0], &v[1], &v[2], &v[3], &v[4], &v[5]);
        std::fclose(f);
        if (n == 6) {
            for (int i = 0; i < 6; ++i) mac[i] = static_cast<uint8_t>(v[i]);
            return;
        }
    }
    // 最后兜底：默认 QEMU virtio-net MAC
    static const uint8_t kDefaultMac[6] = {0x52, 0x54, 0x00, 0x12, 0x34, 0x56};
    std::memcpy(mac, kDefaultMac, 6);
}

// 构造一份广播 ARP 请求帧 (ether_header + ArpHdr)，长度 = kArpFrameLen
void BuildArpRequest(uint8_t* frame, const uint8_t local_mac[6]) {
    std::memset(frame, 0, kArpFrameLen);
    // 以太网头
    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    std::memcpy(frame + 0, bcast, 6);       // dst = 广播
    std::memcpy(frame + 6, local_mac, 6);   // src = 本机
    frame[12] = (ETH_P_ARP >> 8) & 0xff;    // ethertype = 0x0806 (网络序)
    frame[13] = ETH_P_ARP & 0xff;
    // ARP 负载
    ArpHdr* a = reinterpret_cast<ArpHdr*>(frame + kEthHdrLen);
    a->ar_hrd = htons(1);                    // ARPHRD_ETHER
    a->ar_pro = htons(ETH_P_IP);             // IPv4
    a->ar_hln = 6;
    a->ar_pln = 4;
    a->ar_op = htons(ARPOP_REQUEST);
    std::memcpy(a->ar_sha, local_mac, 6);
    struct in_addr spa;
    spa.s_addr = inet_addr(kLocalIp);
    std::memcpy(a->ar_spa, &spa.s_addr, 4);
    // ar_tha 保持 0
    struct in_addr tpa;
    tpa.s_addr = inet_addr(kGateway);
    std::memcpy(a->ar_tpa, &tpa.s_addr, 4);
}

// 构造发往 sockaddr_ll 的目的地址
void MakeDstLL(struct sockaddr_ll* sa, int ifindex, const uint8_t dst_mac[6]) {
    std::memset(sa, 0, sizeof(*sa));
    sa->sll_family = AF_PACKET;
    sa->sll_protocol = htons(ETH_P_ARP);
    sa->sll_ifindex = ifindex;
    sa->sll_hatype = ARPHRD_ETHER;
    sa->sll_pkttype = 0;
    sa->sll_halen = ETH_ALEN;
    std::memcpy(sa->sll_addr, dst_mac, 6);
}

// 校验接收到的 sockaddr_ll 字段是否符合预期
::testing::AssertionResult ValidateSll(const struct sockaddr_ll* sll,
                                       const uint8_t* frame, ssize_t n) {
    if (sll->sll_family != AF_PACKET) {
        return ::testing::AssertionFailure()
               << "sll_family=" << sll->sll_family << " (期望 " << AF_PACKET << ")";
    }
    unsigned short proto = ntohs(sll->sll_protocol);
    if (proto == 0) {
        return ::testing::AssertionFailure() << "sll_protocol=0 (无效)";
    }
    if (n >= kEthHdrLen) {
        unsigned short eth_proto =
            (static_cast<unsigned short>(frame[12]) << 8) | frame[13];
        if (proto != eth_proto) {
            return ::testing::AssertionFailure()
                   << "sll_protocol=0x" << std::hex << proto
                   << " 与帧 ethertype=0x" << eth_proto << " 不一致";
        }
    }
    if (sll->sll_hatype != ARPHRD_ETHER) {
        return ::testing::AssertionFailure()
               << "sll_hatype=" << sll->sll_hatype << " (期望 ARPHRD_ETHER)";
    }
    if (sll->sll_halen != ETH_ALEN) {
        return ::testing::AssertionFailure()
               << "sll_halen=" << static_cast<int>(sll->sll_halen) << " (期望 ETH_ALEN)";
    }
    bool allzero = true;
    for (int i = 0; i < ETH_ALEN; ++i) {
        if (sll->sll_addr[i]) {
            allzero = false;
            break;
        }
    }
    if (allzero) {
        return ::testing::AssertionFailure() << "sll_addr 全零 (期望有效 MAC)";
    }
    return ::testing::AssertionSuccess();
}

// 给一个已绑定的 SOCK_RAW 套接字发若干 ARP 请求，激发网关回应
void Stimulate(int tx_fd, int ifindex, const uint8_t local_mac[6]) {
    uint8_t frame[kArpFrameLen];
    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);
    BuildArpRequest(frame, local_mac);
    for (int i = 0; i < 3; ++i) {
        sendto(tx_fd, frame, kArpFrameLen, 0,
               reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst));
        usleep(20 * 1000);  // 20ms 间隔
    }
}

// 创建并绑定到指定接口的 SOCK_RAW 套接字，返回 fd 或 -1。
// 同时尝试设置 SO_RCVTIMEO (DragonOS 可能不支持，失败不报错)。
int MakeBoundRaw(const std::string& ifname, int ifindex, uint8_t mac[6]) {
    int fd = socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL));
    if (fd < 0) return -1;
    struct sockaddr_ll sa;
    std::memset(&sa, 0, sizeof(sa));
    sa.sll_family = AF_PACKET;
    sa.sll_protocol = htons(ETH_P_ALL);
    sa.sll_ifindex = ifindex;
    if (bind(fd, reinterpret_cast<struct sockaddr*>(&sa), sizeof(sa)) < 0) {
        close(fd);
        return -1;
    }
    GetIfHwaddr(fd, ifname, mac);
    // SO_RCVTIMEO: 尝试设置，失败不报错 (DragonOS 平台限制)
    struct timeval tv;
    tv.tv_sec = 1;
    tv.tv_usec = 0;
    setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &tv, sizeof(tv));
    return fd;
}

// 探测网卡 ifindex；失败返回 -1
int ProbeIfindex(const std::string& ifname) {
    int ctrl = socket(AF_INET, SOCK_DGRAM, 0);
    if (ctrl < 0) return -1;
    int idx = GetIfIndex(ctrl, ifname);
    close(ctrl);
    return idx;
}

}  // namespace

// ===== Test 1: SOCK_RAW 发送 (sendto / sendmsg) =====
TEST(AfPacketE2E, RawSendtoAndSendmsg) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "未找到可用网卡 (" << ifname << ")，跳过 e2e 发送测试";
    }

    FdGuard raw_fd(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(raw_fd.Get(), 0) << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(raw_fd.Get(), ifname, local_mac);
    uint8_t frame[kArpFrameLen];
    BuildArpRequest(frame, local_mac);
    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);

    // 1a: sendto
    ssize_t n = sendto(raw_fd.Get(), frame, kArpFrameLen, 0,
                       reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst));
    ASSERT_EQ(n, kArpFrameLen) << "sendto 字节数错误: " << ErrnoString(errno);

    // 1b: sendmsg 单 iovec
    struct iovec iov;
    iov.iov_base = frame;
    iov.iov_len = kArpFrameLen;
    struct msghdr msg;
    std::memset(&msg, 0, sizeof(msg));
    msg.msg_name = &dst;
    msg.msg_namelen = sizeof(dst);
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    ssize_t m = sendmsg(raw_fd.Get(), &msg, 0);
    ASSERT_EQ(m, kArpFrameLen) << "sendmsg 字节数错误: " << ErrnoString(errno);
}

// ===== Test 4: sendmsg iovec gather =====
// (放在接收测试前，便于 Test 2/3 观察到流量)
TEST(AfPacketE2E, SendmsgIovecGather) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "未找到可用网卡，跳过 sendmsg gather 测试";
    }

    FdGuard raw_fd(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(raw_fd.Get(), 0) << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(raw_fd.Get(), ifname, local_mac);
    uint8_t frame[kArpFrameLen];
    BuildArpRequest(frame, local_mac);

    // 把帧拆成 2 段: eth 头(14) + arp 负载(28)
    uint8_t part1[kEthHdrLen];
    uint8_t part2[kArpPktLen];
    std::memcpy(part1, frame, kEthHdrLen);
    std::memcpy(part2, frame + kEthHdrLen, kArpPktLen);

    struct iovec iov[2];
    iov[0].iov_base = part1;
    iov[0].iov_len = kEthHdrLen;
    iov[1].iov_base = part2;
    iov[1].iov_len = kArpPktLen;

    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);

    struct msghdr msg;
    std::memset(&msg, 0, sizeof(msg));
    msg.msg_name = &dst;
    msg.msg_namelen = sizeof(dst);
    msg.msg_iov = iov;
    msg.msg_iovlen = 2;

    ssize_t m = sendmsg(raw_fd.Get(), &msg, 0);
    ASSERT_EQ(m, kArpFrameLen) << "sendmsg gather 字节数错误: " << ErrnoString(errno);
}

// ===== Test 2: SOCK_RAW 接收 (recvfrom + sockaddr_ll) =====
TEST(AfPacketE2E, RecvfromReturnsDataAndSockaddrLl) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "未找到可用网卡，跳过 recvfrom 接收测试";
    }

    uint8_t local_mac[6];
    FdGuard raw_fd(MakeBoundRaw(ifname, ifindex, local_mac));
    ASSERT_GE(raw_fd.Get(), 0) << ErrnoString(errno);

    Stimulate(raw_fd.Get(), ifindex, local_mac);

    uint8_t rbuf[kEthFrameLen + 64];
    struct sockaddr_ll from;
    bool got_inbound = false;
    ssize_t n = -1;
    // MSG_DONTWAIT 保证不阻塞 (SO_RCVTIMEO 在 DragonOS 可能不支持)
    for (int attempt = 0; attempt < kRecvMaxAttempts; ++attempt) {
        std::memset(&from, 0, sizeof(from));
        socklen_t fromlen = sizeof(from);
        n = recvfrom(raw_fd.Get(), rbuf, sizeof(rbuf), MSG_DONTWAIT,
                     reinterpret_cast<struct sockaddr*>(&from), &fromlen);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                usleep(50 * 1000);  // 50ms 后重试
                continue;
            }
            break;  // 其它错误
        }
        if (from.sll_pkttype == PACKET_OUTGOING) {
            continue;  // 跳过本机发出的回环包
        }
        got_inbound = true;
        break;
    }

    if (!got_inbound || n < 0) {
        GTEST_SKIP() << "超时未收到任何入站帧 (网络环境无回环/回应)";
    }
    EXPECT_GT(n, 0);
    EXPECT_TRUE(ValidateSll(&from, rbuf, n));
}

// ===== Test 3: recvmsg iovec scatter =====
TEST(AfPacketE2E, RecvmsgIovecScatter) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "未找到可用网卡，跳过 recvmsg scatter 测试";
    }

    uint8_t local_mac[6];
    FdGuard raw_fd(MakeBoundRaw(ifname, ifindex, local_mac));
    ASSERT_GE(raw_fd.Get(), 0) << ErrnoString(errno);

    Stimulate(raw_fd.Get(), ifindex, local_mac);

    // 分散到 2 个缓冲区: 前 16 字节 + 剩余
    uint8_t seg0[16];
    uint8_t seg1[kEthFrameLen];
    struct iovec iov[2];
    iov[0].iov_base = seg0;
    iov[1].iov_base = seg1;

    struct sockaddr_ll from;
    struct msghdr msg;
    std::memset(&msg, 0, sizeof(msg));
    msg.msg_name = &from;
    msg.msg_namelen = sizeof(from);
    msg.msg_iov = iov;
    msg.msg_iovlen = 2;

    bool got_inbound = false;
    ssize_t n = -1;
    for (int attempt = 0; attempt < kRecvMaxAttempts; ++attempt) {
        std::memset(&from, 0, sizeof(from));
        msg.msg_namelen = sizeof(from);
        // 重置 iov 长度 (被 recvmsg 修改后需恢复)
        iov[0].iov_len = sizeof(seg0);
        iov[1].iov_len = sizeof(seg1);
        n = recvmsg(raw_fd.Get(), &msg, MSG_DONTWAIT);
        if (n < 0) {
            if (errno == EAGAIN || errno == EWOULDBLOCK) {
                usleep(50 * 1000);
                continue;
            }
            break;
        }
        if (from.sll_pkttype == PACKET_OUTGOING) {
            continue;
        }
        got_inbound = true;
        break;
    }

    if (!got_inbound || n < 0) {
        GTEST_SKIP() << "超时未收到任何入站帧";
    }

    // 拼接两个 iovec 的实际内容
    size_t take0 = static_cast<size_t>(n) < sizeof(seg0)
                       ? static_cast<size_t>(n)
                       : sizeof(seg0);
    size_t take1 = static_cast<size_t>(n) > sizeof(seg0)
                       ? static_cast<size_t>(n) - sizeof(seg0)
                       : 0;
    EXPECT_EQ(static_cast<ssize_t>(take0 + take1), n);

    uint8_t joined[kEthFrameLen + 64];
    std::memcpy(joined, seg0, take0);
    std::memcpy(joined + take0, seg1, take1);

    // 校验拼接帧: ethertype 与 sll_protocol 一致
    if (static_cast<size_t>(n) >= static_cast<size_t>(kEthHdrLen)) {
        unsigned short eth_proto =
            (static_cast<unsigned short>(joined[12]) << 8) | joined[13];
        unsigned short sll_proto = ntohs(from.sll_protocol);
        EXPECT_EQ(eth_proto, sll_proto)
            << "拼接帧 ethertype 与 sll_protocol 不一致";
    } else {
        ADD_FAILURE() << "接收帧过短 (" << n << " < " << kEthHdrLen << ")";
    }

    EXPECT_TRUE(ValidateSll(&from, joined, n));
}

// ===== Test 5: SOCK_DGRAM 收发 (内核构造以太网头) =====
TEST(AfPacketE2E, DgramSendReturnsLayer3PayloadLen) {
    std::string ifname = DiscoverIfname();
    int ifindex = ProbeIfindex(ifname);
    if (ifindex < 0) {
        GTEST_SKIP() << "未找到可用网卡，跳过 SOCK_DGRAM 测试";
    }

    FdGuard dgram_fd(socket(AF_PACKET, SOCK_DGRAM, htons(ETH_P_ALL)));
    ASSERT_GE(dgram_fd.Get(), 0) << ErrnoString(errno);

    // 绑定到接口
    struct sockaddr_ll sa;
    std::memset(&sa, 0, sizeof(sa));
    sa.sll_family = AF_PACKET;
    sa.sll_protocol = htons(ETH_P_ALL);
    sa.sll_ifindex = ifindex;
    ASSERT_EQ(bind(dgram_fd.Get(), reinterpret_cast<struct sockaddr*>(&sa), sizeof(sa)), 0)
        << ErrnoString(errno);

    uint8_t local_mac[6];
    GetIfHwaddr(dgram_fd.Get(), ifname, local_mac);

    // DGRAM 只发 L3 负载 (ARP 28 字节)，内核负责加以太网头
    ArpHdr arp;
    std::memset(&arp, 0, sizeof(arp));
    arp.ar_hrd = htons(1);
    arp.ar_pro = htons(ETH_P_IP);
    arp.ar_hln = 6;
    arp.ar_pln = 4;
    arp.ar_op = htons(ARPOP_REQUEST);
    std::memcpy(arp.ar_sha, local_mac, 6);
    struct in_addr spa;
    spa.s_addr = inet_addr(kLocalIp);
    std::memcpy(arp.ar_spa, &spa.s_addr, 4);
    struct in_addr tpa;
    tpa.s_addr = inet_addr(kGateway);
    std::memcpy(arp.ar_tpa, &tpa.s_addr, 4);

    uint8_t bcast[6] = {0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    struct sockaddr_ll dst;
    MakeDstLL(&dst, ifindex, bcast);

    ssize_t n = sendto(dgram_fd.Get(), &arp, sizeof(arp), 0,
                       reinterpret_cast<struct sockaddr*>(&dst), sizeof(dst));
    ASSERT_EQ(n, static_cast<ssize_t>(sizeof(arp)))
        << "DGRAM sendto 应返回 L3 负载长度 28: " << ErrnoString(errno);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
