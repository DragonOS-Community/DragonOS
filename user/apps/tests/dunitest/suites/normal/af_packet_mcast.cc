// af_packet_mcast.cc - AF_PACKET multicast membership 测试 (dunitest/gtest)
//
// 由 user/apps/c_unitest/test_af_packet_mcast.c 转换而来。
// 验证 DragonOS AF_PACKET 的 PACKET_ADD_MEMBERSHIP / PACKET_DROP_MEMBERSHIP
// 行为，覆盖 PROMISC / ALLMULTI / MULTICAST 三种 mr_type 以及错误码语义。
//
// 需要网络接口: 用 discover_ifname() 暴力枚举 eth0-eth20。

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <net/if.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstring>
#include <string>

// ---- 手动定义常量 (DragonOS musl 可能缺少 if_packet.h) ----

#ifndef AF_PACKET
#define AF_PACKET 17
#endif

#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif

#ifndef SIOCGIFINDEX
#define SIOCGIFINDEX 0x8933
#endif

#ifndef IFNAMSIZ
#define IFNAMSIZ 16
#endif

inline constexpr int kEthPAll = 0x0003;

#ifndef PACKET_ADD_MEMBERSHIP
#define PACKET_ADD_MEMBERSHIP 1
#endif
#ifndef PACKET_DROP_MEMBERSHIP
#define PACKET_DROP_MEMBERSHIP 2
#endif

// packet_mreq 的 mr_type 常量 (对应 Linux if_packet.h)
#ifndef PACKET_MR_PROMISC
#define PACKET_MR_PROMISC 0
#endif
#ifndef PACKET_MR_MULTICAST
#define PACKET_MR_MULTICAST 1
#endif
#ifndef PACKET_MR_ALLMULTI
#define PACKET_MR_ALLMULTI 2
#endif
#ifndef PACKET_MR_UNICAST
#define PACKET_MR_UNICAST 3
#endif

namespace {

// 手动定义 struct packet_mreq (对应 Linux struct packet_mreq)。
// 布局: mr_ifindex(u32) + mr_type(u32) + mr_alen(u16) + mr_address[8]。
struct PacketMreq {
    unsigned int mr_ifindex;
    unsigned int mr_type;
    unsigned short mr_alen;
    unsigned char mr_address[8];
};

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

// 获取网卡 ifindex: 用临时 AF_INET/DGRAM 控制套接字发送 ioctl。
// 成功返回 ifindex (>=1)，失败返回 -1。
int GetIfIndex(const std::string& ifname) {
    struct ifreq ifr;
    std::memset(&ifr, 0, sizeof(ifr));
    std::strncpy(ifr.ifr_name, ifname.c_str(), IFNAMSIZ - 1);
    ifr.ifr_name[IFNAMSIZ - 1] = '\0';

    int ctrl = socket(AF_INET, SOCK_DGRAM, 0);
    if (ctrl < 0) return -1;
    int rc = ioctl(ctrl, SIOCGIFINDEX, &ifr);
    close(ctrl);
    if (rc < 0) return -1;
    return ifr.ifr_ifindex;
}

// 探测网卡并创建 SOCK_RAW 套接字。
// 若无网卡或无权限则 GTEST_SKIP；返回的 FdGuard 持有有效 fd。
// 返回 ifindex，通过 out_fd 输出套接字 fd。
int SetupMcastEnv(FdGuard* out_fd) {
    std::string ifname = DiscoverIfname();
    int ifindex = GetIfIndex(ifname);
    if (ifindex < 0) {
        return -1;
    }
    out_fd->Reset(socket(AF_PACKET, SOCK_RAW, htons(kEthPAll)));
    if (out_fd->Get() < 0) {
        return -2;
    }
    return ifindex;
}

}  // namespace

