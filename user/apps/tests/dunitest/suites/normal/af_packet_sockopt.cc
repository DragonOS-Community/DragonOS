// af_packet_sockopt.cc - AF_PACKET socket option round-trip tests (dunitest/gtest)
//
// Converted from user/apps/c_unitest/test_af_packet.c.
// Validates DragonOS AF_PACKET setsockopt/getsockopt option round-trips,
// value checks and error-code semantics.
//
// This test suite does not depend on network interfaces; it only operates
// at the socket option layer.

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/uio.h>
#include <time.h>
#include <unistd.h>

#include <cstdint>
#include <cstring>
#include <string>
#include <tuple>
#include <utility>

// ---- Manually define constants (DragonOS musl may lack if_packet.h) ----

#ifndef AF_PACKET
#define AF_PACKET 17
#endif

#ifndef SOL_PACKET
#define SOL_PACKET 263
#endif

// Ethernet protocol: ETH_P_ALL = 0x0003 (receive all protocols)
inline constexpr int kEthPAll = 0x0003;
inline constexpr int kPrivateEtherType = 0x88b5;
inline constexpr int kSoRcvtimeoOld = 20;
inline constexpr int kSoSndtimeoOld = 21;
inline constexpr int kSoRcvtimeoNew = 66;
inline constexpr int kSoSndtimeoNew = 67;
inline constexpr int kSoAttachFilter = 26;

struct TestSockFilter {
    uint16_t code;
    uint8_t jt;
    uint8_t jf;
    uint32_t k;
};

struct TestSockFprog {
    uint16_t len;
    TestSockFilter* filter;
};

struct TestSockAddrLl {
    uint16_t sll_family;
    uint16_t sll_protocol;
    int32_t sll_ifindex;
    uint16_t sll_hatype;
    uint8_t sll_pkttype;
    uint8_t sll_halen;
    uint8_t sll_addr[8];
};

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

int MakeTimeoutFd() {
    int fd = socket(AF_PACKET, SOCK_RAW, htons(kPrivateEtherType));
    if (fd < 0) {
        ADD_FAILURE() << "socket(AF_PACKET, SOCK_RAW) failed: " << ErrnoString(errno)
                      << " (requires CAP_NET_RAW, please run as root)";
    }
    return fd;
}

int64_t MonotonicMillis() {
    struct timespec ts {};
    EXPECT_EQ(clock_gettime(CLOCK_MONOTONIC, &ts), 0);
    return static_cast<int64_t>(ts.tv_sec) * 1000 + ts.tv_nsec / 1000000;
}

ssize_t ReadCall(int fd) {
    char byte;
    return read(fd, &byte, sizeof(byte));
}

ssize_t RecvCall(int fd) {
    char byte;
    return recv(fd, &byte, sizeof(byte), 0);
}

ssize_t RecvfromCall(int fd) {
    char byte;
    return recvfrom(fd, &byte, sizeof(byte), 0, nullptr, nullptr);
}

ssize_t RecvmsgCall(int fd) {
    char byte;
    struct iovec iov { &byte, sizeof(byte) };
    struct msghdr msg {};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    return recvmsg(fd, &msg, 0);
}

void NoopSignalHandler(int) {}

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

// Linux 6.6 packet_do_bind() substitutes po->num when sll_protocol is zero.
// Binding with protocol zero therefore selects only the interface and keeps
// the protocol supplied to socket(), rather than disabling packet delivery.
TEST(AfPacketSockopt, ZeroBindProtocolKeepsSocketProtocol) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);

    TestSockAddrLl bind_addr{};
    bind_addr.sll_family = AF_PACKET;
    ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&bind_addr), sizeof(bind_addr)), 0)
        << ErrnoString(errno);

    TestSockAddrLl local_addr{};
    socklen_t local_len = sizeof(local_addr);
    ASSERT_EQ(getsockname(fd.Get(), reinterpret_cast<sockaddr*>(&local_addr), &local_len), 0)
        << ErrnoString(errno);
    EXPECT_GE(local_len, 12U);
    EXPECT_LE(local_len, sizeof(local_addr));
    EXPECT_EQ(ntohs(local_addr.sll_protocol), kEthPAll);
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

