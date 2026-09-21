#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <netdb.h>
#include <fcntl.h>
#include <linux/errqueue.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <sys/mman.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstdint>
#include <cstring>
#include <string>
#include <vector>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;

    ~FdGuard() {
        if (fd_ >= 0) {
            close(fd_);
        }
    }

    int Get() const { return fd_; }

  private:
    int fd_;
};

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

sockaddr_in6 MakeIpv6Addr(const char* addr, uint16_t port) {
    sockaddr_in6 sa {};
    sa.sin6_family = AF_INET6;
    sa.sin6_port = htons(port);
    EXPECT_EQ(inet_pton(AF_INET6, addr, &sa.sin6_addr), 1);
    return sa;
}

void ExpectIpv6Name(int fd, bool peer, const char* address, uint16_t port) {
    sockaddr_in6 actual;
    std::memset(&actual, 0xa5, sizeof(actual));
    socklen_t length = sizeof(actual);
    ASSERT_EQ(peer ? getpeername(fd, reinterpret_cast<sockaddr*>(&actual), &length)
                   : getsockname(fd, reinterpret_cast<sockaddr*>(&actual), &length), 0)
        << ErrnoString(errno);
    const sockaddr_in6 expected = MakeIpv6Addr(address, port);
    EXPECT_EQ(length, sizeof(actual));
    EXPECT_EQ(actual.sin6_family, AF_INET6);
    EXPECT_EQ(std::memcmp(&actual.sin6_addr, &expected.sin6_addr, sizeof(in6_addr)), 0);
    EXPECT_EQ(actual.sin6_port, expected.sin6_port);
    EXPECT_EQ(actual.sin6_flowinfo, 0U);
    EXPECT_EQ(actual.sin6_scope_id, 0U);
}

uint16_t LocalPort(int fd) {
    sockaddr_in6 address {};
    socklen_t length = sizeof(address);
    EXPECT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(&address), &length), 0);
    return ntohs(address.sin6_port);
}

void CheckOversizeErrqueue(int fd, const sockaddr* destination, socklen_t length,
                          const sockaddr_in6* expected_error_address) {
    std::vector<char> payload(65536, 'x');
    errno = 0;
    ASSERT_EQ(sendto(fd, payload.data(), payload.size(), 0, destination, length), -1);
    ASSERT_EQ(errno, EMSGSIZE);

    alignas(cmsghdr) char control[256] {};
    sockaddr_in6 address {};
    msghdr msg {};
    msg.msg_name = &address;
    msg.msg_namelen = sizeof(address);
    msg.msg_control = control;
    msg.msg_controllen = sizeof(control);
    errno = 0;
    const ssize_t result = recvmsg(fd, &msg, MSG_ERRQUEUE | MSG_DONTWAIT);
    if (expected_error_address == nullptr) {
        ASSERT_EQ(result, -1);
        EXPECT_EQ(errno, EAGAIN);
        return;
    }
    ASSERT_EQ(result, 0) << ErrnoString(errno);
    EXPECT_NE(msg.msg_flags & MSG_ERRQUEUE, 0);
    EXPECT_EQ(msg.msg_flags & MSG_CTRUNC, 0);
    ASSERT_EQ(msg.msg_namelen, sizeof(address));
    EXPECT_EQ(address.sin6_family, AF_INET6);
    EXPECT_EQ(address.sin6_port, expected_error_address->sin6_port);
    EXPECT_EQ(std::memcmp(&address.sin6_addr, &expected_error_address->sin6_addr,
                          sizeof(in6_addr)), 0);
    const cmsghdr* cmsg = CMSG_FIRSTHDR(&msg);
    ASSERT_NE(cmsg, nullptr);
    EXPECT_EQ(cmsg->cmsg_level, IPPROTO_IPV6);
    EXPECT_EQ(cmsg->cmsg_type, IPV6_RECVERR);
    ASSERT_GE(cmsg->cmsg_len, CMSG_LEN(sizeof(sock_extended_err)));
    sock_extended_err error {};
    std::memcpy(&error, CMSG_DATA(cmsg), sizeof(error));
    EXPECT_EQ(error.ee_errno, static_cast<unsigned>(EMSGSIZE));
    EXPECT_EQ(error.ee_origin, SO_EE_ORIGIN_LOCAL);
    msg.msg_namelen = sizeof(address);
    msg.msg_controllen = sizeof(control);
    errno = 0;
    EXPECT_EQ(recvmsg(fd, &msg, MSG_ERRQUEUE | MSG_DONTWAIT), -1);
    EXPECT_EQ(errno, EAGAIN);
}

}  // namespace