// ===== Test 1: ADD_MEMBERSHIP (PACKET_MR_PROMISC) =====
TEST(AfPacketMcast, AddMembershipPromisc) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "未找到可用网卡，跳过 multicast 测试";
    }
    ASSERT_NE(ifindex, -2) << "创建 AF_PACKET socket 失败: " << ErrnoString(errno)
                           << " (需要 CAP_NET_RAW)";

    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = PACKET_MR_PROMISC;
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);
}

// ===== Test 2: DROP_MEMBERSHIP (PACKET_MR_PROMISC) =====
TEST(AfPacketMcast, DropMembershipPromisc) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "未找到可用网卡，跳过 multicast 测试";
    }
    ASSERT_NE(ifindex, -2) << "创建 AF_PACKET socket 失败: " << ErrnoString(errno);

    // 先 ADD 再 DROP (复用同一 mreq)
    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = PACKET_MR_PROMISC;
    ASSERT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno) << " (应恢复接口标志)";
}

// ===== Test 3: ADD/DROP MEMBERSHIP (PACKET_MR_ALLMULTI) =====
TEST(AfPacketMcast, AddDropMembershipAllmulti) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "未找到可用网卡，跳过 multicast 测试";
    }
    ASSERT_NE(ifindex, -2) << "创建 AF_PACKET socket 失败: " << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = PACKET_MR_ALLMULTI;

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);

    // DROP 清除
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno) << " (应恢复)";
}

// ===== Test 4: 无效 mr_type (999) 应返回 EINVAL =====
TEST(AfPacketMcast, InvalidMrTypeReturnsEinval) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "未找到可用网卡，跳过 multicast 测试";
    }
    ASSERT_NE(ifindex, -2) << "创建 AF_PACKET socket 失败: " << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = 999;  // 非法类型
    errno = 0;
    int rc = setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq));
    EXPECT_EQ(rc, -1);
    EXPECT_EQ(errno, EINVAL) << ErrnoString(errno);
}

// ===== Test 5: 无效 ifindex (99999) 应返回错误 =====
TEST(AfPacketMcast, InvalidIfindexReturnsError) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "未找到可用网卡，跳过 multicast 测试";
    }
    ASSERT_NE(ifindex, -2) << "创建 AF_PACKET socket 失败: " << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = 99999;  // 不存在的接口
    mreq.mr_type = PACKET_MR_PROMISC;
    errno = 0;
    int rc = setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq));
    // 内核 find_iface 返回 ENODEV；容忍 ENXIO/EINVAL 作为合理错误码
    EXPECT_EQ(rc, -1);
    EXPECT_TRUE(errno == ENODEV || errno == ENXIO || errno == EINVAL)
        << "期望 ENODEV/ENXIO/EINVAL，实际 " << ErrnoString(errno);
}

// ===== Test 6: ADD/DROP MEMBERSHIP (PACKET_MR_MULTICAST, 特定多播 MAC) =====
TEST(AfPacketMcast, AddDropMembershipMulticast) {
    FdGuard fd;
    int ifindex = SetupMcastEnv(&fd);
    if (ifindex == -1) {
        GTEST_SKIP() << "未找到可用网卡，跳过 multicast 测试";
    }
    ASSERT_NE(ifindex, -2) << "创建 AF_PACKET socket 失败: " << ErrnoString(errno);

    PacketMreq mreq{};
    mreq.mr_ifindex = static_cast<unsigned int>(ifindex);
    mreq.mr_type = PACKET_MR_MULTICAST;
    mreq.mr_alen = 6;  // 以太网 MAC 长度
    // 多播 MAC: 01:00:5e:00:00:01 (IGMP/多播组映射)
    mreq.mr_address[0] = 0x01;
    mreq.mr_address[1] = 0x00;
    mreq.mr_address[2] = 0x5e;
    mreq.mr_address[3] = 0x00;
    mreq.mr_address[4] = 0x00;
    mreq.mr_address[5] = 0x01;

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_ADD_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);

    // DROP 清除
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, PACKET_DROP_MEMBERSHIP, &mreq, sizeof(mreq)), 0)
        << ErrnoString(errno);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
