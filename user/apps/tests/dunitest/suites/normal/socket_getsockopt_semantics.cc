#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <limits.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstring>
#include <string>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;

    ~FdGuard() { Reset(); }

    int Get() const { return fd_; }

    void Reset(int fd = -1) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

  private:
    int fd_;
};

class MappingGuard {
  public:
    MappingGuard(void* address, size_t length) : address_(address), length_(length) {}
    MappingGuard(const MappingGuard&) = delete;
    MappingGuard& operator=(const MappingGuard&) = delete;

    ~MappingGuard() {
        if (address_ != MAP_FAILED) {
            munmap(address_, length_);
        }
    }

  private:
    void* address_;
    size_t length_;
};

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

void ExpectIntOptionPrefix(int fd, int level, int optname, int expected, socklen_t request_len) {
    unsigned char value[sizeof(int)] = {0xaa, 0xaa, 0xaa, 0xaa};
    unsigned char expected_bytes[sizeof(int)] = {};
    std::memcpy(expected_bytes, &expected, sizeof(expected));

    socklen_t len = request_len;
    ASSERT_EQ(getsockopt(fd, level, optname, value, &len), 0)
            << "getsockopt(" << level << ", " << optname << ") failed: " << ErrnoString(errno);
    EXPECT_EQ(len, request_len);

    for (socklen_t i = 0; i < request_len; ++i) {
        EXPECT_EQ(value[i], expected_bytes[i]) << "byte " << i;
    }
    for (size_t i = request_len; i < sizeof(value); ++i) {
        EXPECT_EQ(value[i], 0xaa) << "byte " << i << " should remain untouched";
    }
}

void ExpectLingerPrefix(int fd, const struct linger& expected, socklen_t request_len) {
    unsigned char value[sizeof(struct linger)] = {};
    std::memset(value, 0xaa, sizeof(value));

    unsigned char expected_bytes[sizeof(struct linger)] = {};
    std::memcpy(expected_bytes, &expected, sizeof(expected));

    socklen_t len = request_len;
    ASSERT_EQ(getsockopt(fd, SOL_SOCKET, SO_LINGER, value, &len), 0)
            << "getsockopt(SO_LINGER) failed: " << ErrnoString(errno);
    EXPECT_EQ(len, request_len);

    for (socklen_t i = 0; i < request_len; ++i) {
        EXPECT_EQ(value[i], expected_bytes[i]) << "byte " << i;
    }
    for (size_t i = request_len; i < sizeof(value); ++i) {
        EXPECT_EQ(value[i], 0xaa) << "byte " << i << " should remain untouched";
    }
}

}  // namespace

TEST(SocketGetsockoptSemantics, IpMulticastIfReturnsRequestedInAddrPrefix) {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET, SOCK_DGRAM) failed: " << ErrnoString(errno);

    unsigned char value[4] = {0xaa, 0xaa, 0xaa, 0xaa};
    socklen_t len = 2;
    ASSERT_EQ(getsockopt(fd.Get(), IPPROTO_IP, IP_MULTICAST_IF, value, &len), 0)
            << "getsockopt(IP_MULTICAST_IF) failed: " << ErrnoString(errno);

    EXPECT_EQ(len, 2u);
    EXPECT_EQ(value[0], 0);
    EXPECT_EQ(value[1], 0);
    EXPECT_EQ(value[2], 0xaa);
    EXPECT_EQ(value[3], 0xaa);
}

TEST(SocketGetsockoptSemantics, BindToDeviceRequiresIfnameSizedBufferWhenBound) {
    FdGuard fd(socket(AF_INET, SOCK_RAW, IPPROTO_ICMP));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET, SOCK_RAW, IPPROTO_ICMP) failed: "
                           << ErrnoString(errno);

    const char device[] = "lo";
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)), 0)
            << "setsockopt(SO_BINDTODEVICE) failed: " << ErrnoString(errno);

    char small[IFNAMSIZ - 1] = {};
    socklen_t small_len = sizeof(small);
    errno = 0;
    EXPECT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, small, &small_len), -1);
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(small_len, sizeof(small));

    char full[IFNAMSIZ] = {};
    socklen_t full_len = sizeof(full);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, full, &full_len), 0)
            << "getsockopt(SO_BINDTODEVICE) failed: " << ErrnoString(errno);
    EXPECT_STREQ(full, device);
    EXPECT_EQ(full_len, sizeof(device));
}

