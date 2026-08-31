#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstring>
#include <string>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    ~FdGuard() {
        if (fd_ >= 0) close(fd_);
    }
    int Get() const { return fd_; }

  private:
    int fd_;
};

void BindToLoopback(int fd) {
    const char name[] = "lo";
    ASSERT_EQ(setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, name, sizeof(name)), 0)
            << strerror(errno);
}

std::string FindNonLoopbackInterface() {
    std::string name;
    struct if_nameindex* interfaces = if_nameindex();
    if (interfaces == nullptr) return name;
    for (struct if_nameindex* current = interfaces; current->if_index != 0; ++current) {
        if (strcmp(current->if_name, "lo") != 0) {
            name = current->if_name;
            break;
        }
    }
    if_freenameindex(interfaces);
    return name;
}

}  // namespace

TEST(UdpBindToDevice, UnboundGetsockoptHasZeroLengthAndDoesNotWrite) {
    FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0);

    char value[IFNAMSIZ];
    memset(value, 0x5a, sizeof(value));
    socklen_t len = sizeof(value);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, value, &len), 0)
            << strerror(errno);
    EXPECT_EQ(len, 0u);
    for (char byte : value) EXPECT_EQ(static_cast<unsigned char>(byte), 0x5a);
}

TEST(UdpBindToDevice, SetGetInvalidAndClearFollowLinuxAbi) {
    FdGuard fd(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(fd.Get(), 0);

    const char missing[] = "dunit-no-such-iface";
    errno = 0;
    EXPECT_EQ(setsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, missing, sizeof(missing)), -1);
    EXPECT_EQ(errno, ENODEV);

    BindToLoopback(fd.Get());

    char small[IFNAMSIZ - 1] = {};
    socklen_t small_len = sizeof(small);
    errno = 0;
    EXPECT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, small, &small_len), -1);
    EXPECT_EQ(errno, EINVAL);

    char value[IFNAMSIZ] = {};
    socklen_t len = sizeof(value);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, value, &len), 0);
    EXPECT_STREQ(value, "lo");
    EXPECT_EQ(len, 3u);

    const char clear[] = "";
    ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, clear, sizeof(clear)), 0);
    len = sizeof(value);
    ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, value, &len), 0);
    EXPECT_EQ(len, 0u);
}

TEST(UdpBindToDevice, LoopbackDatagramUsesBoundInterface) {
    FdGuard receiver(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard sender(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.Get(), 0);
    ASSERT_GE(sender.Get(), 0);
    BindToLoopback(receiver.Get());
    BindToLoopback(sender.Get());

    timeval timeout = {.tv_sec = 1, .tv_usec = 0};
    ASSERT_EQ(setsockopt(receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);

    sockaddr_in address = {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << strerror(errno);
    socklen_t address_len = sizeof(address);
    ASSERT_EQ(getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&address), &address_len), 0);

    const char payload[] = "bound-lo";
    ASSERT_EQ(sendto(sender.Get(), payload, sizeof(payload), 0,
                     reinterpret_cast<sockaddr*>(&address), sizeof(address)),
              static_cast<ssize_t>(sizeof(payload)))
            << strerror(errno);
    char received[sizeof(payload)] = {};
    ASSERT_EQ(recv(receiver.Get(), received, sizeof(received), 0),
              static_cast<ssize_t>(sizeof(payload)))
            << strerror(errno);
    EXPECT_EQ(memcmp(received, payload, sizeof(payload)), 0);
}

TEST(UdpBindToDevice, WildcardLocalSendSelectsInterfaceSourceAddress) {
    FdGuard receiver(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard sender(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.Get(), 0);
    ASSERT_GE(sender.Get(), 0);

    sockaddr_in receiver_address = {};
    receiver_address.sin_family = AF_INET;
    receiver_address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&receiver_address),
                   sizeof(receiver_address)),
              0)
            << strerror(errno);
    socklen_t receiver_address_len = sizeof(receiver_address);
    ASSERT_EQ(getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&receiver_address),
                          &receiver_address_len),
              0);

    sockaddr_in wildcard = {};
    wildcard.sin_family = AF_INET;
    wildcard.sin_addr.s_addr = htonl(INADDR_ANY);
    ASSERT_EQ(bind(sender.Get(), reinterpret_cast<sockaddr*>(&wildcard), sizeof(wildcard)), 0)
            << strerror(errno);

    const char payload[] = "wildcard-source";
    ASSERT_EQ(sendto(sender.Get(), payload, sizeof(payload), 0,
                     reinterpret_cast<sockaddr*>(&receiver_address), sizeof(receiver_address)),
              static_cast<ssize_t>(sizeof(payload)))
            << strerror(errno);

    char received[sizeof(payload)] = {};
    sockaddr_in peer = {};
    socklen_t peer_len = sizeof(peer);
    ASSERT_EQ(recvfrom(receiver.Get(), received, sizeof(received), 0,
                       reinterpret_cast<sockaddr*>(&peer), &peer_len),
              static_cast<ssize_t>(sizeof(payload)))
            << strerror(errno);
    EXPECT_EQ(peer.sin_addr.s_addr, htonl(INADDR_LOOPBACK));
    EXPECT_EQ(memcmp(received, payload, sizeof(payload)), 0);
}

