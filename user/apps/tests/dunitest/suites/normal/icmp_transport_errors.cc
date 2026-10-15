#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <linux/errqueue.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

#include <array>

namespace {
class SocketFd {
 public:
    explicit SocketFd(int fd) : fd_(fd) {}
    ~SocketFd() { if (fd_ >= 0) close(fd_); }
    int get() const { return fd_; }
    SocketFd(const SocketFd&) = delete;
    SocketFd& operator=(const SocketFd&) = delete;
 private:
    int fd_;
};

class IcmpTransportErrors : public testing::TestWithParam<int> {};

sockaddr_storage Loopback(int family, uint16_t port) {
    sockaddr_storage result{};
    if (family == AF_INET) {
        auto* address = reinterpret_cast<sockaddr_in*>(&result);
        address->sin_family = AF_INET;
        address->sin_port = port;
        address->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
    } else {
        auto* address = reinterpret_cast<sockaddr_in6*>(&result);
        address->sin6_family = AF_INET6;
        address->sin6_port = port;
        address->sin6_addr = in6addr_loopback;
    }
    return result;
}

TEST_P(IcmpTransportErrors, ClosedUdpPortReportsExtendedError) {
    const int family = GetParam();
    const socklen_t length = family == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6);
    SocketFd sender(socket(family, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(sender.get(), 0) << errno;
    auto destination = Loopback(family, 0);
    // Reserve a port, then release it: no fixed host port or external network.
    int reservation = socket(family, SOCK_DGRAM, 0);
    ASSERT_GE(reservation, 0) << errno;
    ASSERT_EQ(bind(reservation, reinterpret_cast<sockaddr*>(&destination), length), 0) << errno;
    socklen_t actual = sizeof(destination);
    ASSERT_EQ(getsockname(reservation, reinterpret_cast<sockaddr*>(&destination), &actual), 0);
    ASSERT_EQ(close(reservation), 0);
    const int enabled = 1;
    ASSERT_EQ(setsockopt(sender.get(), family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6,
                        family == AF_INET ? IP_RECVERR : IPV6_RECVERR,
                        &enabled, sizeof(enabled)), 0) << errno;
    constexpr std::array<char, 4> payload{'D', 'K', '9', '3'};
    ASSERT_EQ(sendto(sender.get(), payload.data(), payload.size(), 0,
                     reinterpret_cast<sockaddr*>(&destination), length), static_cast<ssize_t>(payload.size())) << errno;
    pollfd ready{sender.get(), POLLERR, 0};
    ASSERT_EQ(poll(&ready, 1, 1000), 1) << errno;
    ASSERT_NE(ready.revents & POLLERR, 0);
    std::array<char, 16> bytes{};
    alignas(cmsghdr) std::array<unsigned char, 256> control{};
    sockaddr_storage peer{};
    iovec vector{bytes.data(), bytes.size()};
    msghdr message{};
    message.msg_name = &peer; message.msg_namelen = sizeof(peer);
    message.msg_iov = &vector; message.msg_iovlen = 1;
    message.msg_control = control.data(); message.msg_controllen = control.size();
    ASSERT_EQ(recvmsg(sender.get(), &message, MSG_ERRQUEUE | MSG_DONTWAIT), static_cast<ssize_t>(payload.size())) << errno;
    EXPECT_EQ(memcmp(bytes.data(), payload.data(), payload.size()), 0);
    bool found = false;
    for (auto* header = CMSG_FIRSTHDR(&message); header; header = CMSG_NXTHDR(&message, header)) {
        if (header->cmsg_level != (family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6) ||
            header->cmsg_type != (family == AF_INET ? IP_RECVERR : IPV6_RECVERR)) continue;
        ASSERT_GE(header->cmsg_len, CMSG_LEN(sizeof(sock_extended_err)));
        auto* error = reinterpret_cast<const sock_extended_err*>(CMSG_DATA(header));
        EXPECT_EQ(error->ee_errno, static_cast<unsigned>(ECONNREFUSED));
        EXPECT_EQ(error->ee_origin, family == AF_INET ? SO_EE_ORIGIN_ICMP : SO_EE_ORIGIN_ICMP6);
        EXPECT_EQ(error->ee_type, family == AF_INET ? 3 : 1);
        EXPECT_EQ(error->ee_code, family == AF_INET ? 3 : 4);
        EXPECT_EQ(error->ee_info, 0U);
        found = true;
    }
    EXPECT_TRUE(found);
}

INSTANTIATE_TEST_SUITE_P(DualStack, IcmpTransportErrors, testing::Values(AF_INET, AF_INET6));
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