TEST(SocketGetsockoptSemantics, UnixDatagramScalarOptionsAllowShortBuffers) {
    FdGuard fd(socket(AF_UNIX, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_UNIX, SOCK_DGRAM) failed: " << ErrnoString(errno);

    ExpectIntOptionPrefix(fd.Get(), SOL_SOCKET, SO_TYPE, SOCK_DGRAM, 1);
    ExpectIntOptionPrefix(fd.Get(), SOL_SOCKET, SO_DOMAIN, AF_UNIX, 2);
    ExpectIntOptionPrefix(fd.Get(), SOL_SOCKET, SO_PROTOCOL, 0, 2);
}

TEST(SocketGetsockoptSemantics, UnixStreamScalarOptionsAllowShortBuffers) {
    FdGuard fd(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_UNIX, SOCK_STREAM) failed: " << ErrnoString(errno);

    ExpectIntOptionPrefix(fd.Get(), SOL_SOCKET, SO_TYPE, SOCK_STREAM, 1);
    ExpectIntOptionPrefix(fd.Get(), SOL_SOCKET, SO_DOMAIN, AF_UNIX, 2);
    ExpectIntOptionPrefix(fd.Get(), SOL_SOCKET, SO_PROTOCOL, 0, 2);
    ExpectIntOptionPrefix(fd.Get(), SOL_SOCKET, SO_ACCEPTCONN, 0, 1);
}

TEST(SocketGetsockoptSemantics, UnixStreamSndbufClampsToLinuxDefaultMax) {
    FdGuard fd(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_UNIX, SOCK_STREAM) failed: " << ErrnoString(errno);

    const int requested = INT_MAX;
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, SO_SNDBUF, &requested, sizeof(requested)), 0)
            << "setsockopt(SO_SNDBUF) failed: " << ErrnoString(errno);

    int actual = 0;
    socklen_t len = sizeof(actual);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_SNDBUF, &actual, &len), 0)
            << "getsockopt(SO_SNDBUF) failed: " << ErrnoString(errno);
    EXPECT_EQ(len, sizeof(actual));
    EXPECT_EQ(actual, 425984);
}

TEST(SocketGetsockoptSemantics, ConnectedUnixStreamBuffersClampToLinuxDefaultMax) {
    int sv[2] = {-1, -1};
    ASSERT_EQ(socketpair(AF_UNIX, SOCK_STREAM, 0, sv), 0)
            << "socketpair(AF_UNIX, SOCK_STREAM) failed: " << ErrnoString(errno);
    FdGuard first(sv[0]);
    FdGuard second(sv[1]);

    const int requested = INT_MAX;
    ASSERT_EQ(setsockopt(first.Get(), SOL_SOCKET, SO_SNDBUF, &requested, sizeof(requested)), 0)
            << "setsockopt(SO_SNDBUF) failed: " << ErrnoString(errno);
    ASSERT_EQ(setsockopt(first.Get(), SOL_SOCKET, SO_RCVBUF, &requested, sizeof(requested)), 0)
            << "setsockopt(SO_RCVBUF) failed: " << ErrnoString(errno);

    int actual = 0;
    socklen_t len = sizeof(actual);
    ASSERT_EQ(getsockopt(first.Get(), SOL_SOCKET, SO_SNDBUF, &actual, &len), 0)
            << "getsockopt(SO_SNDBUF) failed: " << ErrnoString(errno);
    EXPECT_EQ(len, sizeof(actual));
    EXPECT_EQ(actual, 425984);

    actual = 0;
    len = sizeof(actual);
    ASSERT_EQ(getsockopt(first.Get(), SOL_SOCKET, SO_RCVBUF, &actual, &len), 0)
            << "getsockopt(SO_RCVBUF) failed: " << ErrnoString(errno);
    EXPECT_EQ(len, sizeof(actual));
    EXPECT_EQ(actual, 425984);
}

