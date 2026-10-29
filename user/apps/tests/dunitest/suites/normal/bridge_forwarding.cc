// End-to-end smoke test for the boot-only two-port bridge fixture. Lock
// ordering and FDB-hit behavior require separate tests or code review.

#include <gtest/gtest.h>

#include <arpa/inet.h>
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

bool ReceiveFrame(int fd, uint8_t marker) {
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
    for (;;) {
        const auto remaining = std::chrono::duration_cast<std::chrono::microseconds>(
            deadline - std::chrono::steady_clock::now());
        if (remaining.count() <= 0) return false;
        const timeval timeout{remaining.count() / 1000000, remaining.count() % 1000000};
        if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) return false;
        std::array<uint8_t, 128> frame{};
        const ssize_t received = recv(fd, frame.data(), frame.size(), 0);
        if (received < 0 && errno == EINTR) continue;
        if (received < 0) return false;
        if (received >= 15 && frame[12] == (kTestProtocol >> 8) &&
            frame[13] == static_cast<uint8_t>(kTestProtocol) && frame[14] == marker)
            return true;
    }
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
    // Bind the reverse receiver only now: otherwise it may first read the
    // sender's own PACKET_OUTGOING copy of the broadcast from veth_a.
    Socket on_a;
    ASSERT_GE(on_a.get(), 0) << std::strerror(errno);
    ASSERT_TRUE(BindToInterface(on_a.get(), a)) << std::strerror(errno);
    ASSERT_TRUE(SendFrame(sender.get(), d, source_a, source_d, 0xd1)) << std::strerror(errno);
    ASSERT_TRUE(ReceiveFrame(on_a.get(), 0xd1));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
