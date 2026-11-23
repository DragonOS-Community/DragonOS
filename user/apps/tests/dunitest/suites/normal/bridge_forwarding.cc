// End-to-end tests for the boot-only two-port bridge fixture.

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <linux/if_link.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netpacket/packet.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <chrono>
#include <cstring>

namespace {

constexpr uint16_t kTestProtocol = 0x88b5;

class Socket {
  public:
    Socket() : fd_(socket(AF_PACKET, SOCK_RAW, htons(kTestProtocol))) {}
    ~Socket() {
        if (fd_ >= 0) close(fd_);
    }
    Socket(const Socket&) = delete;
    Socket& operator=(const Socket&) = delete;
    int get() const { return fd_; }

  private:
    int fd_;
};

bool BindToInterface(int fd, unsigned ifindex) {
    sockaddr_ll address{};
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(kTestProtocol);
    address.sll_ifindex = static_cast<int>(ifindex);
    return bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) == 0;
}

bool SendFrame(int fd, unsigned ifindex, const std::array<uint8_t, 6>& destination,
               const std::array<uint8_t, 6>& source, uint8_t marker) {
    std::array<uint8_t, 64> frame{};
    std::memcpy(frame.data(), destination.data(), destination.size());
    std::memcpy(frame.data() + 6, source.data(), source.size());
    frame[12] = static_cast<uint8_t>(kTestProtocol >> 8);
    frame[13] = static_cast<uint8_t>(kTestProtocol);
    frame[14] = marker;
    sockaddr_ll address{};
    address.sll_family = AF_PACKET;
    address.sll_protocol = htons(kTestProtocol);
    address.sll_ifindex = static_cast<int>(ifindex);
    address.sll_halen = 6;
    std::memcpy(address.sll_addr, destination.data(), destination.size());
    return sendto(fd, frame.data(), frame.size(), 0, reinterpret_cast<sockaddr*>(&address),
                  sizeof(address)) == static_cast<ssize_t>(frame.size());
}

bool ReceiveFrame(int fd, uint8_t marker,
                  std::chrono::milliseconds wait = std::chrono::seconds(3)) {
    const auto deadline = std::chrono::steady_clock::now() + wait;
    for (;;) {
        const auto remaining = std::chrono::duration_cast<std::chrono::microseconds>(
            deadline - std::chrono::steady_clock::now());
        if (remaining.count() <= 0) return false;
        const timeval timeout{remaining.count() / 1000000, remaining.count() % 1000000};
        if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) return false;
        std::array<uint8_t, 128> frame{};
        sockaddr_ll source{};
        socklen_t source_len = sizeof(source);
        const ssize_t received = recvfrom(fd, frame.data(), frame.size(), 0,
                                          reinterpret_cast<sockaddr*>(&source), &source_len);
        if (received < 0 && errno == EINTR) continue;
        if (received < 0) return false;
        if (received >= 15 && source.sll_pkttype != PACKET_OUTGOING &&
            frame[12] == (kTestProtocol >> 8) &&
            frame[13] == static_cast<uint8_t>(kTestProtocol) && frame[14] == marker)
            return true;
    }
}

int SetHairpin(unsigned port, bool enabled) {
    struct {
        nlmsghdr header;
        ifinfomsg link;
        rtattr protinfo;
        rtattr mode;
        uint8_t value;
    } request{};
    request.header.nlmsg_len = sizeof(request);
    request.header.nlmsg_type = RTM_SETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    request.link.ifi_family = AF_BRIDGE;
    request.link.ifi_index = port;
    request.protinfo.rta_type = IFLA_PROTINFO | NLA_F_NESTED;
    request.protinfo.rta_len = RTA_LENGTH(RTA_SPACE(1));
    request.mode.rta_type = IFLA_BRPORT_MODE;
    request.mode.rta_len = RTA_LENGTH(1);
    request.value = enabled;

    const int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) return errno;
    const timeval timeout{2, 0};
    std::array<uint8_t, 256> reply{};
    int result;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0 ||
        send(fd, &request, sizeof(request), 0) < 0) {
        result = errno;
    } else {
        const ssize_t bytes = recv(fd, reply.data(), reply.size(), 0);
        const auto* header = reinterpret_cast<const nlmsghdr*>(reply.data());
        if (bytes < 0) {
            result = errno;
        } else if (!NLMSG_OK(header, bytes) || header->nlmsg_type != NLMSG_ERROR ||
                   header->nlmsg_len < NLMSG_LENGTH(sizeof(nlmsgerr))) {
            result = EPROTO;
        } else {
            result = -static_cast<const nlmsgerr*>(NLMSG_DATA(header))->error;
        }
    }
    close(fd);
    return result;
}