TEST(SocketGetsockoptSemantics, UnixStreamLingerAllowsShortBuffers) {
    FdGuard fd(socket(AF_UNIX, SOCK_STREAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_UNIX, SOCK_STREAM) failed: " << ErrnoString(errno);

    struct linger expected = {};
    expected.l_onoff = 1;
    expected.l_linger = 7;
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, SO_LINGER, &expected, sizeof(expected)), 0)
            << "setsockopt(SO_LINGER) failed: " << ErrnoString(errno);

    ExpectLingerPrefix(fd.Get(), expected, 1);
    ExpectLingerPrefix(fd.Get(), expected, sizeof(expected) - 1);

    struct linger full = {};
    socklen_t full_len = sizeof(full);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_LINGER, &full, &full_len), 0)
            << "getsockopt(SO_LINGER) failed: " << ErrnoString(errno);
    EXPECT_EQ(full_len, sizeof(full));
    EXPECT_EQ(full.l_onoff, expected.l_onoff);
    EXPECT_EQ(full.l_linger, expected.l_linger);
}

TEST(SocketGetsockoptSemantics, Ipv6IntOptionsUseGenericPrefixCopy) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET6, SOCK_DGRAM) failed: " << ErrnoString(errno);

    int enabled = 1;
    ASSERT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_RECVTCLASS, &enabled, sizeof(enabled)), 0)
            << "setsockopt(IPV6_RECVTCLASS) failed: " << ErrnoString(errno);

    ExpectIntOptionPrefix(fd.Get(), IPPROTO_IPV6, IPV6_RECVTCLASS, enabled, 2);
    ExpectIntOptionPrefix(fd.Get(), IPPROTO_IPV6, IPV6_RECVTCLASS, enabled, 3);
}

TEST(SocketGetsockoptSemantics, Ipv4IntOptionsKeepOneByteShortBufferSpecialCase) {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET, SOCK_DGRAM) failed: " << ErrnoString(errno);

    int ttl = 42;
    ASSERT_EQ(setsockopt(fd.Get(), IPPROTO_IP, IP_MULTICAST_TTL, &ttl, sizeof(ttl)), 0)
            << "setsockopt(IP_MULTICAST_TTL) failed: " << ErrnoString(errno);

    unsigned char value[2] = {0xaa, 0xaa};
    socklen_t len = sizeof(value);
    ASSERT_EQ(getsockopt(fd.Get(), IPPROTO_IP, IP_MULTICAST_TTL, value, &len), 0)
            << "getsockopt(IP_MULTICAST_TTL) failed: " << ErrnoString(errno);
    EXPECT_EQ(len, 1u);
    EXPECT_EQ(value[0], static_cast<unsigned char>(ttl));
    EXPECT_EQ(value[1], 0xaa);
}