TEST(UdpIpv6SendSemantics, OversizeErrqueueUsesPacketFamily) {
    // IPv4 socket, IPv6 socket with IPv4 sockaddr, mapped IPv6, native IPv6.
    for (int mode = 0; mode < 4; ++mode) {
        for (bool connected : {false, true}) {
            for (int errors : {0, 1}) {
                SCOPED_TRACE(testing::Message() << "mode=" << mode
                             << " connected=" << connected << " recverr=" << errors);
                FdGuard fd(socket(mode == 0 ? AF_INET : AF_INET6, SOCK_DGRAM, 0));
                ASSERT_GE(fd.Get(), 0);
                ASSERT_EQ(setsockopt(fd.Get(), mode == 0 ? IPPROTO_IP : IPPROTO_IPV6,
                                    mode == 0 ? IP_RECVERR : IPV6_RECVERR,
                                    &errors, sizeof(errors)), 0);
                sockaddr_in ipv4 {};
                ipv4.sin_family = AF_INET;
                ipv4.sin_port = htons(12345);
                ipv4.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
                sockaddr_in6 ipv6 = MakeIpv6Addr(mode == 3 ? "::1" : "::ffff:127.0.0.1", 12345);
                const sockaddr* dest = mode < 2 ? reinterpret_cast<sockaddr*>(&ipv4)
                                                : reinterpret_cast<sockaddr*>(&ipv6);
                const socklen_t length = mode < 2 ? sizeof(ipv4) : sizeof(ipv6);
                if (connected) {
                    ASSERT_EQ(connect(fd.Get(), dest, length), 0);
                }
                ASSERT_NO_FATAL_FAILURE(CheckOversizeErrqueue(
                    fd.Get(), connected ? nullptr : dest, connected ? 0 : length,
                    mode == 3 && errors ? &ipv6 : nullptr));
            }
        }
    }
}

TEST(UdpIpv6SendSemantics, OversizeSendtoErrqueueUsesExplicitDestination) {
    for (bool native_destination : {false, true}) {
        SCOPED_TRACE(native_destination);
        FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
        ASSERT_GE(fd.Get(), 0);
        const int on = 1;
        ASSERT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_RECVERR, &on, sizeof(on)), 0);
        // Connecting to a mapped peer can select an IPv4-only source, for which
        // Linux rejects a native IPv6 override before checking its payload.
        sockaddr_in6 peer = MakeIpv6Addr("::1", 12345);
        sockaddr_in6 dest = MakeIpv6Addr(native_destination ? "::1" : "::ffff:127.0.0.1", 12346);
        ASSERT_EQ(connect(fd.Get(), reinterpret_cast<sockaddr*>(&peer), sizeof(peer)), 0);
        ASSERT_NO_FATAL_FAILURE(CheckOversizeErrqueue(
            fd.Get(), reinterpret_cast<sockaddr*>(&dest), sizeof(dest),
            native_destination ? &dest : nullptr));
    }
}

TEST(UdpIpv6SendSemantics, DualStackNamesPreserveIpv6SocketFamily) {
    for (bool mapped : {false, true}) {
        SCOPED_TRACE(mapped ? "mapped IPv6 input" : "IPv4 input");
        FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
        ASSERT_GE(fd.Get(), 0);
        sockaddr_in ipv4 {};
        ipv4.sin_family = AF_INET;
        ipv4.sin_port = htons(12345);
        ipv4.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        sockaddr_in6 ipv6 = MakeIpv6Addr("::ffff:127.0.0.1", 12345);
        ASSERT_EQ(connect(fd.Get(), mapped ? reinterpret_cast<sockaddr*>(&ipv6)
                                          : reinterpret_cast<sockaddr*>(&ipv4),
                          mapped ? sizeof(ipv6) : sizeof(ipv4)), 0) << ErrnoString(errno);
        const uint16_t port = LocalPort(fd.Get());
        ASSERT_NE(port, 0);
        ExpectIpv6Name(fd.Get(), false, "::ffff:127.0.0.1", port);
        ExpectIpv6Name(fd.Get(), true, "::ffff:127.0.0.1", 12345);
    }
}

