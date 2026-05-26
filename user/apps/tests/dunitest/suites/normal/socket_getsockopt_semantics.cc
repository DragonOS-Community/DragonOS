#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <net/if.h>
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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
