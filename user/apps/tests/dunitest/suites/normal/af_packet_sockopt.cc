// af_packet_sockopt.cc - AF_PACKET socket 选项往返测试 (dunitest/gtest)
//
// 由 user/apps/c_unitest/test_af_packet.c 转换而来。
// 验证 DragonOS AF_PACKET setsockopt/getsockopt 的选项往返、
// 取值校验与错误码语义，共 18 个用例 (对应原 C 测试的 18 个断言点)。
//
// 本组测试不依赖网络接口，仅操作套接字选项层。

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

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

// 以太网协议: ETH_P_ALL = 0x0003 (接收所有协议)
inline constexpr int kEthPAll = 0x0003;

// SOL_PACKET 级别 socket 选项 (对应 Linux if_packet.h)
#ifndef PACKET_ADD_MEMBERSHIP
#define PACKET_ADD_MEMBERSHIP 1
#endif
#ifndef PACKET_DROP_MEMBERSHIP
#define PACKET_DROP_MEMBERSHIP 2
#endif
#ifndef PACKET_STATISTICS
#define PACKET_STATISTICS 6
#endif
#ifndef PACKET_COPY_THRESH
#define PACKET_COPY_THRESH 7
#endif
#ifndef PACKET_AUXDATA
#define PACKET_AUXDATA 8
#endif
#ifndef PACKET_ORIGDEV
#define PACKET_ORIGDEV 9
#endif
#ifndef PACKET_VERSION
#define PACKET_VERSION 10
#endif
#ifndef PACKET_RESERVE
#define PACKET_RESERVE 12
#endif
#ifndef PACKET_VNET_HDR
#define PACKET_VNET_HDR 15
#endif
#ifndef PACKET_TX_TIMESTAMP
#define PACKET_TX_TIMESTAMP 16
#endif
#ifndef PACKET_TIMESTAMP
#define PACKET_TIMESTAMP 17
#endif
#ifndef PACKET_QDISC_BYPASS
#define PACKET_QDISC_BYPASS 20
#endif

// TPACKET 版本 (PACKET_VERSION 取值)
#ifndef TPACKET_V1
#define TPACKET_V1 0
#endif
#ifndef TPACKET_V2
#define TPACKET_V2 1
#endif
#ifndef TPACKET_V3
#define TPACKET_V3 2
#endif

namespace {

// RAII fd 守护，出作用域自动 close
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

// 创建 SOCK_RAW 套接字；失败时 ADD_FAILURE 并返回 -1
int MakeRawFd() {
    int fd = socket(AF_PACKET, SOCK_RAW, htons(kEthPAll));
    if (fd < 0) {
        ADD_FAILURE() << "socket(AF_PACKET, SOCK_RAW) 失败: " << ErrnoString(errno)
                      << " (需要 CAP_NET_RAW，请在 root 下运行)";
    }
    return fd;
}

// setsockopt 整型辅助
int SetIntOpt(int fd, int opt, int val) {
    return setsockopt(fd, SOL_PACKET, opt, &val, sizeof(val));
}

// getsockopt 整型辅助
int GetIntOpt(int fd, int opt, int* val) {
    socklen_t len = sizeof(*val);
    return getsockopt(fd, SOL_PACKET, opt, val, &len);
}

}  // namespace

// ===== Test 1: 创建 SOCK_RAW =====
TEST(AfPacketSockopt, CreateRawSocket) {
    FdGuard fd(socket(AF_PACKET, SOCK_RAW, htons(kEthPAll)));
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
}

// ===== Test 2: 创建 SOCK_DGRAM =====
TEST(AfPacketSockopt, CreateDgramSocket) {
    FdGuard fd(socket(AF_PACKET, SOCK_DGRAM, htons(kEthPAll)));
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
}

// ===== Test 3: PACKET_AUXDATA 设 1 往返 =====
TEST(AfPacketSockopt, AuxdataEnableRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_AUXDATA, 1), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_AUXDATA, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, 1);
}

// ===== Test 4: PACKET_AUXDATA 设 0 往返 =====
TEST(AfPacketSockopt, AuxdataDisableRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_AUXDATA, 0), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_AUXDATA, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, 0);
}

// ===== Test 5: PACKET_VERSION = TPACKET_V2 往返 =====
TEST(AfPacketSockopt, VersionV2Roundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_VERSION, TPACKET_V2), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_VERSION, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, TPACKET_V2);
}

// ===== Test 6: PACKET_VERSION = TPACKET_V3 往返 =====
TEST(AfPacketSockopt, VersionV3Roundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_VERSION, TPACKET_V3), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_VERSION, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, TPACKET_V3);
}

// ===== Test 7: PACKET_VERSION 非法值返回 EINVAL =====
TEST(AfPacketSockopt, VersionInvalidReturnsEinval) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    errno = 0;
    EXPECT_EQ(SetIntOpt(fd.Get(), PACKET_VERSION, 999), -1);
    EXPECT_EQ(errno, EINVAL) << ErrnoString(errno);
}