TEST(UdpIpv6SendSemantics, DisconnectAndReuseAcrossAddressFamilies) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0);
    sockaddr_in6 ipv6 = MakeIpv6Addr("::1", 12345);
    ASSERT_EQ(connect(fd.Get(), reinterpret_cast<sockaddr*>(&ipv6), sizeof(ipv6)), 0);
    const uint16_t port = LocalPort(fd.Get());
    ASSERT_NE(port, 0);
    ExpectIpv6Name(fd.Get(), false, "::1", port);
    ExpectIpv6Name(fd.Get(), true, "::1", 12345);

    sockaddr disconnect {};
    disconnect.sa_family = AF_UNSPEC;
    ASSERT_EQ(connect(fd.Get(), &disconnect, sizeof(disconnect)), 0);
    sockaddr_in6 peer {};
    socklen_t length = sizeof(peer);
    ASSERT_EQ(getpeername(fd.Get(), reinterpret_cast<sockaddr*>(&peer), &length), -1);
    EXPECT_EQ(errno, ENOTCONN);

    sockaddr_in ipv4 {};
    ipv4.sin_family = AF_INET;
    ipv4.sin_port = htons(23456);
    ipv4.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(connect(fd.Get(), reinterpret_cast<sockaddr*>(&ipv4), sizeof(ipv4)), 0);
    const uint16_t reconnected_port = LocalPort(fd.Get());
    ASSERT_NE(reconnected_port, 0);
    ExpectIpv6Name(fd.Get(), false, "::ffff:127.0.0.1", reconnected_port);
    ExpectIpv6Name(fd.Get(), true, "::ffff:127.0.0.1", 23456);
}

TEST(UdpIpv6SendSemantics, WildcardBindKeepsPortWhenConnectingToIpv4) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0);
    sockaddr_in6 address = MakeIpv6Addr("::", 0);
    ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
    const uint16_t port = LocalPort(fd.Get());
    ASSERT_NE(port, 0);
    ExpectIpv6Name(fd.Get(), false, "::", port);
    address = MakeIpv6Addr("::ffff:127.0.0.1", 12345);
    ASSERT_EQ(connect(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
    ExpectIpv6Name(fd.Get(), false, "::ffff:127.0.0.1", port);
    ExpectIpv6Name(fd.Get(), true, "::ffff:127.0.0.1", 12345);
}

TEST(UdpIpv6SendSemantics, UnboundIpv6NamesRemainUnspecifiedAndNotConnected) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0);
    ExpectIpv6Name(fd.Get(), false, "::", 0);
    sockaddr_in6 peer {};
    socklen_t length = sizeof(peer);
    ASSERT_EQ(getpeername(fd.Get(), reinterpret_cast<sockaddr*>(&peer), &length), -1);
    EXPECT_EQ(errno, ENOTCONN);
}

TEST(UdpIpv6SendSemantics, Ipv4NamesKeepIpv4Representation) {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0);
    sockaddr_in destination {};
    destination.sin_family = AF_INET;
    destination.sin_port = htons(12345);
    destination.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(connect(fd.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0);
    for (bool peer : {false, true}) {
        SCOPED_TRACE(peer ? "peer" : "local");
        sockaddr_in actual;
        std::memset(&actual, 0xa5, sizeof(actual));
        socklen_t length = sizeof(actual);
        ASSERT_EQ(peer ? getpeername(fd.Get(), reinterpret_cast<sockaddr*>(&actual), &length)
                       : getsockname(fd.Get(), reinterpret_cast<sockaddr*>(&actual), &length), 0);
        EXPECT_EQ(length, sizeof(actual));
        EXPECT_EQ(actual.sin_family, AF_INET);
        EXPECT_EQ(actual.sin_addr.s_addr, destination.sin_addr.s_addr);
        if (peer) {
            EXPECT_EQ(actual.sin_port, destination.sin_port);
        } else {
            EXPECT_NE(actual.sin_port, 0);
        }
        const char zero[sizeof(actual.sin_zero)] {};
        EXPECT_EQ(std::memcmp(actual.sin_zero, zero, sizeof(zero)), 0);
    }
}

TEST(UdpIpv6SendSemantics, NameTruncationReportsFullLengthWithoutOverwriting) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0);
    sockaddr_in6 destination = MakeIpv6Addr("::ffff:127.0.0.1", 12345);
    ASSERT_EQ(connect(fd.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0);
    for (bool peer : {false, true}) {
        SCOPED_TRACE(peer ? "peer" : "local");
        sockaddr_in6 full {};
        socklen_t length = sizeof(full);
        ASSERT_EQ(peer ? getpeername(fd.Get(), reinterpret_cast<sockaddr*>(&full), &length)
                       : getsockname(fd.Get(), reinterpret_cast<sockaddr*>(&full), &length), 0);
        ASSERT_EQ(length, sizeof(full));
        for (socklen_t available : {0U, 1U, 16U, 27U}) {
            SCOPED_TRACE(available);
            unsigned char buffer[sizeof(sockaddr_in6) + 8];
            std::memset(buffer, 0xa5, sizeof(buffer));
            length = available;
            ASSERT_EQ(peer ? getpeername(fd.Get(), reinterpret_cast<sockaddr*>(buffer), &length)
                           : getsockname(fd.Get(), reinterpret_cast<sockaddr*>(buffer), &length), 0);
            EXPECT_EQ(length, sizeof(full));
            EXPECT_EQ(std::memcmp(buffer, &full, available), 0);
            for (size_t i = available; i < sizeof(buffer); ++i) {
                EXPECT_EQ(buffer[i], 0xa5);
            }
        }
    }
}

