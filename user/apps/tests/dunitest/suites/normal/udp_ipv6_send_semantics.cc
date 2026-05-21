#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <netinet/in.h>
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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
