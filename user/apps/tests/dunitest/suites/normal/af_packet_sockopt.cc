// af_packet_sockopt.cc - AF_PACKET socket option round-trip tests (dunitest/gtest)
//
// Converted from user/apps/c_unitest/test_af_packet.c.
// Validates DragonOS AF_PACKET setsockopt/getsockopt option round-trips,
// value checks and error-code semantics, 18 test cases total (corresponding
// to the 18 assertion points in the original C test).
//
// This test suite does not depend on network interfaces; it only operates
// at the socket option layer.

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <sys/socket.h>
#include <sys/uio.h>
#include <unistd.h>

#include <cstdint>
#include <cstring>
#include <string>

// ---- Manually define constants (DragonOS musl may lack if_packet.h) ----

#ifndef AF_PACKET
#define AF_PACKET 17
#endif

#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif

// Ethernet protocol: ETH_P_ALL = 0x0003 (receive all protocols)
inline constexpr int kEthPAll = 0x0003;

// SOL_PACKET level socket options (matching Linux if_packet.h)
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

// TPACKET versions (PACKET_VERSION values)
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

// RAII fd guard, auto-closes when leaving scope
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

// Create a SOCK_RAW socket; on failure ADD_FAILURE and return -1
int MakeRawFd() {
    int fd = socket(AF_PACKET, SOCK_RAW, htons(kEthPAll));
    if (fd < 0) {
        ADD_FAILURE() << "socket(AF_PACKET, SOCK_RAW) failed: " << ErrnoString(errno)
                      << " (requires CAP_NET_RAW, please run as root)";
    }
    return fd;
}

// setsockopt integer helper
int SetIntOpt(int fd, int opt, int val) {
    return setsockopt(fd, SOL_PACKET, opt, &val, sizeof(val));
}

// getsockopt integer helper
int GetIntOpt(int fd, int opt, int* val) {
    socklen_t len = sizeof(*val);
    return getsockopt(fd, SOL_PACKET, opt, val, &len);
}

}  // namespace

// ===== Test 1: Create SOCK_RAW =====
TEST(AfPacketSockopt, CreateRawSocket) {
    FdGuard fd(socket(AF_PACKET, SOCK_RAW, htons(kEthPAll)));
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
}

// ===== Test 2: Create SOCK_DGRAM =====
TEST(AfPacketSockopt, CreateDgramSocket) {
    FdGuard fd(socket(AF_PACKET, SOCK_DGRAM, htons(kEthPAll)));
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
}

// ===== Test 3: PACKET_AUXDATA set to 1 round-trip =====
TEST(AfPacketSockopt, AuxdataEnableRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_AUXDATA, 1), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_AUXDATA, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, 1);
}

// ===== Test 4: PACKET_AUXDATA set to 0 round-trip =====
TEST(AfPacketSockopt, AuxdataDisableRoundtrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    ASSERT_EQ(SetIntOpt(fd.Get(), PACKET_AUXDATA, 0), 0) << ErrnoString(errno);
    int got = -1;
    ASSERT_EQ(GetIntOpt(fd.Get(), PACKET_AUXDATA, &got), 0) << ErrnoString(errno);
    EXPECT_EQ(got, 0);
}

// origin/master intentionally accepts unsupported setsockopt names so applications
// can continue, but it does not advertise fake state through getsockopt. PR #2046
// must preserve that compatibility boundary.
TEST(AfPacketSockopt, UnsupportedSetSucceedsButGetIsNotAdvertised) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    const int options[] = {PACKET_COPY_THRESH, PACKET_ORIGDEV, PACKET_VERSION,
                           PACKET_RESERVE, PACKET_VNET_HDR, PACKET_TX_TIMESTAMP,
                           PACKET_TIMESTAMP, PACKET_QDISC_BYPASS, 9999};
    for (int option : options) {
        ASSERT_EQ(SetIntOpt(fd.Get(), option, 999), 0)
            << "setsockopt option=" << option << ": " << ErrnoString(errno);
        int got = 0;
        errno = 0;
        EXPECT_EQ(GetIntOpt(fd.Get(), option, &got), -1) << "option=" << option;
        EXPECT_EQ(errno, ENOPROTOOPT) << "option=" << option << ": " << ErrnoString(errno);
    }
}

// ===== Test 15: PACKET_STATISTICS returns 8-byte struct =====
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

// ===== Test 17: getsockopt invalid option returns ENOPROTOOPT =====
TEST(AfPacketSockopt, InvalidGetsockoptReturnsEnoprotoopt) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    int got = 0;
    socklen_t len = sizeof(got);
    errno = 0;
    EXPECT_EQ(getsockopt(fd.Get(), SOL_PACKET, 9999, &got, &len), -1);
    EXPECT_EQ(errno, ENOPROTOOPT) << ErrnoString(errno);
}

// ===== Test 18: recvmsg does not return ENOSYS =====
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
    // MSG_DONTWAIT: returns EAGAIN immediately when no data; should not block
    ssize_t n = recvmsg(fd.Get(), &msg, MSG_DONTWAIT);
    if (n >= 0) {
        SUCCEED();  // having data also counts as recvmsg working correctly
        return;
    }
    EXPECT_NE(errno, ENOSYS) << "recvmsg returned ENOSYS (feature not implemented)";
    // EAGAIN/EWOULDBLOCK is normal (no data)
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