TEST(BridgeForwarding, BroadcastAndUnicastReachThePeer) {
    // veth_a--veth_b(bridge0)veth_c--veth_d is created only by the
    // dragonos.net_test_fixtures boot option.
    const unsigned a = if_nametoindex("veth_a");
    const unsigned d = if_nametoindex("veth_d");
    ASSERT_NE(a, 0u);
    ASSERT_NE(d, 0u);
    Socket sender;
    Socket on_d;
    ASSERT_GE(sender.get(), 0) << std::strerror(errno);
    ASSERT_GE(on_d.get(), 0) << std::strerror(errno);
    ASSERT_TRUE(BindToInterface(on_d.get(), d)) << std::strerror(errno);

    constexpr std::array<uint8_t, 6> broadcast{0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    constexpr std::array<uint8_t, 6> source_a{0x02, 0x35, 0x00, 0x00, 0x00, 0x01};
    constexpr std::array<uint8_t, 6> source_d{0x02, 0x35, 0x00, 0x00, 0x00, 0x02};
    ASSERT_TRUE(SendFrame(sender.get(), a, broadcast, source_a, 0xa1)) << std::strerror(errno);
    ASSERT_TRUE(ReceiveFrame(on_d.get(), 0xa1));
    Socket on_a;
    ASSERT_GE(on_a.get(), 0) << std::strerror(errno);
    ASSERT_TRUE(BindToInterface(on_a.get(), a)) << std::strerror(errno);
    ASSERT_TRUE(SendFrame(sender.get(), d, source_a, source_d, 0xd1)) << std::strerror(errno);
    ASSERT_TRUE(ReceiveFrame(on_a.get(), 0xd1));
}

TEST(BridgeForwarding, HairpinReflectsOnlyWhenEnabled) {
    const unsigned a = if_nametoindex("veth_a");
    const unsigned b = if_nametoindex("veth_b");
    const unsigned d = if_nametoindex("veth_d");
    ASSERT_NE(a, 0u);
    ASSERT_NE(b, 0u);
    ASSERT_NE(d, 0u);
    Socket sender;
    Socket on_a;
    Socket on_d;
    ASSERT_GE(sender.get(), 0);
    ASSERT_GE(on_a.get(), 0);
    ASSERT_GE(on_d.get(), 0);
    ASSERT_TRUE(BindToInterface(on_a.get(), a));
    ASSERT_TRUE(BindToInterface(on_d.get(), d));

    constexpr std::array<uint8_t, 6> broadcast{0xff, 0xff, 0xff, 0xff, 0xff, 0xff};
    constexpr std::array<uint8_t, 6> learned{0x02, 0x36, 0x00, 0x00, 0x00, 0x01};
    constexpr std::array<uint8_t, 6> sender_mac{0x02, 0x36, 0x00, 0x00, 0x00, 0x02};
    constexpr std::chrono::milliseconds quiet{250};

    // The first broadcast learns a non-local source on port b.
    ASSERT_TRUE(SendFrame(sender.get(), a, broadcast, learned, 0xa2));
    ASSERT_TRUE(ReceiveFrame(on_d.get(), 0xa2));
    EXPECT_FALSE(ReceiveFrame(on_a.get(), 0xa2, quiet));
    ASSERT_TRUE(SendFrame(sender.get(), a, learned, sender_mac, 0xa3));
    EXPECT_FALSE(ReceiveFrame(on_a.get(), 0xa3, quiet));

    ASSERT_EQ(SetHairpin(b, true), 0);
    EXPECT_TRUE(SendFrame(sender.get(), a, learned, sender_mac, 0xa4));
    EXPECT_TRUE(ReceiveFrame(on_a.get(), 0xa4));
    EXPECT_FALSE(ReceiveFrame(on_d.get(), 0xa4, quiet));
    EXPECT_TRUE(SendFrame(sender.get(), a, broadcast, sender_mac, 0xa5));
    EXPECT_TRUE(ReceiveFrame(on_a.get(), 0xa5));
    EXPECT_TRUE(ReceiveFrame(on_d.get(), 0xa5));
    EXPECT_FALSE(ReceiveFrame(on_a.get(), 0xa5, quiet));
    ASSERT_EQ(SetHairpin(b, false), 0);
    ASSERT_TRUE(SendFrame(sender.get(), a, learned, sender_mac, 0xa6));
    EXPECT_FALSE(ReceiveFrame(on_a.get(), 0xa6, quiet));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