TEST(UdpIpv6SendSemantics, GetaddrinfoNullNodeDoesNotAbort) {
    // glibc probes both families while sorting results. Isolate any libc abort.
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(10);
        addrinfo hints {};
        hints.ai_family = AF_UNSPEC;
        hints.ai_socktype = SOCK_STREAM;
        addrinfo* result = nullptr;
        const int rc = getaddrinfo(nullptr, "443", &hints, &result);
        if (rc != 0 || result == nullptr) {
            _exit(1);
        }
        freeaddrinfo(result);
        _exit(0);
    }
    int status = 0;
    pid_t waited;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(waited, child);
    ASSERT_TRUE(WIFEXITED(status)) << "child status: " << status;
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(UdpIpv6SendSemantics, UnreachableNativeIpv6DoesNotPanic) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET6, SOCK_DGRAM) failed: " << ErrnoString(errno);

    sockaddr_in6 dst = MakeIpv6Addr("2001:db8::1", 12345);
    errno = 0;
    ssize_t ret = sendto(fd.Get(), "test", 4, 0, reinterpret_cast<sockaddr*>(&dst), sizeof(dst));

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(errno, ENETUNREACH) << "unexpected errno: " << ErrnoString(errno);
}

TEST(UdpIpv6SendSemantics, UnspecifiedIpv6DestinationUsesIpv6Loopback) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET6, SOCK_DGRAM) failed: " << ErrnoString(errno);

    sockaddr_in6 dst = MakeIpv6Addr("::", 12345);
    errno = 0;
    ssize_t ret = sendto(fd.Get(), "test", 4, 0, reinterpret_cast<sockaddr*>(&dst), sizeof(dst));

    EXPECT_EQ(ret, 4) << "sendto(::) failed: " << ErrnoString(errno);
}

TEST(UdpIpv6SendSemantics, ConnectToUnspecifiedIpv6UsesIpv6Loopback) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET6, SOCK_DGRAM) failed: " << ErrnoString(errno);

    sockaddr_in6 dst = MakeIpv6Addr("::", 12345);
    errno = 0;
    int ret = connect(fd.Get(), reinterpret_cast<sockaddr*>(&dst), sizeof(dst));

    EXPECT_EQ(ret, 0) << "connect(::) failed: " << ErrnoString(errno);

    sockaddr_in6 peer {};
    socklen_t peer_len = sizeof(peer);
    ASSERT_EQ(getpeername(fd.Get(), reinterpret_cast<sockaddr*>(&peer), &peer_len), 0)
        << "getpeername failed: " << ErrnoString(errno);
    EXPECT_TRUE(IN6_IS_ADDR_LOOPBACK(&peer.sin6_addr));
    EXPECT_EQ(ntohs(peer.sin6_port), 12345);
}

TEST(UdpIpv6SendSemantics, InvalidFdWithLargePayloadReturnsEbadfBeforeCopy) {
    constexpr size_t kLargeLen = 256UL * 1024 * 1024;
    void* mapping = mmap(nullptr, kLargeLen, PROT_READ, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(mapping, MAP_FAILED) << "mmap failed: " << ErrnoString(errno);

    errno = 0;
    ssize_t ret = sendto(-1, mapping, kLargeLen, 0, nullptr, 0);
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, EBADF) << "unexpected errno: " << ErrnoString(saved_errno);
    EXPECT_EQ(munmap(mapping, kLargeLen), 0) << "munmap failed: " << ErrnoString(errno);
}

