// A synthetic peer observes physical ingress on veth1 for traffic emitted on
// veth2. The TCP source belongs to veth1: socket owner and egress must differ.
// No peer IP is assigned to the host, so the host cannot fake the handshake.
// Linux reference runs must disable veth TX checksum/GSO/GRO offloads first:
// ethtool -K veth{1,2} tx off rx off tso off gso off gro off (each device).
// Otherwise AF_PACKET exposes unfinished checksums and aggregated segments.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <linux/if_addr.h>
#include <linux/if_packet.h>
#include <linux/neighbour.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/ethernet.h>
#include <net/if.h>
#include <poll.h>
#include <sys/ioctl.h>
#include <sys/socket.h>
#include <unistd.h>

#include <array>
#include <chrono>
#include <cstring>
#include <vector>

namespace {
class Fd {
 public:
    explicit Fd(int value = -1) : value_(value) {}
    ~Fd() { reset(-1); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return value_; }
    void reset(int value) { if (value_ >= 0) close(value_); value_ = value; }
 private:
    int value_;
};

uint16_t Get16(const unsigned char* p) { return (uint16_t(p[0]) << 8) | p[1]; }
uint32_t Get32(const unsigned char* p) { return (uint32_t(Get16(p)) << 16) | Get16(p + 2); }
void Put16(unsigned char* p, uint16_t n) { p[0] = n >> 8; p[1] = n; }
void Put32(unsigned char* p, uint32_t n) { Put16(p, n >> 16); Put16(p + 2, n); }
uint32_t Sum(const unsigned char* p, size_t n) {
    uint32_t sum = 0;
    for (size_t i = 0; i < n; i += 2)
        sum += (uint16_t(p[i]) << 8) | (i + 1 < n ? p[i + 1] : 0);
    return sum;
}
uint16_t Checksum(const unsigned char* ip, const unsigned char* tcp, size_t n,
                  uint8_t protocol = IPPROTO_TCP) {
    uint32_t sum = Sum(ip + 8, 32) + n + protocol + Sum(tcp, n);
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return ~sum;
}

struct Request {
    nlmsghdr header{};
    std::array<unsigned char, 512> body{};
    Request(uint16_t type, size_t size, uint16_t flags = 0) {
        header.nlmsg_len = NLMSG_LENGTH(size);
        header.nlmsg_type = type;
        header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
    }
    void Attribute(uint16_t type, const void* data, size_t size) {
        size_t offset = NLMSG_ALIGN(header.nlmsg_len);
        ASSERT_LE(offset + RTA_SPACE(size), sizeof(*this));
        auto* attr = reinterpret_cast<rtattr*>(reinterpret_cast<unsigned char*>(this) + offset);
        attr->rta_type = type;
        attr->rta_len = RTA_LENGTH(size);
        memcpy(RTA_DATA(attr), data, size);
        header.nlmsg_len = offset + RTA_SPACE(size);
    }
};

class Ipv6RoutedOutput : public testing::Test {
 protected:
    Fd netlink_, packet_, client_;
    unsigned int owner_ = 0, egress_ = 0, sequence_ = 0;
    in6_addr source_{}, egress_address_{}, peer_{};
    std::array<unsigned char, 6> peer_mac_{}, egress_mac_{};
    bool source_added_ = false, egress_added_ = false, neighbor_added_ = false;
    bool wrong_linklocal_added_ = false, right_linklocal_added_ = false;
    in6_addr wrong_linklocal_{}, right_linklocal_{};
    bool ingress_test_neighbor_ = false, owner_test_neighbor_ = false;
    in6_addr ingress_test_target_{}, owner_test_target_{};
    bool dynamic_neighbor_ = false, source_neighbor_ = false;
    int original_mtu_ = 0;
    unsigned int solicitations_ = 0;
    uint16_t port_ = 0;
    uint32_t client_next_ = 0;
    static constexpr uint16_t kPeerPort = 19876;
    static constexpr uint32_t kPeerSequence = 1234567;