TEST(UdpBindToDevice, PortConflictsIncludeTheBoundDeviceDimension) {
    const std::string non_loopback = FindNonLoopbackInterface();
    if (non_loopback.empty()) GTEST_SKIP() << "no non-loopback interface";

    FdGuard loopback(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard physical(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard wildcard(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(loopback.Get(), 0);
    ASSERT_GE(physical.Get(), 0);
    ASSERT_GE(wildcard.Get(), 0);
    BindToLoopback(loopback.Get());
    ASSERT_EQ(setsockopt(physical.Get(), SOL_SOCKET, SO_BINDTODEVICE, non_loopback.c_str(),
                         non_loopback.size() + 1),
              0)
            << strerror(errno);

    sockaddr_in address = {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_ANY);
    ASSERT_EQ(bind(loopback.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
    socklen_t address_len = sizeof(address);
    ASSERT_EQ(getsockname(loopback.Get(), reinterpret_cast<sockaddr*>(&address), &address_len), 0);

    ASSERT_EQ(bind(physical.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << "different nonzero bound devices may share a UDP port: " << strerror(errno);
    errno = 0;
    EXPECT_EQ(bind(wildcard.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
    EXPECT_EQ(errno, EADDRINUSE);
}

TEST(UdpBindToDevice, LocalFastPathDoesNotCrossBoundInterface) {
    const std::string non_loopback = FindNonLoopbackInterface();
    if (non_loopback.empty()) GTEST_SKIP() << "no non-loopback interface";

    FdGuard receiver(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard sender(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.Get(), 0);
    ASSERT_GE(sender.Get(), 0);

    timeval timeout = {.tv_sec = 0, .tv_usec = 100000};
    ASSERT_EQ(setsockopt(receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);

    ASSERT_EQ(setsockopt(sender.Get(), SOL_SOCKET, SO_BINDTODEVICE, non_loopback.c_str(),
                         non_loopback.size() + 1),
              0)
            << strerror(errno);

    sockaddr_in address = {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << strerror(errno);
    socklen_t address_len = sizeof(address);
    ASSERT_EQ(getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&address), &address_len), 0);

    const char payload[] = "wrong-interface";
    ASSERT_EQ(sendto(sender.Get(), payload, sizeof(payload), 0,
                     reinterpret_cast<sockaddr*>(&address), sizeof(address)),
              static_cast<ssize_t>(sizeof(payload)))
            << strerror(errno);

    char received[sizeof(payload)] = {};
    errno = 0;
    EXPECT_EQ(recv(receiver.Get(), received, sizeof(received), 0), -1);
    EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << strerror(errno);

    ASSERT_EQ(sendto(sender.Get(), payload, sizeof(payload), 0,
                     reinterpret_cast<sockaddr*>(&address), sizeof(address)),
              static_cast<ssize_t>(sizeof(payload)))
            << "the bound interface must remain usable after processing ingress: "
            << strerror(errno);
}

TEST(UdpBindToDevice, ReuseOptionsAreReadAtBindConflictTime) {
    FdGuard first(socket(AF_INET, SOCK_DGRAM, 0));
    FdGuard second(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(first.Get(), 0);
    ASSERT_GE(second.Get(), 0);

    sockaddr_in address = {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    ASSERT_EQ(bind(first.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
    socklen_t address_len = sizeof(address);
    ASSERT_EQ(getsockname(first.Get(), reinterpret_cast<sockaddr*>(&address), &address_len), 0);

    int one = 1;
    ASSERT_EQ(setsockopt(first.Get(), SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)), 0);
    ASSERT_EQ(setsockopt(second.Get(), SOL_SOCKET, SO_REUSEADDR, &one, sizeof(one)), 0);
    EXPECT_EQ(bind(second.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << strerror(errno);
}

TEST(UdpBindToDevice, RebindingAnExistingBindingRequiresCapability) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        FdGuard fd(socket(AF_INET, SOCK_DGRAM, 0));
        if (fd.Get() < 0 || setuid(65534) != 0) _exit(10);
        const char name[] = "lo";
        if (setsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, sizeof(name)) != 0) _exit(11);
        errno = 0;
        if (setsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, sizeof(name)) != -1 ||
            errno != EPERM) {
            _exit(12);
        }
        _exit(0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