TEST(AfPacketSockopt, SocketBufferOptionsRoundTrip) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);

    for (auto [option, requested] :
         {std::pair{SO_RCVBUF, 8192}, std::pair{SO_SNDBUF, 12288}}) {
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &requested, sizeof(requested)), 0)
            << "option=" << option << ": " << ErrnoString(errno);
        int actual = 0;
        socklen_t len = sizeof(actual);
        ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, option, &actual, &len), 0)
            << "option=" << option << ": " << ErrnoString(errno);
        EXPECT_EQ(len, sizeof(actual));
        EXPECT_EQ(actual, requested * 2) << "option=" << option;
    }
}

TEST(AfPacketSockopt, SocketBufferOptionsFollowLinuxBounds) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);

    for (auto [option, minimum] :
         {std::pair{SO_RCVBUF, 2304}, std::pair{SO_SNDBUF, 4608}}) {
        int requested = 0;
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &requested, sizeof(requested)), 0);
        int actual = 0;
        socklen_t len = sizeof(actual);
        ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, option, &actual, &len), 0);
        EXPECT_EQ(actual, minimum) << "option=" << option;

        requested = -1;
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &requested, sizeof(requested)), 0);
        len = sizeof(actual);
        ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, option, &actual, &len), 0);
        EXPECT_EQ(actual, 425984) << "option=" << option;

        requested = 212993;
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &requested, sizeof(requested)), 0);
        len = sizeof(actual);
        ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, option, &actual, &len), 0);
        EXPECT_EQ(actual, 425984) << "option=" << option;

        struct {
            int requested;
            int ignored;
        } extended{4096, 0x12345678};
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &extended, sizeof(extended)), 0);
        len = sizeof(actual);
        ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, option, &actual, &len), 0);
        EXPECT_EQ(actual, 8192) << "option=" << option;

        requested = 4096;
        for (socklen_t short_len : {0U, 1U, 2U, 3U}) {
            errno = 0;
            EXPECT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &requested, short_len), -1);
            EXPECT_EQ(errno, EINVAL) << "option=" << option << " len=" << short_len;
        }
    }
}

TEST(AfPacketSockopt, SocketBufferOptionsAreIndependent) {
    FdGuard first(MakeRawFd());
    FdGuard second(MakeRawFd());
    ASSERT_GE(first.Get(), 0);
    ASSERT_GE(second.Get(), 0);

    int first_default_send = 0;
    int second_default_receive = 0;
    socklen_t len = sizeof(int);
    ASSERT_EQ(getsockopt(first.Get(), SOL_SOCKET, SO_SNDBUF, &first_default_send, &len), 0);
    len = sizeof(int);
    ASSERT_EQ(getsockopt(second.Get(), SOL_SOCKET, SO_RCVBUF, &second_default_receive, &len), 0);

    int requested = 16384;
    ASSERT_EQ(setsockopt(first.Get(), SOL_SOCKET, SO_RCVBUF, &requested, sizeof(requested)), 0);
    int actual = 0;
    len = sizeof(actual);
    ASSERT_EQ(getsockopt(first.Get(), SOL_SOCKET, SO_SNDBUF, &actual, &len), 0);
    EXPECT_EQ(actual, first_default_send);
    len = sizeof(actual);
    ASSERT_EQ(getsockopt(second.Get(), SOL_SOCKET, SO_RCVBUF, &actual, &len), 0);
    EXPECT_EQ(actual, second_default_receive);

    int first_receive = 0;
    len = sizeof(first_receive);
    ASSERT_EQ(getsockopt(first.Get(), SOL_SOCKET, SO_RCVBUF, &first_receive, &len), 0);
    requested = 24576;
    ASSERT_EQ(setsockopt(first.Get(), SOL_SOCKET, SO_SNDBUF, &requested, sizeof(requested)), 0);
    len = sizeof(actual);
    ASSERT_EQ(getsockopt(first.Get(), SOL_SOCKET, SO_RCVBUF, &actual, &len), 0);
    EXPECT_EQ(actual, first_receive);
    len = sizeof(actual);
    ASSERT_EQ(getsockopt(second.Get(), SOL_SOCKET, SO_RCVBUF, &actual, &len), 0);
    EXPECT_EQ(actual, second_default_receive);
}

TEST(AfPacketSockopt, AttachFilterIsNotSilentlyAccepted) {
    FdGuard fd(MakeRawFd());
    ASSERT_GE(fd.Get(), 0);
    TestSockFilter accept_all{0x06, 0, 0, 0xffffffff};
    TestSockFprog program{1, &accept_all};
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoAttachFilter, &program, sizeof(program)), -1);
    EXPECT_EQ(errno, ENOPROTOOPT) << ErrnoString(errno);
}

