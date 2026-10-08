#include <gtest/gtest.h>

#include <errno.h>
#include <netinet/icmp6.h>
#include <net/if.h>
#include <poll.h>
#include <sched.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#include <array>

namespace {

class RawIcmp6Socket {
  public:
    RawIcmp6Socket() : fd_(socket(AF_INET6, SOCK_RAW | SOCK_NONBLOCK, IPPROTO_ICMPV6)) {}
    ~RawIcmp6Socket() { if (fd_ >= 0) close(fd_); }
    RawIcmp6Socket(const RawIcmp6Socket&) = delete;
    RawIcmp6Socket& operator=(const RawIcmp6Socket&) = delete;
    int fd() const { return fd_; }

  private:
    int fd_;
};

sockaddr_in6 LoopbackAddress() {
    sockaddr_in6 address {};
    address.sin6_family = AF_INET6;
    address.sin6_addr = in6addr_loopback;
    return address;
}

ssize_t SendType(int fd, uint8_t type) {
    icmp6_hdr packet {};
    packet.icmp6_type = type;
    const sockaddr_in6 address = LoopbackAddress();
    return sendto(fd, &packet, sizeof(packet), 0,
                  reinterpret_cast<const sockaddr*>(&address), sizeof(address));
}

int WaitReadable(int fd) {
    pollfd descriptor {fd, POLLIN, 0};
    int result;
    do {
        result = poll(&descriptor, 1, 1000);
    } while (result < 0 && errno == EINTR);
    return result > 0 && (descriptor.revents & POLLIN) ? 1 : result;
}

void IsolateLoopback() {
    // Send only admits asynchronous output; closing its socket does not cancel
    // committed packets. Isolate every iteration, including the creation-to-
    // setsockopt default-pass window, rather than draining or delaying input.
    ASSERT_EQ(0, unshare(CLONE_NEWNET)) << errno;
    RawIcmp6Socket control;
    ASSERT_GE(control.fd(), 0) << errno;
    ifreq request{};
    request.ifr_name[0] = 'l'; request.ifr_name[1] = 'o';
    ASSERT_EQ(0, ioctl(control.fd(), SIOCGIFFLAGS, &request)) << errno;
    request.ifr_flags |= IFF_UP;
    ASSERT_EQ(0, ioctl(control.fd(), SIOCSIFFLAGS, &request)) << errno;
}

class RawIcmp6Filter : public ::testing::TestWithParam<uint8_t> {};

TEST_P(RawIcmp6Filter, PublicationAndIngressNeverBypassTypeMask) {
    // Recreate sockets to exercise publication while the namespace poller is
    // active. The exact publication race is timing dependent; the assertions
    // cover the public filter contract rather than assuming it always races.
    for (int iteration = 0; iteration < 32; ++iteration) {
        SCOPED_TRACE(iteration);
        ASSERT_NO_FATAL_FAILURE(IsolateLoopback());
        RawIcmp6Socket socket;
        ASSERT_GE(socket.fd(), 0) << errno;
        icmp6_filter filter;
        ICMP6_FILTER_SETBLOCKALL(&filter);
        ICMP6_FILTER_SETPASS(GetParam(), &filter);
        ASSERT_EQ(0, setsockopt(socket.fd(), IPPROTO_ICMPV6, ICMP6_FILTER,
                               &filter, sizeof(filter))) << errno;
        // Include Neighbor Solicitation and the uppermost mask bit. Raw
        // sockets receive unknown types before protocol-specific validation.
        for (uint8_t type : std::array<uint8_t, 3> {0, 135, 255}) {
            ASSERT_EQ(static_cast<ssize_t>(sizeof(icmp6_hdr)), SendType(socket.fd(), type))
                << errno;
        }
        ASSERT_EQ(1, WaitReadable(socket.fd())) << errno;
        icmp6_hdr packet {};
        ASSERT_EQ(static_cast<ssize_t>(sizeof(packet)),
                  recv(socket.fd(), &packet, sizeof(packet), MSG_DONTWAIT)) << errno;
        EXPECT_EQ(GetParam(), packet.icmp6_type);
        EXPECT_EQ(0, packet.icmp6_code);
        EXPECT_NE(0, packet.icmp6_cksum);
        errno = 0;
        EXPECT_EQ(-1, recv(socket.fd(), &packet, sizeof(packet), MSG_DONTWAIT));
        EXPECT_EQ(EAGAIN, errno);
    }
}

INSTANTIATE_TEST_SUITE_P(MaskEdges, RawIcmp6Filter,
                         ::testing::Values(uint8_t {0}, uint8_t {255}));

TEST(RawIcmp6FilterSemantics, ChangingMaskDoesNotRefilterQueuedPackets) {
    RawIcmp6Socket socket;
    ASSERT_GE(socket.fd(), 0) << errno;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(icmp6_hdr)), SendType(socket.fd(), 255)) << errno;
    // POLLIN establishes that this datagram was accepted before changing the
    // mask. Linux applies ICMP6_FILTER at delivery, not when recv dequeues it.
    ASSERT_EQ(1, WaitReadable(socket.fd())) << errno;
    icmp6_filter filter;
    ICMP6_FILTER_SETBLOCKALL(&filter);
    ASSERT_EQ(0, setsockopt(socket.fd(), IPPROTO_ICMPV6, ICMP6_FILTER,
                           &filter, sizeof(filter))) << errno;
    icmp6_hdr packet {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(packet)),
              recv(socket.fd(), &packet, sizeof(packet), MSG_DONTWAIT)) << errno;
    EXPECT_EQ(255, packet.icmp6_type);
    ASSERT_EQ(static_cast<ssize_t>(sizeof(icmp6_hdr)), SendType(socket.fd(), 255)) << errno;
    // A subsequent datagram is rejected by the new mask.
    pollfd descriptor {socket.fd(), POLLIN, 0};
    ASSERT_EQ(0, poll(&descriptor, 1, 50)) << errno;
    errno = 0;
    EXPECT_EQ(-1, recv(socket.fd(), &packet, sizeof(packet), MSG_DONTWAIT));
    EXPECT_EQ(EAGAIN, errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
