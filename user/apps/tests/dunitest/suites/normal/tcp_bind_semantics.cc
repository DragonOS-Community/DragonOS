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

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

sockaddr_in LoopbackAddr(uint16_t port) {
    sockaddr_in addr {};
    addr.sin_family = AF_INET;
    addr.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    addr.sin_port = htons(port);
    return addr;
}

uint16_t BoundPort(int fd) {
    sockaddr_in addr {};
    socklen_t len = sizeof(addr);
    EXPECT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(&addr), &len), 0)
            << "getsockname failed: " << ErrnoString(errno);
    EXPECT_EQ(len, sizeof(addr));
    return ntohs(addr.sin_port);
}

}  // namespace

TEST(TcpBindSemantics, RepeatedBindReturnsEinvalAndPreservesSocket) {
    FdGuard fd(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET, SOCK_STREAM) failed: " << ErrnoString(errno);

    sockaddr_in addr = LoopbackAddr(0);
    ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0)
            << "first bind failed: " << ErrnoString(errno);
    const uint16_t first_port = BoundPort(fd.Get());
    ASSERT_NE(first_port, 0);

    errno = 0;
    EXPECT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), -1);
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(BoundPort(fd.Get()), first_port);

    EXPECT_EQ(listen(fd.Get(), 1), 0) << "listen after failed rebind failed: "
                                     << ErrnoString(errno);
}

TEST(TcpBindSemantics, PortConflictDoesNotPoisonSocket) {
    FdGuard first(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(first.Get(), 0) << "socket(first) failed: " << ErrnoString(errno);

    sockaddr_in addr = LoopbackAddr(0);
    ASSERT_EQ(bind(first.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0)
            << "bind(first) failed: " << ErrnoString(errno);
    const uint16_t occupied_port = BoundPort(first.Get());
    ASSERT_NE(occupied_port, 0);

    FdGuard second(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(second.Get(), 0) << "socket(second) failed: " << ErrnoString(errno);

    sockaddr_in occupied = LoopbackAddr(occupied_port);
    errno = 0;
    EXPECT_EQ(bind(second.Get(), reinterpret_cast<sockaddr*>(&occupied), sizeof(occupied)), -1);
    EXPECT_EQ(errno, EADDRINUSE);

    sockaddr_in ephemeral = LoopbackAddr(0);
    ASSERT_EQ(bind(second.Get(), reinterpret_cast<sockaddr*>(&ephemeral), sizeof(ephemeral)), 0)
            << "bind(second, ephemeral) after conflict failed: " << ErrnoString(errno);
    ASSERT_NE(BoundPort(second.Get()), 0);
    EXPECT_EQ(listen(second.Get(), 1), 0) << "listen(second) failed: " << ErrnoString(errno);
}

TEST(TcpBindSemantics, NonlocalAddressFailureDoesNotPoisonSocket) {
    FdGuard fd(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(fd.Get(), 0) << "socket(AF_INET, SOCK_STREAM) failed: " << ErrnoString(errno);

    sockaddr_in nonlocal = LoopbackAddr(0);
    ASSERT_EQ(inet_pton(AF_INET, "203.0.113.1", &nonlocal.sin_addr), 1);

    errno = 0;
    EXPECT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&nonlocal), sizeof(nonlocal)), -1);
    EXPECT_EQ(errno, EADDRNOTAVAIL);

    sockaddr_in loopback = LoopbackAddr(0);
    ASSERT_EQ(bind(fd.Get(), reinterpret_cast<sockaddr*>(&loopback), sizeof(loopback)), 0)
            << "bind(loopback) after nonlocal failure failed: " << ErrnoString(errno);
    ASSERT_NE(BoundPort(fd.Get()), 0);
    EXPECT_EQ(listen(fd.Get(), 1), 0) << "listen after nonlocal failure failed: "
                                     << ErrnoString(errno);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