TEST(AfPacketSockopt, ReceiveTimeoutOldAndNewRoundTrip) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);

    for (int option : {kSoRcvtimeoOld, kSoRcvtimeoNew}) {
        struct timeval set_value { 1, 234567 };
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &set_value, sizeof(set_value)), 0)
            << "option=" << option << ": " << ErrnoString(errno);

        struct timeval got_value {};
        socklen_t len = sizeof(got_value);
        ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, option, &got_value, &len), 0)
            << "option=" << option << ": " << ErrnoString(errno);
        EXPECT_EQ(len, sizeof(got_value));
        EXPECT_EQ(got_value.tv_sec, set_value.tv_sec);
        // DragonOS HZ=250, matching Linux's ceil-to-tick socket timeout storage.
        EXPECT_EQ(got_value.tv_usec, 236000);
    }
}

TEST(AfPacketSockopt, SendTimeoutOldAndNewShareOneState) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);

    for (int option : {kSoSndtimeoOld, kSoSndtimeoNew}) {
        struct timeval default_value { -1, -1 };
        socklen_t default_len = sizeof(default_value);
        ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, option, &default_value, &default_len), 0)
            << "option=" << option << ": " << ErrnoString(errno);
        EXPECT_EQ(default_value.tv_sec, 0);
        EXPECT_EQ(default_value.tv_usec, 0);
    }

    for (auto [set_option, input_usec, expected_usec] :
         {std::tuple{kSoSndtimeoOld, 234567L, 236000L},
          std::tuple{kSoSndtimeoNew, 345678L, 348000L}}) {
        struct timeval set_value { 1, input_usec };
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, set_option, &set_value, sizeof(set_value)), 0)
            << "option=" << set_option << ": " << ErrnoString(errno);
        for (int get_option : {kSoSndtimeoOld, kSoSndtimeoNew}) {
            struct timeval got {};
            socklen_t len = sizeof(got);
            ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, get_option, &got, &len), 0)
                << "set=" << set_option << " get=" << get_option << ": " << ErrnoString(errno);
            EXPECT_EQ(got.tv_sec, 1);
            EXPECT_EQ(got.tv_usec, expected_usec);
        }
    }

    struct timeval zero {};
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoSndtimeoOld, &zero, sizeof(zero)), 0);
    struct timeval got { -1, -1 };
    socklen_t len = sizeof(got);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, kSoSndtimeoNew, &got, &len), 0);
    EXPECT_EQ(got.tv_sec, 0);
    EXPECT_EQ(got.tv_usec, 0);
}

TEST(AfPacketSockopt, SendAndReceiveTimeoutsAreIndependent) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);
    struct timeval receive_value { 0, 111111 };
    struct timeval send_value { 0, 222222 };
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &receive_value,
                         sizeof(receive_value)),
              0);
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoSndtimeoNew, &send_value,
                         sizeof(send_value)),
              0);

    struct timeval got_receive {};
    struct timeval got_send {};
    socklen_t receive_len = sizeof(got_receive);
    socklen_t send_len = sizeof(got_send);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoNew, &got_receive, &receive_len), 0);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, kSoSndtimeoOld, &got_send, &send_len), 0);
    EXPECT_EQ(got_receive.tv_sec, 0);
    EXPECT_EQ(got_receive.tv_usec, 112000);
    EXPECT_EQ(got_send.tv_sec, 0);
    EXPECT_EQ(got_send.tv_usec, 224000);
}

TEST(AfPacketSockopt, SendTimeoutRejectsInvalidAndShortNativeValues) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);

    for (int option : {kSoSndtimeoOld, kSoSndtimeoNew}) {
        for (long usec : {-1L, 1000000L}) {
            struct timeval invalid { 0, usec };
            errno = 0;
            EXPECT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &invalid, sizeof(invalid)), -1);
            EXPECT_EQ(errno, EDOM) << "option=" << option << " usec=" << usec;
        }
        struct timeval valid { 0, 50000 };
        for (socklen_t len : {static_cast<socklen_t>(8), static_cast<socklen_t>(12)}) {
            errno = 0;
            EXPECT_EQ(setsockopt(fd.Get(), SOL_SOCKET, option, &valid, len), -1);
            EXPECT_EQ(errno, EINVAL) << "option=" << option << " len=" << len;
        }
    }
}

