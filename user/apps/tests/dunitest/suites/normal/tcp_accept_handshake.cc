// Packet-level handshake tests use the same veth1/veth2 fixture as rtnetlink tests.
// The peer address is deliberately not assigned: no host TCP stack can complete
// the handshake or reset the SYN-ACK on behalf of this test.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <linux/if_ether.h>
#include <linux/if_packet.h>
#include <net/if.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#include <array>
#include <chrono>
#include <cstring>

namespace {
class Fd {
 public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { if (value_ >= 0) close(value_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return value_; }
    void reset(int value) { if (value_ >= 0) close(value_); value_ = value; }
 private:
    int value_;
};

void Put16(unsigned char* p, uint16_t value) {
    p[0] = value >> 8;
    p[1] = value;
}
void Put32(unsigned char* p, uint32_t value) {
    Put16(p, value >> 16);
    Put16(p + 2, value);
}
uint32_t Get32(const unsigned char* p) {
    return (uint32_t(p[0]) << 24) | (uint32_t(p[1]) << 16) |
           (uint32_t(p[2]) << 8) | p[3];
}
uint16_t Checksum(const unsigned char* p, size_t size, uint32_t sum = 0) {
    for (size_t i = 0; i < size; i += 2)
        sum += (uint16_t(p[i]) << 8) | (i + 1 < size ? p[i + 1] : 0);
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return static_cast<uint16_t>(~sum);
}

class TcpAcceptHandshake : public testing::Test {
 protected:
    static constexpr uint32_t kPeer = 0x6f6f0bfe;  // 111.111.11.254
    static constexpr uint32_t kServer = 0x6f6f0b02;
    static constexpr uint16_t kPeerPort = 43017;
    static constexpr uint32_t kInitialSequence = 1234567;
    Fd packet_, listener_;
    int interface_ = 0;
    uint16_t port_ = 0;
    uint32_t server_sequence_ = 0;
    std::array<unsigned char, 6> source_mac_{}, destination_mac_{};