TEST(SocketGetsockoptSemantics, RawIpv6ChecksumRejectsShortValuesBeforeCopying) {
    FdGuard fd(socket(AF_INET6, SOCK_RAW, IPPROTO_UDP));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET6, SOCK_RAW, IPPROTO_UDP) failed: "
                           << ErrnoString(errno);

    const int configured_offset = 2;
    const int alternate_offset = 4;
    ASSERT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, &configured_offset,
                         sizeof(configured_offset)),
              0)
            << "setsockopt(IPV6_CHECKSUM) failed: " << ErrnoString(errno);

    for (socklen_t len = 0; len < sizeof(alternate_offset); ++len) {
        errno = 0;
        EXPECT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, &alternate_offset, len), -1)
                << "optlen=" << len;
        EXPECT_EQ(errno, EINVAL) << "optlen=" << len;
    }

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, nullptr, 1), -1);
    EXPECT_EQ(errno, EINVAL);

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    const size_t mapping_size = static_cast<size_t>(page_size) * 2;
    void* mapping = mmap(nullptr, mapping_size, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED) << "mmap failed: " << ErrnoString(errno);
    MappingGuard mapping_guard(mapping, mapping_size);
    auto* second_page = static_cast<unsigned char*>(mapping) + page_size;
    ASSERT_EQ(mprotect(second_page, page_size, PROT_NONE), 0)
            << "mprotect failed: " << ErrnoString(errno);

    auto* three_accessible_bytes = second_page - 3;
    std::memcpy(three_accessible_bytes, &alternate_offset, 3);

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, second_page, 3), -1);
    EXPECT_EQ(errno, EINVAL);

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, three_accessible_bytes, 3), -1);
    EXPECT_EQ(errno, EINVAL);

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, three_accessible_bytes,
                         sizeof(alternate_offset)),
              -1);
    EXPECT_EQ(errno, EFAULT);

    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, nullptr,
                         sizeof(alternate_offset)),
              -1);
    EXPECT_EQ(errno, EFAULT);

    int actual = -1;
    socklen_t actual_len = sizeof(actual);
    ASSERT_EQ(getsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, &actual, &actual_len), 0)
            << "getsockopt(IPV6_CHECKSUM) failed: " << ErrnoString(errno);
    EXPECT_EQ(actual_len, sizeof(actual));
    EXPECT_EQ(actual, configured_offset);

    auto* four_accessible_bytes = second_page - sizeof(alternate_offset);
    std::memcpy(four_accessible_bytes, &alternate_offset, sizeof(alternate_offset));
    ASSERT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, four_accessible_bytes,
                         sizeof(alternate_offset) * 2),
              0)
            << "setsockopt(IPV6_CHECKSUM, oversized optlen) failed: " << ErrnoString(errno);

    actual = -1;
    actual_len = sizeof(actual);
    ASSERT_EQ(getsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, &actual, &actual_len), 0)
            << "getsockopt(IPV6_CHECKSUM) failed: " << ErrnoString(errno);
    EXPECT_EQ(actual_len, sizeof(actual));
    EXPECT_EQ(actual, alternate_offset);
}

TEST(SocketGetsockoptSemantics, RawIpv6ChecksumPreservesStateAndErrorOrdering) {
    int checksum_offset = 2;

    errno = 0;
    EXPECT_EQ(setsockopt(-1, IPPROTO_IPV6, IPV6_CHECKSUM, nullptr, 1), -1);
    EXPECT_EQ(errno, EBADF);

    int pipe_fds[2] = {-1, -1};
    ASSERT_EQ(pipe(pipe_fds), 0) << "pipe failed: " << ErrnoString(errno);
    FdGuard pipe_read(pipe_fds[0]);
    FdGuard pipe_write(pipe_fds[1]);
    errno = 0;
    EXPECT_EQ(setsockopt(pipe_read.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, nullptr, 1), -1);
    EXPECT_EQ(errno, ENOTSOCK);

    FdGuard fd(socket(AF_INET6, SOCK_RAW, IPPROTO_UDP));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET6, SOCK_RAW, IPPROTO_UDP) failed: "
                           << ErrnoString(errno);
    ASSERT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, &checksum_offset,
                         sizeof(checksum_offset)),
              0)
            << "setsockopt(IPV6_CHECKSUM) failed: " << ErrnoString(errno);

    int actual = -1;
    socklen_t actual_len = sizeof(actual);
    ASSERT_EQ(getsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, &actual, &actual_len), 0)
            << "getsockopt(IPV6_CHECKSUM) failed: " << ErrnoString(errno);
    EXPECT_EQ(actual_len, sizeof(actual));
    EXPECT_EQ(actual, checksum_offset);

    const int odd_offset = 3;
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, &odd_offset,
                         sizeof(odd_offset)),
              -1);
    EXPECT_EQ(errno, EINVAL);

    actual = -1;
    actual_len = sizeof(actual);
    ASSERT_EQ(getsockopt(fd.Get(), IPPROTO_IPV6, IPV6_CHECKSUM, &actual, &actual_len), 0)
            << "getsockopt(IPV6_CHECKSUM) failed: " << ErrnoString(errno);
    EXPECT_EQ(actual_len, sizeof(actual));
    EXPECT_EQ(actual, checksum_offset);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
