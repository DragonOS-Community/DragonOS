#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
#include <fcntl.h>
#include <sys/syscall.h>
#include <sys/socket.h>
#include <sys/mman.h>
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

}  // namespace

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