TEST(AfPacketSockopt, ReceiveTimeoutZeroMeansInfinite) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);
    struct timeval zero {};
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &zero, sizeof(zero)), 0)
        << ErrnoString(errno);
    struct timeval got { -1, -1 };
    socklen_t len = sizeof(got);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &got, &len), 0)
        << ErrnoString(errno);
    EXPECT_EQ(got.tv_sec, 0);
    EXPECT_EQ(got.tv_usec, 0);
}

TEST(AfPacketSockopt, ReceiveTimeoutRejectsInvalidUsecAndShortNativeLayout) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);

    struct timeval invalid { 0, 1000000 };
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &invalid, sizeof(invalid)), -1);
    EXPECT_EQ(errno, EDOM) << ErrnoString(errno);

    struct timeval valid { 0, 50000 };
    for (socklen_t len : {static_cast<socklen_t>(8), static_cast<socklen_t>(12)}) {
        errno = 0;
        EXPECT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &valid, len), -1);
        EXPECT_EQ(errno, EINVAL) << "len=" << len << ": " << ErrnoString(errno);
    }
}

TEST(AfPacketSockopt, NegativeReceiveTimeoutExpiresImmediatelyAndReadsBackZero) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);
    struct timeval negative { -1, 0 };
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &negative, sizeof(negative)), 0)
        << ErrnoString(errno);

    char byte;
    errno = 0;
    EXPECT_EQ(recv(fd.Get(), &byte, sizeof(byte), 0), -1);
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << ErrnoString(errno);

    struct timeval got { -1, -1 };
    socklen_t len = sizeof(got);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &got, &len), 0);
    EXPECT_EQ(got.tv_sec, 0);
    EXPECT_EQ(got.tv_usec, 0);
}

TEST(AfPacketSockopt, UnknownSocketOptionReturnsEnoprotoopt) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);
    int value = 1;
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_SOCKET, 0x7fff, &value, sizeof(value)), -1);
    EXPECT_EQ(errno, ENOPROTOOPT) << ErrnoString(errno);
}

TEST(AfPacketSockopt, AllReceiveEntrypointsHonorOneTimeoutBudget) {
    using ReceiveCall = ssize_t (*)(int);
    const ReceiveCall calls[] = {ReadCall, RecvCall, RecvfromCall, RecvmsgCall};

    for (ReceiveCall call : calls) {
        FdGuard fd(MakeTimeoutFd());
        ASSERT_GE(fd.Get(), 0);
        struct timeval timeout { 0, 50000 };
        ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &timeout, sizeof(timeout)), 0)
            << ErrnoString(errno);

        const int64_t started = MonotonicMillis();
        errno = 0;
        EXPECT_EQ(call(fd.Get()), -1);
        const int saved_errno = errno;
        const int64_t elapsed = MonotonicMillis() - started;
        EXPECT_TRUE(saved_errno == EAGAIN || saved_errno == EWOULDBLOCK)
            << ErrnoString(saved_errno);
        EXPECT_GE(elapsed, 20) << "timeout returned too early";
        EXPECT_LT(elapsed, 2000) << "timeout budget was not bounded";
    }
}

TEST(AfPacketSockopt, HugeFiniteTimeoutDoesNotWrapToImmediateExpiry) {
    FdGuard fd(MakeTimeoutFd());
    ASSERT_GE(fd.Get(), 0);
    // Above u64::MAX microseconds, but below Linux MAX_SCHEDULE_TIMEOUT/HZ.
    struct timeval huge { 20000000000000LL, 0 };
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &huge, sizeof(huge)), 0)
        << ErrnoString(errno);
    struct timeval got {};
    socklen_t got_len = sizeof(got);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, kSoRcvtimeoOld, &got, &got_len), 0);
    EXPECT_EQ(got.tv_sec, huge.tv_sec);
    EXPECT_EQ(got.tv_usec, 0);

    struct sigaction action {};
    struct sigaction old_action {};
    action.sa_handler = NoopSignalHandler;
    sigemptyset(&action.sa_mask);
    ASSERT_EQ(sigaction(SIGALRM, &action, &old_action), 0);
    alarm(1);
    char byte;
    errno = 0;
    EXPECT_EQ(recv(fd.Get(), &byte, sizeof(byte), 0), -1);
    const int saved_errno = errno;
    alarm(0);
    EXPECT_EQ(sigaction(SIGALRM, &old_action, nullptr), 0);
    EXPECT_EQ(saved_errno, EINTR) << ErrnoString(saved_errno);
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