    int Submit(Request& request) {
        request.header.nlmsg_seq = ++sequence_;
        sockaddr_nl kernel{};
        kernel.nl_family = AF_NETLINK;
        if (sendto(netlink_.get(), &request, request.header.nlmsg_len, 0,
                   reinterpret_cast<sockaddr*>(&kernel), sizeof(kernel)) < 0) return -1;
        for (;;) {
            alignas(nlmsghdr) unsigned char reply[8192];
            ssize_t size = recv(netlink_.get(), reply, sizeof(reply), 0);
            if (size < 0) { if (errno == EINTR) continue; return -1; }
            for (auto* h = reinterpret_cast<nlmsghdr*>(reply); NLMSG_OK(h, size);
                 h = NLMSG_NEXT(h, size)) {
                if (h->nlmsg_seq != sequence_ || h->nlmsg_type != NLMSG_ERROR) continue;
                if (h->nlmsg_len < NLMSG_LENGTH(sizeof(nlmsgerr))) { errno = EPROTO; return -1; }
                int error = reinterpret_cast<nlmsgerr*>(NLMSG_DATA(h))->error;
                if (error) { errno = -error; return -1; }
                return 0;
            }
        }
    }
    int Address(unsigned int index, const in6_addr& address, bool add,
                uint8_t scope = RT_SCOPE_UNIVERSE) {
        Request request(add ? RTM_NEWADDR : RTM_DELADDR, sizeof(ifaddrmsg),
                        add ? NLM_F_CREATE | NLM_F_EXCL : 0);
        auto* msg = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(&request.header));
        msg->ifa_family = AF_INET6; msg->ifa_prefixlen = 64;
        msg->ifa_flags = IFA_F_NODAD; msg->ifa_index = index;
        msg->ifa_scope = scope;
        request.Attribute(IFA_ADDRESS, &address, sizeof(address));
        return Submit(request);
    }
    int Neighbor(bool add, const in6_addr* target = nullptr, unsigned int index = 0) {
        Request request(add ? RTM_NEWNEIGH : RTM_DELNEIGH, sizeof(ndmsg),
                        add ? NLM_F_CREATE | NLM_F_EXCL : 0);
        auto* msg = reinterpret_cast<ndmsg*>(NLMSG_DATA(&request.header));
        msg->ndm_family = AF_INET6; msg->ndm_ifindex = index ? index : egress_;
        msg->ndm_state = NUD_PERMANENT; msg->ndm_type = RTN_UNICAST;
        request.Attribute(NDA_DST, target ? target : &peer_, sizeof(peer_));
        if (add) request.Attribute(NDA_LLADDR, peer_mac_.data(), peer_mac_.size());
        return Submit(request);
    }
    int SetMtu(int mtu) {
        Request request(RTM_NEWLINK, sizeof(ifinfomsg));
        reinterpret_cast<ifinfomsg*>(NLMSG_DATA(&request.header))->ifi_index = egress_;
        request.Attribute(IFLA_MTU, &mtu, sizeof(mtu));
        return Submit(request);
    }
    void SetUp() override {
        owner_ = if_nametoindex("veth1"); egress_ = if_nametoindex("veth2");
        ASSERT_NE(owner_, 0u) << "requires the standard veth1/veth2 fixture";
        ASSERT_NE(egress_, 0u);
        ASSERT_EQ(inet_pton(AF_INET6, "fd10:91::1", &source_), 1);
        ASSERT_EQ(inet_pton(AF_INET6, "fd10:92::2", &egress_address_), 1);
        ASSERT_EQ(inet_pton(AF_INET6, "fd10:92::99", &peer_), 1);
        netlink_.reset(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
        ASSERT_GE(netlink_.get(), 0);
        sockaddr_nl local{}; local.nl_family = AF_NETLINK;
        ASSERT_EQ(bind(netlink_.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
        timeval timeout{5, 0};
        ASSERT_EQ(setsockopt(netlink_.get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
        ASSERT_EQ(Address(owner_, source_, true), 0) << strerror(errno); source_added_ = true;
        ASSERT_EQ(Address(egress_, egress_address_, true), 0) << strerror(errno); egress_added_ = true;
        packet_.reset(socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK, htons(ETH_P_IPV6)));
        ASSERT_GE(packet_.get(), 0);
        ifreq request{};
        strcpy(request.ifr_name, "veth1");
        ASSERT_EQ(ioctl(packet_.get(), SIOCGIFHWADDR, &request), 0);
        memcpy(peer_mac_.data(), request.ifr_hwaddr.sa_data, 6);
        strcpy(request.ifr_name, "veth2");
        ASSERT_EQ(ioctl(packet_.get(), SIOCGIFHWADDR, &request), 0);
        memcpy(egress_mac_.data(), request.ifr_hwaddr.sa_data, 6);
        ASSERT_EQ(ioctl(packet_.get(), SIOCGIFMTU, &request), 0);
        original_mtu_ = request.ifr_mtu;
        sockaddr_ll link{}; link.sll_family = AF_PACKET;
        link.sll_protocol = htons(ETH_P_IPV6); link.sll_ifindex = owner_;
        ASSERT_EQ(bind(packet_.get(), reinterpret_cast<sockaddr*>(&link), sizeof(link)), 0);
        ASSERT_EQ(Neighbor(true), 0) << strerror(errno); neighbor_added_ = true;
    }
    void TearDown() override {
        client_.reset(-1);
        if (owner_test_neighbor_) {
            int result = Neighbor(false, &owner_test_target_, owner_);
            EXPECT_TRUE(result == 0 || errno == ENOENT) << strerror(errno);
        }
        if (ingress_test_neighbor_) {
            int result = Neighbor(false, &ingress_test_target_);
            EXPECT_TRUE(result == 0 || errno == ENOENT) << strerror(errno);
        }
        if (neighbor_added_) { EXPECT_EQ(Neighbor(false), 0) << strerror(errno); }
        if (dynamic_neighbor_) {
            int result = Neighbor(false);
            EXPECT_TRUE(result == 0 || errno == ENOENT) << strerror(errno);
        }
        if (source_neighbor_) {
            int result = Neighbor(false, &source_);
            EXPECT_TRUE(result == 0 || errno == ENOENT) << strerror(errno);
        }
        if (right_linklocal_added_) {
            EXPECT_EQ(Address(egress_, right_linklocal_, false, RT_SCOPE_LINK), 0)
                << strerror(errno);
        }
        if (wrong_linklocal_added_) {
            EXPECT_EQ(Address(owner_, wrong_linklocal_, false, RT_SCOPE_LINK), 0)
                << strerror(errno);
        }
        if (original_mtu_) { EXPECT_EQ(SetMtu(original_mtu_), 0) << strerror(errno); }
        if (egress_added_) { EXPECT_EQ(Address(egress_, egress_address_, false), 0) << strerror(errno); }
        if (source_added_) { EXPECT_EQ(Address(owner_, source_, false), 0) << strerror(errno); }
    }

    void SendFrame(const std::vector<unsigned char>& frame) {
        sockaddr_ll link{}; link.sll_family = AF_PACKET; link.sll_ifindex = owner_;
        link.sll_halen = 6; memcpy(link.sll_addr, egress_mac_.data(), 6);
        ASSERT_EQ(sendto(packet_.get(), frame.data(), frame.size(), 0,
                         reinterpret_cast<sockaddr*>(&link), sizeof(link)),
                  static_cast<ssize_t>(frame.size())) << strerror(errno);
    }
    void Ndisc(uint8_t type, const in6_addr& source, const in6_addr& destination,
               const in6_addr& target) {
        std::vector<unsigned char> frame(14 + 40 + 32);
        memcpy(frame.data(), egress_mac_.data(), 6);
        memcpy(frame.data() + 6, peer_mac_.data(), 6);
        Put16(frame.data() + 12, ETH_P_IPV6);
        auto* ip = frame.data() + 14;
        ip[0] = 0x60; Put16(ip + 4, 32); ip[6] = IPPROTO_ICMPV6; ip[7] = 255;
        memcpy(ip + 8, &source, 16); memcpy(ip + 24, &destination, 16);
        auto* icmp = ip + 40;
        icmp[0] = type; icmp[4] = type == 136 ? 0x60 : 0;
        memcpy(icmp + 8, &target, 16);
        icmp[24] = type == 136 ? 2 : 1; icmp[25] = 1;
        memcpy(icmp + 26, peer_mac_.data(), 6);
        Put16(icmp + 2, Checksum(ip, icmp, 32, IPPROTO_ICMPV6));
        SendFrame(frame);
    }
    void SendUdpFrame(const in6_addr& source, const in6_addr& destination,
                      uint16_t destination_port) {
        std::vector<unsigned char> frame(14 + 40 + 8 + 1);
        memcpy(frame.data(), egress_mac_.data(), 6);
        memcpy(frame.data() + 6, peer_mac_.data(), 6);
        Put16(frame.data() + 12, ETH_P_IPV6);
        auto* ip = frame.data() + 14;
        ip[0] = 0x60;
        Put16(ip + 4, 9);
        ip[6] = IPPROTO_UDP;
        ip[7] = 64;
        memcpy(ip + 8, &source, 16);
        memcpy(ip + 24, &destination, 16);
        auto* udp = ip + 40;
        Put16(udp, 19037);
        Put16(udp + 2, destination_port);
        Put16(udp + 4, 9);
        udp[8] = 0x35;
        Put16(udp + 6, Checksum(ip, udp, 9, IPPROTO_UDP));
        SendFrame(frame);
    }
    void Segment(uint8_t flags, uint32_t sequence, uint32_t ack,
                 const unsigned char* payload = nullptr, size_t length = 0) {
        // Advertise a large MSS so the peer cannot accidentally mask egress MTU.
        const size_t tcp_header = (flags & 2) ? 24 : 20;
        std::vector<unsigned char> frame(14 + 40 + tcp_header + length);
        memcpy(frame.data(), egress_mac_.data(), 6);
        memcpy(frame.data() + 6, peer_mac_.data(), 6);
        Put16(frame.data() + 12, ETH_P_IPV6);
        auto* ip = frame.data() + 14;
        ip[0] = 0x60; Put16(ip + 4, tcp_header + length); ip[6] = IPPROTO_TCP; ip[7] = 64;
        memcpy(ip + 8, &peer_, 16); memcpy(ip + 24, &source_, 16);
        auto* tcp = ip + 40;
        Put16(tcp, kPeerPort); Put16(tcp + 2, port_);
        Put32(tcp + 4, sequence); Put32(tcp + 8, ack);
        tcp[12] = (tcp_header / 4) << 4; tcp[13] = flags; Put16(tcp + 14, 65535);
        if (flags & 2) { tcp[20] = 2; tcp[21] = 4; Put16(tcp + 22, 1460); }
        if (length) memcpy(tcp + tcp_header, payload, length);
        Put16(tcp + 16, Checksum(ip, tcp, tcp_header + length));
        SendFrame(frame);
    }
    bool Receive(std::vector<unsigned char>& frame, int timeout_ms) {
        auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(timeout_ms);
        while (std::chrono::steady_clock::now() < deadline) {
            pollfd event{packet_.get(), POLLIN, 0};
            if (poll(&event, 1, 50) < 0) { ADD_FAILURE() << strerror(errno); return false; }
            std::array<unsigned char, 65536> bytes{};
            sockaddr_ll link{}; socklen_t size = sizeof(link);
            ssize_t n = recvfrom(packet_.get(), bytes.data(), bytes.size(), 0,
                                 reinterpret_cast<sockaddr*>(&link), &size);
            if (n < 74 || Get16(bytes.data() + 12) != ETH_P_IPV6) continue;
            const auto* ip = bytes.data() + 14;
            if (n >= 86 && ip[6] == IPPROTO_ICMPV6 && ip[40] == 135 &&
                !memcmp(ip + 48, &peer_, 16) && link.sll_pkttype != PACKET_OUTGOING) {
                EXPECT_EQ(ip[7], 255);
                in6_addr destination;
                memcpy(&destination, ip + 8, 16);
                Ndisc(136, peer_, destination, peer_);
                ++solicitations_;
                continue;
            }
            if (ip[6] != IPPROTO_TCP || memcmp(ip + 8, &source_, 16) ||
                memcmp(ip + 24, &peer_, 16)) continue;
            const auto* tcp = ip + 40;
            if (Get16(tcp) != port_ || Get16(tcp + 2) != kPeerPort) continue;
            EXPECT_EQ(link.sll_pkttype, PACKET_HOST) << "must arrive from veth2, not owner output";
            if (link.sll_pkttype != PACKET_HOST) continue;
            size_t payload_size = Get16(ip + 4);
            if (payload_size < 20 || n < static_cast<ssize_t>(54 + payload_size) ||
                (tcp[12] >> 4) * 4u < 20 || (tcp[12] >> 4) * 4u > payload_size) {
                ADD_FAILURE() << "malformed TCP packet"; return false;
            }
            // RX checksum offload is not used by the synthetic peer.
            EXPECT_EQ(Checksum(ip, tcp, payload_size), 0);
            frame.assign(bytes.begin(), bytes.begin() + 54 + payload_size);
            return true;
        }
        return false;
    }
    void Connect(bool bind_device, bool reuse_socket = false) {
        sockaddr_in6 local{}; local.sin6_family = AF_INET6; local.sin6_addr = source_;
        if (!reuse_socket) {
            client_.reset(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
            ASSERT_GE(client_.get(), 0);
            ASSERT_EQ(bind(client_.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
        }
        if (bind_device) {
            const char device[] = "veth2";
            ASSERT_EQ(setsockopt(client_.get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)), 0);
        }
        sockaddr_in6 remote{}; remote.sin6_family = AF_INET6;
        remote.sin6_addr = peer_; remote.sin6_port = htons(kPeerPort);
        ASSERT_EQ(connect(client_.get(), reinterpret_cast<sockaddr*>(&remote), sizeof(remote)), -1);
        ASSERT_EQ(errno, EINPROGRESS);
        socklen_t size = sizeof(local);
        ASSERT_EQ(getsockname(client_.get(), reinterpret_cast<sockaddr*>(&local), &size), 0);
        port_ = ntohs(local.sin6_port);
        std::vector<unsigned char> frame;
        ASSERT_TRUE(Receive(frame, 4000)) << "no routed SYN";
        ASSERT_EQ(frame[67] & 0x12, 2);
        client_next_ = Get32(frame.data() + 58) + 1;
        ASSERT_NO_FATAL_FAILURE(Segment(0x12, kPeerSequence, client_next_));
        pollfd event{client_.get(), POLLOUT, 0};
        ASSERT_EQ(poll(&event, 1, 4000), 1);
        int error = -1; size = sizeof(error);
        ASSERT_EQ(getsockopt(client_.get(), SOL_SOCKET, SO_ERROR, &error, &size), 0);
        ASSERT_EQ(error, 0);
    }
    void Transfer(size_t length, size_t mtu) {
        std::vector<unsigned char> data(length);
        for (size_t i = 0; i < length; ++i) data[i] = (i * 17 + 9) % 251;
        ASSERT_EQ(send(client_.get(), data.data(), data.size(), MSG_NOSIGNAL),
                  static_cast<ssize_t>(data.size())) << strerror(errno);
        size_t received = 0;
        auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(8);
        while (received < length && std::chrono::steady_clock::now() < deadline) {
            std::vector<unsigned char> frame;
            ASSERT_TRUE(Receive(frame, 2000)) << "TCP data stalled at " << received;
            ASSERT_LE(frame.size() - 14, mtu);
            const auto* tcp = frame.data() + 54;
            size_t header = (tcp[12] >> 4) * 4;
            size_t count = frame.size() - 54 - header;
            if (!count) continue;
            uint32_t sequence = Get32(tcp + 4);
            if (sequence == client_next_) {
                ASSERT_LE(received + count, length);
                EXPECT_EQ(memcmp(tcp + header, data.data() + received, count), 0);
                received += count; client_next_ += count;
            }
            ASSERT_NO_FATAL_FAILURE(Segment(0x10, kPeerSequence + 1, client_next_));
        }
        ASSERT_EQ(received, length);
        // A reply arrives physically on veth2 but belongs to veth1's socket.
        const unsigned char reply[] = "owner-handoff";
        ASSERT_NO_FATAL_FAILURE(Segment(0x18, kPeerSequence + 1, client_next_, reply, sizeof(reply)));
        pollfd event{client_.get(), POLLIN, 0};
        ASSERT_EQ(poll(&event, 1, 4000), 1);
        unsigned char buffer[sizeof(reply)]{};
        ASSERT_EQ(recv(client_.get(), buffer, sizeof(buffer), MSG_WAITALL),
                  static_cast<ssize_t>(sizeof(reply)));
        EXPECT_EQ(memcmp(buffer, reply, sizeof(reply)), 0);
        ASSERT_NO_FATAL_FAILURE(Segment(0x14, kPeerSequence + 1 + sizeof(reply), client_next_));
    }
    void VerifyOversizeUdp(bool cold_neighbor);
};

TEST_F(Ipv6RoutedOutput, ExplicitSourceUsesFibEgressAndReceivesOnOwner) {
    ASSERT_NO_FATAL_FAILURE(Connect(false));
    ASSERT_NO_FATAL_FAILURE(Transfer(256, original_mtu_));
}
TEST_F(Ipv6RoutedOutput, BoundDeviceUsesEgressAndReceivesOnOwner) {
    ASSERT_NO_FATAL_FAILURE(Connect(true));
    ASSERT_NO_FATAL_FAILURE(Transfer(256, original_mtu_));
}
TEST_F(Ipv6RoutedOutput, LocalUdpToNonLoopbackAddressUsesScopedInputDevice) {
    Fd receiver(socket(AF_INET6, SOCK_DGRAM, 0));
    Fd sender(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.get(), 0);
    ASSERT_GE(sender.get(), 0);
    const char owner_name[] = "veth1";
    ASSERT_EQ(setsockopt(receiver.get(), SOL_SOCKET, SO_BINDTODEVICE,
                         owner_name, sizeof(owner_name)), 0) << strerror(errno);

    sockaddr_in6 destination{};
    destination.sin6_family = AF_INET6;
    destination.sin6_addr = source_;
    ASSERT_EQ(bind(receiver.get(), reinterpret_cast<sockaddr*>(&destination),
                   sizeof(destination)), 0) << strerror(errno);
    socklen_t length = sizeof(destination);
    ASSERT_EQ(getsockname(receiver.get(), reinterpret_cast<sockaddr*>(&destination), &length), 0);

    constexpr char payload[] = "ipv6-scoped-local";
    ASSERT_EQ(sendto(sender.get(), payload, sizeof(payload), 0,
                     reinterpret_cast<sockaddr*>(&destination), length),
              static_cast<ssize_t>(sizeof(payload))) << strerror(errno);
    pollfd event{receiver.get(), POLLIN, 0};
    ASSERT_EQ(poll(&event, 1, 1000), 1) << strerror(errno);
    char received[sizeof(payload)]{};
    ASSERT_EQ(recv(receiver.get(), received, sizeof(received), 0),
              static_cast<ssize_t>(sizeof(payload))) << strerror(errno);
    EXPECT_EQ(memcmp(received, payload, sizeof(payload)), 0);
}
TEST_F(Ipv6RoutedOutput, LargeTcpTransferUsesSmallerEgressMtu) {
    ASSERT_EQ(SetMtu(1280), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(Connect(true));
    ASSERT_NO_FATAL_FAILURE(Transfer(8192, 1280));
}
TEST_F(Ipv6RoutedOutput, ColdNeighborDiscoveryReleasesQueuedTcp) {
    ASSERT_EQ(Neighbor(false), 0) << strerror(errno);
    neighbor_added_ = false;
    dynamic_neighbor_ = true;
    ASSERT_NO_FATAL_FAILURE(Connect(true));
    ASSERT_GT(solicitations_, 0u) << "must exercise NDP, not a warm cache";
    ASSERT_NO_FATAL_FAILURE(Transfer(256, original_mtu_));
}
TEST_F(Ipv6RoutedOutput, ColdNeighborDiscoveryReleasesPreparedUdpPromptly) {
    ASSERT_EQ(Neighbor(false), 0) << strerror(errno);
    neighbor_added_ = false;
    dynamic_neighbor_ = true;
    client_.reset(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(client_.get(), 0);
    sockaddr_in6 local{};
    local.sin6_family = AF_INET6;
    local.sin6_addr = source_;
    ASSERT_EQ(bind(client_.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    const char device[] = "veth2";
    ASSERT_EQ(setsockopt(client_.get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)), 0);
    sockaddr_in6 remote{};
    remote.sin6_family = AF_INET6;
    remote.sin6_addr = peer_;
    remote.sin6_port = htons(kPeerPort);
    const unsigned char payload = 0x43;
    ASSERT_EQ(sendto(client_.get(), &payload, 1, 0,
                     reinterpret_cast<sockaddr*>(&remote), sizeof(remote)), 1);

    bool answered = false;
    std::chrono::steady_clock::time_point answered_at{};
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(4);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd event{packet_.get(), POLLIN, 0};
        ASSERT_GE(poll(&event, 1, 50), 0);
        std::array<unsigned char, 2048> frame{};
        sockaddr_ll link{};
        socklen_t size = sizeof(link);
        const ssize_t count = recvfrom(packet_.get(), frame.data(), frame.size(), 0,
                                       reinterpret_cast<sockaddr*>(&link), &size);
        if (count < 14 + 40 + 8 || Get16(frame.data() + 12) != ETH_P_IPV6) continue;
        const auto* ip = frame.data() + 14;
        if (count >= 14 + 40 + 24 && ip[6] == IPPROTO_ICMPV6 && ip[40] == 135 &&
            !memcmp(ip + 48, &peer_, sizeof(peer_)) && link.sll_pkttype != PACKET_OUTGOING) {
            ASSERT_FALSE(answered) << "neighbor retried before the prepared packet was sent";
            in6_addr soliciting_address{};
            memcpy(&soliciting_address, ip + 8, sizeof(soliciting_address));
            Ndisc(136, peer_, soliciting_address, peer_);
            answered = true;
            answered_at = std::chrono::steady_clock::now();
            continue;
        }
        if (count < 14 + 40 + 8 + 1 || ip[6] != IPPROTO_UDP ||
            memcmp(ip + 8, &source_, sizeof(source_)) ||
            memcmp(ip + 24, &peer_, sizeof(peer_)) || Get16(ip + 42) != kPeerPort) continue;
        ASSERT_TRUE(answered) << "packet bypassed the cold neighbor probe";
        EXPECT_EQ(link.sll_pkttype, PACKET_HOST);
        EXPECT_EQ(ip[48], payload);
        EXPECT_LT(std::chrono::steady_clock::now() - answered_at,
                  std::chrono::milliseconds(850))
            << "prepared datagram waited for the NDP retry timer after the reply";
        return;
    }
    FAIL() << "prepared datagram was not emitted after the neighbor reply";
}
TEST_F(Ipv6RoutedOutput, UdpMtuSizedPayloadUsesPhysicalEgress) {
    ASSERT_EQ(SetMtu(1280), 0) << strerror(errno);
    client_.reset(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(client_.get(), 0);
    sockaddr_in6 local{}; local.sin6_family = AF_INET6; local.sin6_addr = source_;
    ASSERT_EQ(bind(client_.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    const char device[] = "veth2";
    ASSERT_EQ(setsockopt(client_.get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)), 0);
    sockaddr_in6 remote{}; remote.sin6_family = AF_INET6;
    remote.sin6_addr = peer_; remote.sin6_port = htons(kPeerPort);
    std::vector<unsigned char> payload(1232, 0x5a);
    ASSERT_EQ(sendto(client_.get(), payload.data(), payload.size(), 0,
                     reinterpret_cast<sockaddr*>(&remote), sizeof(remote)), 1232);
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(4);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd event{packet_.get(), POLLIN, 0};
        ASSERT_GE(poll(&event, 1, 50), 0);
        unsigned char frame[2048]; sockaddr_ll link{}; socklen_t size = sizeof(link);
        ssize_t n = recvfrom(packet_.get(), frame, sizeof(frame), 0,
                             reinterpret_cast<sockaddr*>(&link), &size);
        if (n < 62 || Get16(frame + 12) != ETH_P_IPV6) continue;
        const auto* ip = frame + 14;
        if (ip[6] != IPPROTO_UDP || memcmp(ip + 8, &source_, 16) ||
            memcmp(ip + 24, &peer_, 16) || Get16(ip + 42) != kPeerPort) continue;
        ASSERT_EQ(link.sll_pkttype, PACKET_HOST);
        ASSERT_EQ(n, 14 + 1280);
        ASSERT_EQ(Get16(ip + 4), 1240);
        ASSERT_EQ(Get16(ip + 44), 1240);
        ASSERT_EQ(Checksum(ip, ip + 40, 1240, IPPROTO_UDP), 0);
        ASSERT_EQ(memcmp(ip + 48, payload.data(), payload.size()), 0);
        return;
    }
    FAIL() << "MTU-sized UDP datagram did not arrive physically from veth2";
}
void Ipv6RoutedOutput::VerifyOversizeUdp(bool cold_neighbor) {
    ASSERT_EQ(SetMtu(1280), 0) << strerror(errno);
    if (cold_neighbor) {
        ASSERT_EQ(Neighbor(false), 0) << strerror(errno);
        neighbor_added_ = false;
        dynamic_neighbor_ = true;
    }
    client_.reset(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(client_.get(), 0);
    sockaddr_in6 local{}; local.sin6_family = AF_INET6; local.sin6_addr = source_;
    ASSERT_EQ(bind(client_.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    const char device[] = "veth2";
    ASSERT_EQ(setsockopt(client_.get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)), 0);
    sockaddr_in6 remote{}; remote.sin6_family = AF_INET6;
    remote.sin6_addr = peer_; remote.sin6_port = htons(kPeerPort);
    std::vector<unsigned char> payload(2000);
    for (size_t i = 0; i < payload.size(); ++i) payload[i] = (i * 17 + 9) % 251;
    ASSERT_EQ(sendto(client_.get(), payload.data(), payload.size(), 0,
                     reinterpret_cast<sockaddr*>(&remote), sizeof(remote)),
              static_cast<ssize_t>(payload.size())) << strerror(errno);

    std::vector<unsigned char> reassembled(payload.size() + 8);
    std::vector<bool> received(reassembled.size(), false);
    uint32_t fragment_id = 0;
    bool have_id = false, saw_last = false;
    bool answered = false;
    std::chrono::steady_clock::time_point answered_at{};
    size_t filled = 0;
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(4);
    while (filled < reassembled.size() && std::chrono::steady_clock::now() < deadline) {
        pollfd event{packet_.get(), POLLIN, 0};
        ASSERT_GE(poll(&event, 1, 50), 0);
        unsigned char frame[2048]; sockaddr_ll link{}; socklen_t size = sizeof(link);
        ssize_t n = recvfrom(packet_.get(), frame, sizeof(frame), 0,
                             reinterpret_cast<sockaddr*>(&link), &size);
        if (n < 62 || Get16(frame + 12) != ETH_P_IPV6) continue;
        const auto* ip = frame + 14;
        if (n >= 78 && ip[6] == IPPROTO_ICMPV6 && ip[40] == 135 &&
            !memcmp(ip + 48, &peer_, sizeof(peer_)) && link.sll_pkttype != PACKET_OUTGOING) {
            ASSERT_TRUE(cold_neighbor) << "permanent neighbor unexpectedly needed solicitation";
            ASSERT_FALSE(answered) << "neighbor retried before the fragments were emitted";
            in6_addr soliciting_address{};
            memcpy(&soliciting_address, ip + 8, sizeof(soliciting_address));
            Ndisc(136, peer_, soliciting_address, peer_);
            answered = true;
            answered_at = std::chrono::steady_clock::now();
            continue;
        }
        if (ip[6] != IPPROTO_FRAGMENT || memcmp(ip + 8, &source_, 16) ||
            memcmp(ip + 24, &peer_, 16) || ip[40] != IPPROTO_UDP) continue;
        if (cold_neighbor) {
            ASSERT_TRUE(answered) << "fragments bypassed the cold neighbor probe";
        }
        if (cold_neighbor && !have_id) {
            EXPECT_LT(std::chrono::steady_clock::now() - answered_at,
                      std::chrono::milliseconds(850));
        }
        ASSERT_EQ(link.sll_pkttype, PACKET_HOST);
        ASSERT_LE(n - 14, 1280);
        ASSERT_GE(Get16(ip + 4), 8);
        const size_t frag_len = Get16(ip + 4) - 8;
        const uint16_t flags = Get16(ip + 42);
        const size_t offset = size_t(flags >> 3) * 8;
        ASSERT_EQ(n, static_cast<ssize_t>(14 + 40 + 8 + frag_len));
        ASSERT_LE(offset + frag_len, reassembled.size());
        if (flags & 1) EXPECT_EQ(frag_len % 8, 0u);
        else saw_last = true;
        if (!have_id) { fragment_id = Get32(ip + 44); have_id = true; }
        EXPECT_EQ(Get32(ip + 44), fragment_id);
        for (size_t i = 0; i < frag_len; ++i) {
            if (!received[offset + i]) ++filled;
            received[offset + i] = true;
            reassembled[offset + i] = ip[48 + i];
        }
    }
    ASSERT_TRUE(have_id && saw_last);
    ASSERT_EQ(filled, reassembled.size());
    EXPECT_EQ(Get16(reassembled.data() + 2), kPeerPort);
    EXPECT_EQ(Get16(reassembled.data() + 4), reassembled.size());
    EXPECT_EQ(memcmp(reassembled.data() + 8, payload.data(), payload.size()), 0);
    unsigned char pseudo_ip[40]{};
    memcpy(pseudo_ip + 8, &source_, 16);
    memcpy(pseudo_ip + 24, &peer_, 16);
    EXPECT_EQ(Checksum(pseudo_ip, reassembled.data(), reassembled.size(), IPPROTO_UDP), 0);
}
TEST_F(Ipv6RoutedOutput, OversizeUdpIsSourceFragmentedAfterOneSend) {
    ASSERT_NO_FATAL_FAILURE(VerifyOversizeUdp(false));
}
TEST_F(Ipv6RoutedOutput, ColdNeighborOversizeUdpIsSourceFragmentedAfterOneSend) {
    ASSERT_NO_FATAL_FAILURE(VerifyOversizeUdp(true));
}
TEST_F(Ipv6RoutedOutput, MissingBoundDeviceRouteDoesNotConsumeSocket) {
    client_.reset(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(client_.get(), 0);
    sockaddr_in6 local{}; local.sin6_family = AF_INET6; local.sin6_addr = source_;
    ASSERT_EQ(bind(client_.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    const char wrong_device[] = "veth1";
    ASSERT_EQ(setsockopt(client_.get(), SOL_SOCKET, SO_BINDTODEVICE,
                         wrong_device, sizeof(wrong_device)), 0);
    sockaddr_in6 remote{}; remote.sin6_family = AF_INET6;
    remote.sin6_addr = peer_; remote.sin6_port = htons(kPeerPort);
    ASSERT_EQ(connect(client_.get(), reinterpret_cast<sockaddr*>(&remote), sizeof(remote)), -1);
    ASSERT_EQ(errno, ENETUNREACH);
    ASSERT_NO_FATAL_FAILURE(Connect(true, true));
    ASSERT_NO_FATAL_FAILURE(Transfer(256, original_mtu_));
}
TEST_F(Ipv6RoutedOutput, GlobalSourceNeighborAdvertisementStaysOnIngressInterface) {
    // A global source owned by the other interface must not reroute the NDP
    // reply through RTN_LOCAL. NDP is always scoped to the receiving link.
    source_neighbor_ = true;
    ASSERT_NO_FATAL_FAILURE(Ndisc(135, source_, egress_address_, egress_address_));
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(4);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd event{packet_.get(), POLLIN, 0};
        ASSERT_GE(poll(&event, 1, 50), 0);
        unsigned char frame[2048];
        sockaddr_ll link{}; socklen_t size = sizeof(link);
        ssize_t n = recvfrom(packet_.get(), frame, sizeof(frame), 0,
                             reinterpret_cast<sockaddr*>(&link), &size);
        if (n < 78 || Get16(frame + 12) != ETH_P_IPV6) continue;
        const auto* ip = frame + 14;
        if (ip[6] != IPPROTO_ICMPV6 || ip[40] != 136 ||
            memcmp(ip + 8, &egress_address_, 16) || memcmp(ip + 24, &source_, 16)) continue;
        ASSERT_EQ(link.sll_pkttype, PACKET_HOST);
        ASSERT_EQ(ip[7], 255);
        ASSERT_LE(54 + Get16(ip + 4), n);
        ASSERT_EQ(Checksum(ip, ip + 40, Get16(ip + 4), IPPROTO_ICMPV6), 0);
        return;
    }
    FAIL() << "NDP response was not emitted back through physical ingress veth2";
}
TEST_F(Ipv6RoutedOutput, CrossInterfaceNeighborAdvertisementCannotPoisonOwnerCache) {
    // A unicast NA addressed to veth1 arrives on veth2. It may resolve a
    // pending neighbor on that ingress link, but must not poison veth1.
    in6_addr ingress_target{};
    ASSERT_EQ(inet_pton(AF_INET6, "fd10:92::98", &ingress_target), 1);
    ingress_test_target_ = ingress_target;
    int result = Neighbor(false, &ingress_test_target_);
    ASSERT_TRUE(result == 0 || errno == ENOENT) << strerror(errno);
    ingress_test_neighbor_ = true;
    Fd ingress_sender(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(ingress_sender.get(), 0);
    sockaddr_in6 ingress_local{};
    ingress_local.sin6_family = AF_INET6;
    ingress_local.sin6_addr = egress_address_;
    ASSERT_EQ(bind(ingress_sender.get(), reinterpret_cast<sockaddr*>(&ingress_local),
                   sizeof(ingress_local)), 0);
    sockaddr_in6 ingress_remote{};
    ingress_remote.sin6_family = AF_INET6;
    ingress_remote.sin6_addr = ingress_target;
    ingress_remote.sin6_port = htons(19036);
    const uint8_t probe = 0x35;
    ASSERT_EQ(1, sendto(ingress_sender.get(), &probe, 1, 0,
                        reinterpret_cast<sockaddr*>(&ingress_remote),
                        sizeof(ingress_remote))) << strerror(errno);
    bool saw_pending_solicit = false;
    auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(4);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd ready{packet_.get(), POLLIN, 0};
        ASSERT_GE(poll(&ready, 1, 50), 0);
        std::array<unsigned char, 2048> frame{};
        sockaddr_ll link{};
        socklen_t link_size = sizeof(link);
        const ssize_t count = recvfrom(packet_.get(), frame.data(), frame.size(), 0,
                                       reinterpret_cast<sockaddr*>(&link), &link_size);
        if (count < 14 + 40 + 24 || Get16(frame.data() + 12) != ETH_P_IPV6 ||
            link.sll_pkttype == PACKET_OUTGOING) continue;
        const auto* ip = frame.data() + 14;
        if (ip[6] == IPPROTO_ICMPV6 && ip[40] == 135 &&
            !memcmp(ip + 48, &ingress_target, sizeof(ingress_target))) {
            saw_pending_solicit = true;
            break;
        }
    }
    ASSERT_TRUE(saw_pending_solicit) << "veth2 must first have an unresolved neighbor";
    Ndisc(136, ingress_target, source_, ingress_target);
    bool saw_ingress_udp = false;
    deadline = std::chrono::steady_clock::now() + std::chrono::seconds(4);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd ready{packet_.get(), POLLIN, 0};
        ASSERT_GE(poll(&ready, 1, 50), 0);
        std::array<unsigned char, 2048> frame{};
        sockaddr_ll link{};
        socklen_t link_size = sizeof(link);
        const ssize_t count = recvfrom(packet_.get(), frame.data(), frame.size(), 0,
                                       reinterpret_cast<sockaddr*>(&link), &link_size);
        if (count < 14 + 40 + 8 || Get16(frame.data() + 12) != ETH_P_IPV6 ||
            link.sll_pkttype == PACKET_OUTGOING) continue;
        const auto* ip = frame.data() + 14;
        if (ip[6] == IPPROTO_UDP &&
            !memcmp(ip + 24, &ingress_target, sizeof(ingress_target))) {
            saw_ingress_udp = true;
            break;
        }
    }
    ASSERT_TRUE(saw_ingress_udp) << "NA must resolve veth2's pending neighbor";

    in6_addr target{};
    ASSERT_EQ(inet_pton(AF_INET6, "fd10:91::99", &target), 1);
    owner_test_target_ = target;
    result = Neighbor(false, &owner_test_target_, owner_);
    ASSERT_TRUE(result == 0 || errno == ENOENT) << strerror(errno);
    owner_test_neighbor_ = true;
    Fd observer(socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK, htons(ETH_P_IPV6)));
    ASSERT_GE(observer.get(), 0);
    sockaddr_ll bind_address{};
    bind_address.sll_family = AF_PACKET;
    bind_address.sll_protocol = htons(ETH_P_IPV6);
    bind_address.sll_ifindex = static_cast<int>(egress_);
    ASSERT_EQ(bind(observer.get(), reinterpret_cast<sockaddr*>(&bind_address),
                   sizeof(bind_address)), 0);

    Ndisc(136, target, source_, target);

    Fd sender(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(sender.get(), 0);
    sockaddr_in6 local{};
    local.sin6_family = AF_INET6;
    local.sin6_addr = source_;
    ASSERT_EQ(bind(sender.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
    sockaddr_in6 remote{};
    remote.sin6_family = AF_INET6;
    remote.sin6_addr = target;
    remote.sin6_port = htons(19035);
    ASSERT_EQ(1, sendto(sender.get(), &probe, 1, 0,
                        reinterpret_cast<sockaddr*>(&remote), sizeof(remote))) << strerror(errno);

    deadline = std::chrono::steady_clock::now() + std::chrono::seconds(4);
    while (std::chrono::steady_clock::now() < deadline) {
        pollfd ready{observer.get(), POLLIN, 0};
        ASSERT_GE(poll(&ready, 1, 50), 0);
        std::array<unsigned char, 2048> frame{};
        sockaddr_ll link{};
        socklen_t link_size = sizeof(link);
        const ssize_t count = recvfrom(observer.get(), frame.data(), frame.size(), 0,
                                       reinterpret_cast<sockaddr*>(&link), &link_size);
        if (count < 14 + 40 + 24 || Get16(frame.data() + 12) != ETH_P_IPV6 ||
            link.sll_pkttype == PACKET_OUTGOING) continue;
        const auto* ip = frame.data() + 14;
        if (ip[6] == IPPROTO_ICMPV6 && ip[40] == 135 &&
            !memcmp(ip + 48, &target, sizeof(target))) {
            EXPECT_EQ(ip[7], 255);
            return;
        }
        if (ip[6] == IPPROTO_UDP && !memcmp(ip + 24, &target, sizeof(target))) {
            FAIL() << "veth1 transmitted directly after an NA received only on veth2";
            return;
        }
    }
    FAIL() << "veth1 did not solicit the still-unknown neighbor";
}
TEST_F(Ipv6RoutedOutput, LinkLocalUdpDeliveryFollowsIngressScope) {
    ASSERT_EQ(inet_pton(AF_INET6, "fe80::35:1", &wrong_linklocal_), 1);
    ASSERT_EQ(inet_pton(AF_INET6, "fe80::35:2", &right_linklocal_), 1);
    ASSERT_EQ(Address(owner_, wrong_linklocal_, true, RT_SCOPE_LINK), 0)
        << strerror(errno);
    wrong_linklocal_added_ = true;
    ASSERT_EQ(Address(egress_, right_linklocal_, true, RT_SCOPE_LINK), 0)
        << strerror(errno);
    right_linklocal_added_ = true;
    Fd wrong(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    Fd right(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(wrong.get(), 0);
    ASSERT_GE(right.get(), 0);
    sockaddr_in6 bind_address{};
    bind_address.sin6_family = AF_INET6;
    bind_address.sin6_port = htons(19038);
    bind_address.sin6_addr = wrong_linklocal_;
    bind_address.sin6_scope_id = owner_;
    ASSERT_EQ(bind(wrong.get(), reinterpret_cast<sockaddr*>(&bind_address),
                   sizeof(bind_address)), 0) << strerror(errno);
    bind_address.sin6_addr = right_linklocal_;
    bind_address.sin6_scope_id = egress_;
    ASSERT_EQ(bind(right.get(), reinterpret_cast<sockaddr*>(&bind_address),
                   sizeof(bind_address)), 0) << strerror(errno);
    in6_addr synthetic_source{};
    ASSERT_EQ(inet_pton(AF_INET6, "fe80::35:3", &synthetic_source), 1);
    SendUdpFrame(synthetic_source, wrong_linklocal_, 19038);
    pollfd wrong_ready{wrong.get(), POLLIN, 0};
    pollfd right_ready{right.get(), POLLIN, 0};
    EXPECT_EQ(0, poll(&wrong_ready, 1, 150))
        << "wrong-interface link-local UDP must not reach veth1's socket";
    EXPECT_EQ(0, poll(&right_ready, 1, 0));
    SendUdpFrame(synthetic_source, right_linklocal_, 19038);
    EXPECT_EQ(1, poll(&right_ready, 1, 1000));
    unsigned char received = 0;
    EXPECT_EQ(1, recv(right.get(), &received, sizeof(received), 0));
    EXPECT_EQ(0x35, received);
}
TEST_F(Ipv6RoutedOutput, LinkLocalUdpWildcardRejectsCrossInterfaceLocalAddress) {
    ASSERT_EQ(inet_pton(AF_INET6, "fe80::35:1", &wrong_linklocal_), 1);
    ASSERT_EQ(Address(owner_, wrong_linklocal_, true, RT_SCOPE_LINK), 0)
        << strerror(errno);
    wrong_linklocal_added_ = true;
    Fd receiver(socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(receiver.get(), 0);
    sockaddr_in6 local{};
    local.sin6_family = AF_INET6;
    local.sin6_port = htons(19039);
    ASSERT_EQ(bind(receiver.get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0)
        << strerror(errno);
    in6_addr synthetic_source{};
    ASSERT_EQ(inet_pton(AF_INET6, "fe80::35:3", &synthetic_source), 1);
    SendUdpFrame(synthetic_source, wrong_linklocal_, 19039);
    pollfd ready{receiver.get(), POLLIN, 0};
    EXPECT_EQ(poll(&ready, 1, 150), 0)
        << "link-local local address belongs to a different ingress link";
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