    void SetUp() override {
        packet_.reset(socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK, htons(ETH_P_ALL)));
        ASSERT_GE(packet_.get(), 0) << strerror(errno);
        interface_ = if_nametoindex("veth1");
        ASSERT_NE(interface_, 0) << "requires the standard veth1/veth2 network fixture";
        ifreq request{};
        strcpy(request.ifr_name, "veth1");
        ASSERT_EQ(ioctl(packet_.get(), SIOCGIFHWADDR, &request), 0);
        memcpy(source_mac_.data(), request.ifr_hwaddr.sa_data, 6);
        strcpy(request.ifr_name, "veth2");
        ASSERT_EQ(ioctl(packet_.get(), SIOCGIFHWADDR, &request), 0);
        memcpy(destination_mac_.data(), request.ifr_hwaddr.sa_data, 6);
        sockaddr_ll address{};
        address.sll_family = AF_PACKET;
        address.sll_protocol = htons(ETH_P_ALL);
        address.sll_ifindex = interface_;
        ASSERT_EQ(bind(packet_.get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0);
        listener_.reset(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
        ASSERT_GE(listener_.get(), 0);
        // Both fixture interfaces share a subnet. Pin replies to the ingress
        // side rather than depending on the host's equal-prefix route order.
        constexpr char device[] = "veth2";
        ASSERT_EQ(setsockopt(listener_.get(), SOL_SOCKET, SO_BINDTODEVICE,
                             device, sizeof(device)), 0);
        sockaddr_in local{};
        local.sin_family = AF_INET;
        local.sin_addr.s_addr = htonl(kServer);
        ASSERT_EQ(bind(listener_.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
        socklen_t length = sizeof(local);
        ASSERT_EQ(getsockname(listener_.get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
        port_ = ntohs(local.sin_port);
        ASSERT_EQ(listen(listener_.get(), 8), 0);
        // A valid ARP request teaches the receiving interface how to reply to
        // our synthetic peer without modifying routes or neighbor tables.
        std::array<unsigned char, 42> arp{};
        Ethernet(arp.data(), ETH_P_ARP);
        auto* a = arp.data() + 14;
        Put16(a, 1); Put16(a + 2, ETH_P_IP);
        a[4] = 6; a[5] = 4; Put16(a + 6, 1);
        memcpy(a + 8, source_mac_.data(), 6);
        Put32(a + 14, kPeer); Put32(a + 24, kServer);
        ASSERT_NO_FATAL_FAILURE(Send(arp.data(), arp.size()));
    }

    void Ethernet(unsigned char* p, uint16_t protocol) {
        memcpy(p, destination_mac_.data(), 6);
        memcpy(p + 6, source_mac_.data(), 6);
        Put16(p + 12, protocol);
    }
    void Send(const unsigned char* p, size_t size) {
        sockaddr_ll address{};
        address.sll_family = AF_PACKET;
        address.sll_ifindex = interface_;
        address.sll_halen = 6;
        memcpy(address.sll_addr, destination_mac_.data(), 6);
        ASSERT_EQ(sendto(packet_.get(), p, size, 0,
                         reinterpret_cast<sockaddr*>(&address), sizeof(address)),
                  static_cast<ssize_t>(size)) << strerror(errno);
    }
    void Segment(uint8_t flags, uint32_t sequence, uint32_t acknowledgement) {
        std::array<unsigned char, 54> frame{};
        Ethernet(frame.data(), ETH_P_IP);
        auto* ip = frame.data() + 14;
        ip[0] = 0x45; Put16(ip + 2, 40); ip[8] = 64; ip[9] = IPPROTO_TCP;
        Put32(ip + 12, kPeer); Put32(ip + 16, kServer);
        Put16(ip + 10, Checksum(ip, 20));
        auto* tcp = ip + 20;
        Put16(tcp, kPeerPort); Put16(tcp + 2, port_);
        Put32(tcp + 4, sequence); Put32(tcp + 8, acknowledgement);
        tcp[12] = 0x50; tcp[13] = flags; Put16(tcp + 14, 65535);
        const uint32_t pseudo = (kPeer >> 16) + (kPeer & 0xffff) +
                                (kServer >> 16) + (kServer & 0xffff) + IPPROTO_TCP + 20;
        Put16(tcp + 16, Checksum(tcp, 20, pseudo));
        Send(frame.data(), frame.size());
    }
    void WaitReply(uint8_t required_flags, uint32_t acknowledgement = 0) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
        while (std::chrono::steady_clock::now() < deadline) {
            pollfd event{packet_.get(), POLLIN, 0};
            ASSERT_GE(poll(&event, 1, 100), 0);
            unsigned char packet[2048];
            ssize_t size = recv(packet_.get(), packet, sizeof(packet), 0);
            if (size < 54 || packet[12] != 8 || packet[13] != 0) continue;
            auto* ip = packet + 14;
            const size_t header = (ip[0] & 15) * 4;
            if (header < 20 || size < static_cast<ssize_t>(14 + header + 20) ||
                ip[9] != IPPROTO_TCP || Get32(ip + 12) != kServer ||
                Get32(ip + 16) != kPeer) continue;
            auto* tcp = ip + header;
            if ((uint16_t(tcp[0]) << 8 | tcp[1]) != port_ ||
                (uint16_t(tcp[2]) << 8 | tcp[3]) != kPeerPort ||
                (tcp[13] & required_flags) != required_flags ||
                (acknowledgement != 0 && Get32(tcp + 8) != acknowledgement)) continue;
            server_sequence_ = Get32(tcp + 4);
            return;
        }
        FAIL() << "did not observe TCP reply flags=" << unsigned(required_flags);
    }
    void StartHandshake() {
        ASSERT_NO_FATAL_FAILURE(Segment(0x02, kInitialSequence, 0));
        ASSERT_NO_FATAL_FAILURE(WaitReply(0x12));
    }
};

TEST_F(TcpAcceptHandshake, SynReceivedIsNotReadableOrAcceptable) {
    ASSERT_NO_FATAL_FAILURE(StartHandshake());
    pollfd event{listener_.get(), POLLIN, 0};
    EXPECT_EQ(poll(&event, 1, 100), 0);
    Fd accepted(accept4(listener_.get(), nullptr, nullptr, SOCK_NONBLOCK));
    EXPECT_EQ(accepted.get(), -1);
    if (accepted.get() < 0) {
        EXPECT_EQ(errno, EAGAIN);
    }
    ASSERT_NO_FATAL_FAILURE(Segment(0x14, kInitialSequence + 1, server_sequence_ + 1));
}

TEST_F(TcpAcceptHandshake, FinalAckMakesConnectionAcceptable) {
    ASSERT_NO_FATAL_FAILURE(StartHandshake());
    ASSERT_NO_FATAL_FAILURE(Segment(0x10, kInitialSequence + 1, server_sequence_ + 1));
    pollfd event{listener_.get(), POLLIN, 0};
    ASSERT_EQ(poll(&event, 1, 3000), 1);
    Fd accepted(accept4(listener_.get(), nullptr, nullptr, SOCK_NONBLOCK));
    ASSERT_GE(accepted.get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Segment(0x14, kInitialSequence + 1, server_sequence_ + 1));
}

TEST_F(TcpAcceptHandshake, FinalAckWithFinRemainsAcceptable) {
    ASSERT_NO_FATAL_FAILURE(StartHandshake());
    ASSERT_NO_FATAL_FAILURE(Segment(0x11, kInitialSequence + 1, server_sequence_ + 1));
    // Observe the FIN acknowledgement before accepting, so this exercises
    // CLOSE_WAIT rather than relying on a race with final handshake processing.
    ASSERT_NO_FATAL_FAILURE(WaitReply(0x10, kInitialSequence + 2));
    pollfd event{listener_.get(), POLLIN, 0};
    ASSERT_EQ(poll(&event, 1, 3000), 1);
    Fd accepted(accept4(listener_.get(), nullptr, nullptr, SOCK_NONBLOCK));
    ASSERT_GE(accepted.get(), 0) << strerror(errno);
    char byte;
    EXPECT_EQ(recv(accepted.get(), &byte, 1, 0), 0);
    ASSERT_NO_FATAL_FAILURE(Segment(0x14, kInitialSequence + 2, server_sequence_ + 1));
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