// ===== Test 8: PACKET_COPY_THRESH 往返 =====
TEST(AfPacketSockopt, CopyThreshRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_COPY_THRESH, 4096), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_COPY_THRESH, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, 4096);
}

// ===== Test 9: PACKET_RESERVE 往返 =====
TEST(AfPacketSockopt, ReserveRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_RESERVE, 128), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_RESERVE, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, 128);
}

// ===== Test 10: PACKET_ORIGDEV 0/1 往返 =====
TEST(AfPacketSockopt, OrigdevBoolRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_ORIGDEV, 1), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_ORIGDEV, &got), 0);
    EXPECT_EQ(got, 1);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_ORIGDEV, 0), 0);
    got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_ORIGDEV, &got), 0);
    EXPECT_EQ(got, 0);
}

// ===== Test 11: PACKET_VNET_HDR 0/1 往返 =====
TEST(AfPacketSockopt, VnetHdrBoolRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_VNET_HDR, 1), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_VNET_HDR, &got), 0);
    EXPECT_EQ(got, 1);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_VNET_HDR, 0), 0);
    got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_VNET_HDR, &got), 0);
    EXPECT_EQ(got, 0);
}

// ===== Test 12: PACKET_QDISC_BYPASS 0/1 往返 =====
TEST(AfPacketSockopt, QdiscBypassBoolRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_QDISC_BYPASS, 1), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_QDISC_BYPASS, &got), 0);
    EXPECT_EQ(got, 1);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_QDISC_BYPASS, 0), 0);
    got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_QDISC_BYPASS, &got), 0);
    EXPECT_EQ(got, 0);
}

// ===== Test 13: PACKET_TX_TIMESTAMP = -1 往返 (DragonOS 行为) =====
// 注意: 真实 Linux 不支持该 option 作为可写整数 (返回 ENOPROTOOPT)，
// 本用例仅针对 DragonOS 内核按原始 i32 存储的行为；在原生 Linux 上会失败属预期。
TEST(AfPacketSockopt, TxTimestampRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_TX_TIMESTAMP, -1), 0) << ErrnoString(errno);
    int got = 0;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_TX_TIMESTAMP, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, -1);
}

// ===== Test 14: PACKET_TIMESTAMP = 1 往返 =====
TEST(AfPacketSockopt, TimestampRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_TIMESTAMP, 1), 0) << ErrnoString(errno);
    int got = 0;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_TIMESTAMP, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, 1);
}

// ===== Test 15: PACKET_STATISTICS 返回 8 字节结构 =====
TEST(AfPacketSockopt, StatisticsReturnsStruct) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    struct {
        uint32_t tp_packets;
        uint32_t tp_drops;
    } stats;
    std::memset(&stats, 0xff, sizeof(stats));
    socklen_t len = sizeof(stats);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_PACKET, PACKET_STATISTICS, &stats, &len), 0)
        << ErrnoString(errno);
    EXPECT_EQ(len, sizeof(stats));
}

// ===== Test 16: setsockopt 非法 option 返回 ENOPROTOOPT =====
TEST(AfPacketSockopt, InvalidSetsockoptReturnsEnoprotoopt) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    int val = 1;
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_PACKET, 9999, &val, sizeof(val)), -1);
    EXPECT_EQ(errno, ENOPROTOOPT) << ErrnoString(errno);
}

// ===== Test 17: getsockopt 非法 option 返回 ENOPROTOOPT =====
TEST(AfPacketSockopt, InvalidGetsockoptReturnsEnoprotoopt) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    int got = 0;
    socklen_t len = sizeof(got);
    errno = 0;
    EXPECT_EQ(getsockopt(fd.Get(), SOL_PACKET, 9999, &got, &len), -1);
    EXPECT_EQ(errno, ENOPROTOOPT) << ErrnoString(errno);
}

// ===== Test 18: recvmsg 不返回 ENOSYS =====
TEST(AfPacketSockopt, RecvmsgDoesNotReturnEnosys) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    char rbuf[2048];
    struct iovec iov;
    iov.iov_base = rbuf;
    iov.iov_len = sizeof(rbuf);
    struct msghdr msg;
    std::memset(&msg, 0, sizeof(msg));
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;

    errno = 0;
    // MSG_DONTWAIT: 无数据时立即返回 EAGAIN，不应阻塞
    ssize_t n = recvmsg(fd.Get(), &msg, MSG_DONTWAIT);
    if (n >= 0) {
        SUCCEED();  // 有数据也算 recvmsg 正常工作
        return;
    }
    EXPECT_NE(errno, ENOSYS) << "recvmsg 返回 ENOSYS (功能未实现)";
    // EAGAIN/EWOULDBLOCK 是正常的 (无数据)
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