TEST(UdpIpv6SendSemantics, OversizeSendtoPortZeroReturnsEinvalWithoutErrqueue) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET6, SOCK_DGRAM) failed: " << ErrnoString(errno);

    int on = 1;
    ASSERT_EQ(setsockopt(fd.Get(), IPPROTO_IPV6, IPV6_RECVERR, &on, sizeof(on)), 0)
        << "setsockopt(IPV6_RECVERR) failed: " << ErrnoString(errno);

    std::vector<char> payload(65536, 'x');
    sockaddr_in6 dst = MakeIpv6Addr("::1", 0);

    errno = 0;
    ssize_t ret = sendto(fd.Get(), payload.data(), payload.size(), 0,
                         reinterpret_cast<sockaddr*>(&dst), sizeof(dst));
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, EINVAL) << "unexpected errno: " << ErrnoString(saved_errno);

    char data[8] {};
    char control[256] {};
    iovec iov {
        .iov_base = data,
        .iov_len = sizeof(data),
    };
    msghdr msg {};
    msg.msg_iov = &iov;
    msg.msg_iovlen = 1;
    msg.msg_control = control;
    msg.msg_controllen = sizeof(control);

    errno = 0;
    ssize_t errq_ret = recvmsg(fd.Get(), &msg, MSG_ERRQUEUE);
    int errq_errno = errno;

    EXPECT_EQ(errq_ret, -1);
    EXPECT_EQ(errq_errno, EAGAIN) << "unexpected errqueue errno: " << ErrnoString(errq_errno);
}

TEST(UdpIpv6SendSemantics, InvalidFdSendtoReturnsEbadfBeforeCheckingPayload) {
    errno = 0;
    ssize_t ret = sendto(-1, reinterpret_cast<void*>(1), 4, 0, nullptr, 0);
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, EBADF) << "unexpected errno: " << ErrnoString(saved_errno);
}

TEST(UdpIpv6SendSemantics, NonSocketSendtoReturnsEnotsockBeforeCheckingPayload) {
    FdGuard fd(open("/dev/null", O_RDONLY));
    ASSERT_GE(fd.Get(), 0) << "open(/dev/null) failed: " << ErrnoString(errno);

    errno = 0;
    ssize_t ret = sendto(fd.Get(), reinterpret_cast<void*>(1), 4, 0, nullptr, 0);
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, ENOTSOCK) << "unexpected errno: " << ErrnoString(saved_errno);
}

TEST(UdpIpv6SendSemantics, SendtoRejectsOutOfRangePayloadBeforeFdLookup) {
    void* out_of_range = reinterpret_cast<void*>(UINTPTR_MAX);

    errno = 0;
    ssize_t ret = sendto(-1, out_of_range, 4, 0, nullptr, 0);
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, EFAULT) << "unexpected errno: " << ErrnoString(saved_errno);
}

TEST(UdpIpv6SendSemantics, SendtoRejectsOutOfRangePayloadBeforeSocketType) {
    FdGuard fd(open("/dev/null", O_RDONLY));
    ASSERT_GE(fd.Get(), 0) << "open(/dev/null) failed: " << ErrnoString(errno);
    void* out_of_range = reinterpret_cast<void*>(UINTPTR_MAX);

    errno = 0;
    ssize_t ret = sendto(fd.Get(), out_of_range, 4, 0, nullptr, 0);
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, EFAULT) << "unexpected errno: " << ErrnoString(saved_errno);
}

TEST(UdpIpv6SendSemantics, InvalidFdSendmsgReturnsEbadfBeforeCopyingMsgHdr) {
    errno = 0;
    ssize_t ret = sendmsg(-1, reinterpret_cast<msghdr*>(1), 0);
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, EBADF) << "unexpected errno: " << ErrnoString(saved_errno);
}

TEST(UdpIpv6SendSemantics, InvalidFdSendmmsgReturnsEbadfBeforeCopyingMsgVec) {
    errno = 0;
    long ret = syscall(SYS_sendmmsg, -1, reinterpret_cast<mmsghdr*>(1), 1U, 0U);
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, EBADF) << "unexpected errno: " << ErrnoString(saved_errno);
}

TEST(UdpIpv6SendSemantics, InvalidFdSendmmsgVlenZeroStillChecksFdFirst) {
    errno = 0;
    long ret = syscall(SYS_sendmmsg, -1, reinterpret_cast<mmsghdr*>(1), 0U, 0U);
    int saved_errno = errno;

    EXPECT_EQ(ret, -1);
    EXPECT_EQ(saved_errno, EBADF) << "unexpected errno: " << ErrnoString(saved_errno);
}

TEST(UdpIpv6SendSemantics, SendmmsgVlenZeroDoesNotCopyMsgVec) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET6, SOCK_DGRAM) failed: " << ErrnoString(errno);

    errno = 0;
    long ret = syscall(SYS_sendmmsg, fd.Get(), reinterpret_cast<mmsghdr*>(1), 0U, 0U);
    int saved_errno = errno;

    EXPECT_EQ(ret, 0);
    EXPECT_EQ(saved_errno, 0) << "unexpected errno: " << ErrnoString(saved_errno);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
