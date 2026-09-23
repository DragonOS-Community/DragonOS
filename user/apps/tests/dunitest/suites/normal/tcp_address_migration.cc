// TCP protocol ownership must survive changes to the local address's device.
// Requires the standard veth1/veth2 fixture. All addresses are private to this
// test; no pre-existing address, link state, or route is changed.
#include <gtest/gtest.h>
#include <arpa/inet.h>
#include <linux/if_addr.h>
#include <linux/if_packet.h>
#include <linux/neighbour.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <net/ethernet.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <chrono>
#include <cstring>
#include <vector>

namespace {
class Fd {
 public:
    Fd() = default;
    ~Fd() { Reset(); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    void Reset(int fd = -1) { if (fd_ >= 0) close(fd_); fd_ = fd; }
 private:
    int fd_ = -1;
};

class TcpAddressMigration : public testing::Test {
 protected:
    struct Address {
        int family;
        unsigned char prefix = 0;
        std::array<unsigned char, 16> bytes{};
        sockaddr_storage SocketAddress(uint16_t port = 0) const {
            sockaddr_storage storage{};
            if (family == AF_INET) {
                auto* value = reinterpret_cast<sockaddr_in*>(&storage);
                value->sin_family = family;
                value->sin_port = port;
                memcpy(&value->sin_addr, bytes.data(), sizeof(value->sin_addr));
            } else {
                auto* value = reinterpret_cast<sockaddr_in6*>(&storage);
                value->sin6_family = family;
                value->sin6_port = port;
                memcpy(&value->sin6_addr, bytes.data(), sizeof(value->sin6_addr));
            }
            return storage;
        }
        socklen_t Length() const {
            return family == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6);
        }
    };
    struct InstalledAddress { unsigned int device; Address address; };
    unsigned int first_ = 0, second_ = 0, sequence_ = 0;
    Fd netlink_, listener_, client_, accepted_;
    Address local_{}, remote_{};
    uint16_t listen_port_ = 0;
    std::vector<InstalledAddress> installed_;

    void SetUp() override {
        first_ = if_nametoindex("veth1"); second_ = if_nametoindex("veth2");
        ASSERT_NE(first_, 0u) << "requires the standard veth1/veth2 fixture";
        ASSERT_NE(second_, 0u);
        netlink_.Reset(socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE));
        ASSERT_GE(netlink_.get(), 0);
        sockaddr_nl bind_address{}; bind_address.nl_family = AF_NETLINK;
        ASSERT_EQ(bind(netlink_.get(), reinterpret_cast<sockaddr*>(&bind_address),
                       sizeof(bind_address)), 0);
        timeval timeout{3, 0};
        ASSERT_EQ(setsockopt(netlink_.get(), SOL_SOCKET, SO_RCVTIMEO,
                             &timeout, sizeof(timeout)), 0);
    }
    void TearDown() override {
        client_.Reset(); accepted_.Reset(); listener_.Reset();
        for (auto it = installed_.rbegin(); it != installed_.rend(); ++it)
            EXPECT_EQ(ChangeAddress(it->device, it->address, false), 0) << strerror(errno);
    }
    int ChangeAddress(unsigned int device, const Address& address, bool add) {
        struct Request {
            nlmsghdr header{};
            ifaddrmsg value{};
            unsigned char attrs[64]{};
        } request;
        request.header.nlmsg_len = NLMSG_LENGTH(sizeof(request.value));
        request.header.nlmsg_type = add ? RTM_NEWADDR : RTM_DELADDR;
        request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK |
                                     (add ? NLM_F_CREATE | NLM_F_EXCL : 0);
        request.header.nlmsg_seq = ++sequence_;
        request.value.ifa_family = address.family;
        request.value.ifa_prefixlen = address.prefix ? address.prefix : address.family == AF_INET ? 32 : 128;
        request.value.ifa_flags = IFA_F_NODAD;
        request.value.ifa_index = device;
        size_t size = address.family == AF_INET ? 4 : 16;
        for (int type : {IFA_ADDRESS, IFA_LOCAL}) {
            if (type == IFA_LOCAL && address.family == AF_INET6) continue;
            auto* attribute = reinterpret_cast<rtattr*>(
                reinterpret_cast<char*>(&request) + NLMSG_ALIGN(request.header.nlmsg_len));
            attribute->rta_type = type; attribute->rta_len = RTA_LENGTH(size);
            memcpy(RTA_DATA(attribute), address.bytes.data(), size);
            request.header.nlmsg_len = NLMSG_ALIGN(request.header.nlmsg_len) + RTA_SPACE(size);
        }
        return Submit(request.header);
    }
    int Submit(nlmsghdr& request) {
        request.nlmsg_seq = ++sequence_;
        sockaddr_nl kernel{}; kernel.nl_family = AF_NETLINK;
        if (sendto(netlink_.get(), &request, request.nlmsg_len, 0,
                   reinterpret_cast<sockaddr*>(&kernel), sizeof(kernel)) < 0) return -1;
        for (;;) {
            alignas(nlmsghdr) unsigned char reply[8192];
            ssize_t size = recv(netlink_.get(), reply, sizeof(reply), 0);
            if (size < 0) { if (errno == EINTR) continue; return -1; }
            for (auto* header = reinterpret_cast<nlmsghdr*>(reply); NLMSG_OK(header, size);
                 header = NLMSG_NEXT(header, size)) {
                if (header->nlmsg_seq != sequence_ || header->nlmsg_type != NLMSG_ERROR) continue;
                if (header->nlmsg_len < NLMSG_LENGTH(sizeof(nlmsgerr))) { errno = EPROTO; return -1; }
                int error = reinterpret_cast<nlmsgerr*>(NLMSG_DATA(header))->error;
                if (error) { errno = -error; return -1; }
                return 0;
            }
        }
    }
    void Add(unsigned int device, const Address& address) {
        ASSERT_EQ(ChangeAddress(device, address, true), 0) << strerror(errno);
        installed_.push_back({device, address});
    }
    void Remove(unsigned int device, const Address& address) {
        ASSERT_EQ(ChangeAddress(device, address, false), 0) << strerror(errno);
        for (auto it = installed_.begin(); it != installed_.end(); ++it) {
            if (it->device == device && it->address.family == address.family &&
                it->address.bytes == address.bytes) { installed_.erase(it); return; }
        }
        FAIL() << "attempted to remove an address not installed by this test";
    }
    void Configure(int family = AF_INET) {
        local_.family = remote_.family = family;
        ASSERT_EQ(inet_pton(family, family == AF_INET ? "198.18.78.1" : "fd11:78::1",
                            local_.bytes.data()), 1);
        ASSERT_EQ(inet_pton(family, family == AF_INET ? "198.18.78.2" : "fd11:78::2",
                            remote_.bytes.data()), 1);
        ASSERT_NO_FATAL_FAILURE(Add(first_, local_));
        ASSERT_NO_FATAL_FAILURE(Add(second_, remote_));
    }
    void MoveLocal() {
        ASSERT_NO_FATAL_FAILURE(Remove(first_, local_));
        ASSERT_NO_FATAL_FAILURE(Add(second_, local_));
    }
    void NewSocket(Fd& fd, bool reuse = false) {
        fd.Reset(socket(local_.family, SOCK_STREAM | SOCK_NONBLOCK, 0));
        ASSERT_GE(fd.get(), 0);
        if (reuse) {
            int yes = 1;
            ASSERT_EQ(setsockopt(fd.get(), SOL_SOCKET, SO_REUSEADDR, &yes, sizeof(yes)), 0);
        }
    }
    void Bind(int fd, const Address& address, uint16_t port = 0) {
        auto local = address.SocketAddress(port);
        ASSERT_EQ(bind(fd, reinterpret_cast<sockaddr*>(&local), address.Length()), 0)
            << strerror(errno);
    }
    uint16_t Port(int fd) {
        sockaddr_storage address{}; socklen_t size = sizeof(address);
        if (getsockname(fd, reinterpret_cast<sockaddr*>(&address), &size) != 0) return 0;
        return address.ss_family == AF_INET
            ? reinterpret_cast<sockaddr_in*>(&address)->sin_port
            : reinterpret_cast<sockaddr_in6*>(&address)->sin6_port;
    }
    void Wait(int fd, short event) {
        pollfd descriptor{fd, event, 0};
        ASSERT_EQ(poll(&descriptor, 1, 3000), 1) << strerror(errno);
    }
    void Listen(const Address& address, bool reuse = false) {
        ASSERT_NO_FATAL_FAILURE(NewSocket(listener_, reuse));
        ASSERT_NO_FATAL_FAILURE(Bind(listener_.get(), address));
        listen_port_ = Port(listener_.get()); ASSERT_NE(listen_port_, 0);
        ASSERT_EQ(listen(listener_.get(), 4), 0);
    }
    void Connect(const Address& from, const Address& to, uint16_t port = 0,
                 bool reuse = false) {
        ASSERT_NO_FATAL_FAILURE(NewSocket(client_, reuse));
        ASSERT_NO_FATAL_FAILURE(Bind(client_.get(), from, port));
        auto peer = to.SocketAddress(listen_port_);
        int result = connect(client_.get(), reinterpret_cast<sockaddr*>(&peer), to.Length());
        if (result != 0) { ASSERT_EQ(errno, EINPROGRESS) << strerror(errno); }
        ASSERT_NO_FATAL_FAILURE(Wait(client_.get(), POLLOUT));
        int error = -1; socklen_t size = sizeof(error);
        ASSERT_EQ(getsockopt(client_.get(), SOL_SOCKET, SO_ERROR, &error, &size), 0);
        ASSERT_EQ(error, 0) << strerror(error);
    }
    void Accept() {
        ASSERT_NO_FATAL_FAILURE(Wait(listener_.get(), POLLIN));
        accepted_.Reset(accept4(listener_.get(), nullptr, nullptr, SOCK_NONBLOCK));
        ASSERT_GE(accepted_.get(), 0) << strerror(errno);
    }
    void Exchange(int sender, int receiver) {
        constexpr char sent = 'M'; char received = 0;
        ASSERT_EQ(send(sender, &sent, 1, MSG_NOSIGNAL), 1) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(Wait(receiver, POLLIN));
        ASSERT_EQ(recv(receiver, &received, 1, 0), 1) << strerror(errno);
        ASSERT_EQ(received, sent);
    }
    void BothDirections() {
        ASSERT_NO_FATAL_FAILURE(Exchange(client_.get(), accepted_.get()));
        ASSERT_NO_FATAL_FAILURE(Exchange(accepted_.get(), client_.get()));
    }
    void OrderlyClose(Fd& active, Fd& passive) {
        ASSERT_EQ(shutdown(active.get(), SHUT_WR), 0);
        ASSERT_NO_FATAL_FAILURE(Wait(passive.get(), POLLIN));
        char byte;
        ASSERT_EQ(recv(passive.get(), &byte, 1, 0), 0);
        passive.Reset();
        ASSERT_NO_FATAL_FAILURE(Wait(active.get(), POLLIN));
        ASSERT_EQ(recv(active.get(), &byte, 1, 0), 0);
        active.Reset();
    }
};

TEST_F(TcpAddressMigration, EstablishedIpv4DataAndFinSurviveMove) {
    ASSERT_NO_FATAL_FAILURE(Configure());
    ASSERT_NO_FATAL_FAILURE(Listen(local_));
    ASSERT_NO_FATAL_FAILURE(Connect(remote_, local_));
    ASSERT_NO_FATAL_FAILURE(Accept());
    ASSERT_NO_FATAL_FAILURE(BothDirections());
    ASSERT_NO_FATAL_FAILURE(MoveLocal());
    ASSERT_NO_FATAL_FAILURE(BothDirections());
    ASSERT_NO_FATAL_FAILURE(OrderlyClose(accepted_, client_));
}

TEST_F(TcpAddressMigration, EstablishedIpv4SourceAddressSurvivesMove) {
    ASSERT_NO_FATAL_FAILURE(Configure());
    ASSERT_NO_FATAL_FAILURE(Listen(remote_));
    ASSERT_NO_FATAL_FAILURE(Connect(local_, remote_));
    ASSERT_NO_FATAL_FAILURE(Accept());
    ASSERT_NO_FATAL_FAILURE(BothDirections());
    ASSERT_NO_FATAL_FAILURE(MoveLocal());
    ASSERT_NO_FATAL_FAILURE(BothDirections());
    ASSERT_NO_FATAL_FAILURE(OrderlyClose(client_, accepted_));
}

TEST_F(TcpAddressMigration, ConcreteIpv4ListenerAcceptsAfterMove) {
    ASSERT_NO_FATAL_FAILURE(Configure());
    ASSERT_NO_FATAL_FAILURE(Listen(local_));
    ASSERT_NO_FATAL_FAILURE(MoveLocal());
    ASSERT_NO_FATAL_FAILURE(Connect(remote_, local_));
    ASSERT_NO_FATAL_FAILURE(Accept());
    ASSERT_NO_FATAL_FAILURE(BothDirections());
}

TEST_F(TcpAddressMigration, DuplicateIpv4AddressWinnerChangePreservesConnection) {
    ASSERT_NO_FATAL_FAILURE(Configure());
    ASSERT_NO_FATAL_FAILURE(Listen(local_));
    ASSERT_NO_FATAL_FAILURE(Connect(remote_, local_));
    ASSERT_NO_FATAL_FAILURE(Accept());
    ASSERT_NO_FATAL_FAILURE(BothDirections());
    // The old address stays installed until the second device also has it.
    ASSERT_NO_FATAL_FAILURE(Add(second_, local_));
    ASSERT_NO_FATAL_FAILURE(BothDirections());
    ASSERT_NO_FATAL_FAILURE(Remove(first_, local_));
    ASSERT_NO_FATAL_FAILURE(BothDirections());
    ASSERT_NO_FATAL_FAILURE(OrderlyClose(accepted_, client_));
}

TEST_F(TcpAddressMigration, TimeWaitBindProtectionSurvivesMove) {
    for (int family : {AF_INET, AF_INET6}) {
        SCOPED_TRACE(family);
        ASSERT_NO_FATAL_FAILURE(Configure(family));
        ASSERT_NO_FATAL_FAILURE(Listen(local_));
        ASSERT_NO_FATAL_FAILURE(Connect(remote_, local_));
        ASSERT_NO_FATAL_FAILURE(Accept());
        ASSERT_NO_FATAL_FAILURE(OrderlyClose(accepted_, client_));
        listener_.Reset();
        ASSERT_NO_FATAL_FAILURE(MoveLocal());
        Fd replacement;
        ASSERT_NO_FATAL_FAILURE(NewSocket(replacement));
        auto address = local_.SocketAddress(listen_port_);
        ASSERT_EQ(bind(replacement.get(), reinterpret_cast<sockaddr*>(&address), local_.Length()), -1);
        ASSERT_EQ(errno, EADDRINUSE);
        ASSERT_NO_FATAL_FAILURE(Remove(second_, local_));
        ASSERT_NO_FATAL_FAILURE(Remove(second_, remote_));
    }
}

TEST_F(TcpAddressMigration, ExplicitSameTupleReuseAfterMove) {
    ASSERT_NO_FATAL_FAILURE(Configure());
    ASSERT_NO_FATAL_FAILURE(Listen(remote_, true));
    ASSERT_NO_FATAL_FAILURE(Connect(local_, remote_, 0, true));
    uint16_t client_port = Port(client_.get()); ASSERT_NE(client_port, 0);
    ASSERT_NO_FATAL_FAILURE(Accept());
    ASSERT_NO_FATAL_FAILURE(BothDirections());
    ASSERT_NO_FATAL_FAILURE(OrderlyClose(client_, accepted_));
    ASSERT_NO_FATAL_FAILURE(MoveLocal());
    ASSERT_NO_FATAL_FAILURE(Connect(local_, remote_, client_port, true));
    ASSERT_NO_FATAL_FAILURE(Accept());
    ASSERT_NO_FATAL_FAILURE(BothDirections());
}

TEST_F(TcpAddressMigration, MovingAddressDoesNotRetargetBoundDevice) {
    ASSERT_NO_FATAL_FAILURE(Configure());
    ASSERT_NO_FATAL_FAILURE(NewSocket(listener_));
    constexpr char device[] = "veth1";
    ASSERT_EQ(setsockopt(listener_.get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)), 0);
    ASSERT_NO_FATAL_FAILURE(Bind(listener_.get(), local_));
    listen_port_ = Port(listener_.get()); ASSERT_NE(listen_port_, 0);
    ASSERT_EQ(listen(listener_.get(), 1), 0);
    ASSERT_NO_FATAL_FAILURE(MoveLocal());
    char actual[IFNAMSIZ]{}; socklen_t length = sizeof(actual);
    ASSERT_EQ(getsockopt(listener_.get(), SOL_SOCKET, SO_BINDTODEVICE, actual, &length), 0);
    ASSERT_STREQ(actual, device);
    ASSERT_NO_FATAL_FAILURE(NewSocket(client_));
    ASSERT_NO_FATAL_FAILURE(Bind(client_.get(), remote_));
    auto destination = local_.SocketAddress(listen_port_);
    int result = connect(client_.get(), reinterpret_cast<sockaddr*>(&destination), local_.Length());
    ASSERT_EQ(result, -1);
    ASSERT_TRUE(errno == EINPROGRESS || errno == ECONNREFUSED) << strerror(errno);
    pollfd descriptor{listener_.get(), POLLIN, 0};
    ASSERT_EQ(poll(&descriptor, 1, 100), 0);
}

// An unassigned synthetic IPv6 peer avoids the Linux local client's cached-dst
// invalidation race when deleting an IPv6 address. Frames genuinely cross the
// veth pair; neither a host-local source nor host TCP can complete the handshake.
class TcpAddressMigrationWire : public TcpAddressMigration {
 protected:
    Fd packet_;
    Address wire_peer_{};
    std::array<unsigned char, 6> destination_mac_{}, peer_mac_{};
    unsigned int ingress_ = 0;
    uint32_t peer_next_ = 10001, server_next_ = 0;
    std::vector<unsigned int> neighbors_;
    static uint16_t Read16(const unsigned char* p) { return (uint16_t(p[0]) << 8) | p[1]; }
    static uint32_t Read32(const unsigned char* p) { return (uint32_t(Read16(p)) << 16) | Read16(p + 2); }
    static void Write16(unsigned char* p, uint16_t n) { p[0] = n >> 8; p[1] = n; }
    static void Write32(unsigned char* p, uint32_t n) { Write16(p, n >> 16); Write16(p + 2, n); }
    static uint32_t Sum(const unsigned char* p, size_t n) {
        uint32_t sum = 0;
        for (size_t i = 0; i < n; i += 2)
            sum += (uint16_t(p[i]) << 8) | (i + 1 < n ? p[i + 1] : 0);
        return sum;
    }
    int Neighbor(unsigned int device, bool add) {
        struct { nlmsghdr header{}; ndmsg value{}; unsigned char attrs[64]{}; } request;
        request.header.nlmsg_len = NLMSG_LENGTH(sizeof(request.value));
        request.header.nlmsg_type = add ? RTM_NEWNEIGH : RTM_DELNEIGH;
        request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK |
                                     (add ? NLM_F_CREATE | NLM_F_REPLACE : 0);
        request.value.ndm_family = AF_INET6; request.value.ndm_ifindex = device;
        request.value.ndm_state = NUD_PERMANENT; request.value.ndm_type = RTN_UNICAST;
        for (int type : {NDA_DST, NDA_LLADDR}) {
            if (!add && type == NDA_LLADDR) continue;
            size_t size = type == NDA_DST ? 16 : 6;
            auto* attr = reinterpret_cast<rtattr*>(
                reinterpret_cast<char*>(&request) + NLMSG_ALIGN(request.header.nlmsg_len));
            attr->rta_type = type; attr->rta_len = RTA_LENGTH(size);
            memcpy(RTA_DATA(attr), type == NDA_DST ? wire_peer_.bytes.data() : peer_mac_.data(), size);
            request.header.nlmsg_len = NLMSG_ALIGN(request.header.nlmsg_len) + RTA_SPACE(size);
        }
        return Submit(request.header);
    }
    void TearDown() override {
        packet_.Reset();
        for (unsigned int device : neighbors_) EXPECT_EQ(Neighbor(device, false), 0) << strerror(errno);
        TcpAddressMigration::TearDown();
    }
    void ConfigureWire() {
        local_.family = wire_peer_.family = AF_INET6;
        local_.prefix = 64;
        ASSERT_EQ(inet_pton(AF_INET6, "fd11:79::1", local_.bytes.data()), 1);
        ASSERT_EQ(inet_pton(AF_INET6, "fd11:79::99", wire_peer_.bytes.data()), 1);
        ASSERT_NO_FATAL_FAILURE(Add(first_, local_));
    }
    void SelectIngress(bool first) {
        ingress_ = first ? first_ : second_;
        unsigned int injection = first ? second_ : first_;
        packet_.Reset(socket(AF_PACKET, SOCK_RAW | SOCK_NONBLOCK, htons(ETH_P_IPV6)));
        ASSERT_GE(packet_.get(), 0);
        sockaddr_ll bind_address{}; bind_address.sll_family = AF_PACKET;
        bind_address.sll_protocol = htons(ETH_P_IPV6); bind_address.sll_ifindex = injection;
        ASSERT_EQ(bind(packet_.get(), reinterpret_cast<sockaddr*>(&bind_address), sizeof(bind_address)), 0);
        ifreq request{};
        // The fixture already resolved these names to first_/second_. Avoid
        // making TCP migration depend on the unrelated reverse-name ioctl.
        strcpy(request.ifr_name, first ? "veth1" : "veth2");
        ASSERT_EQ(ioctl(packet_.get(), SIOCGIFHWADDR, &request), 0);
        memcpy(destination_mac_.data(), request.ifr_hwaddr.sa_data, 6);
        strcpy(request.ifr_name, first ? "veth2" : "veth1");
        ASSERT_EQ(ioctl(packet_.get(), SIOCGIFHWADDR, &request), 0);
        memcpy(peer_mac_.data(), request.ifr_hwaddr.sa_data, 6);
        ASSERT_EQ(Neighbor(ingress_, true), 0) << strerror(errno);
        neighbors_.push_back(ingress_);
    }
    void Send(uint8_t flags, bool data = false) {
        std::array<unsigned char, 75> frame{};
        memcpy(frame.data(), destination_mac_.data(), 6);
        memcpy(frame.data() + 6, peer_mac_.data(), 6);
        Write16(frame.data() + 12, ETH_P_IPV6);
        auto* ip = frame.data() + 14; auto* tcp = ip + 40;
        ip[0] = 0x60; Write16(ip + 4, data ? 21 : 20); ip[6] = IPPROTO_TCP; ip[7] = 64;
        memcpy(ip + 8, wire_peer_.bytes.data(), 16); memcpy(ip + 24, local_.bytes.data(), 16);
        Write16(tcp, 19779); Write16(tcp + 2, ntohs(listen_port_));
        Write32(tcp + 4, peer_next_); Write32(tcp + 8, server_next_);
        tcp[12] = 5 << 4; tcp[13] = flags; Write16(tcp + 14, 32768);
        if (data) tcp[20] = 'W';
        uint32_t sum = Sum(ip + 8, 32) + IPPROTO_TCP + (data ? 21 : 20) + Sum(tcp, data ? 21 : 20);
        while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
        Write16(tcp + 16, ~sum);
        size_t size = data ? 75 : 74;
        ASSERT_EQ(send(packet_.get(), frame.data(), size, 0), static_cast<ssize_t>(size)) << strerror(errno);
        if ((flags & 3) || data) ++peer_next_;
    }
    void Receive(bool syn, bool data, bool fin = false) {
        auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(3);
        while (std::chrono::steady_clock::now() < deadline) {
            pollfd descriptor{packet_.get(), POLLIN, 0};
            int ready = poll(&descriptor, 1, 100);
            ASSERT_GE(ready, 0); if (!ready) continue;
            unsigned char frame[2048]; ssize_t size = recv(packet_.get(), frame, sizeof(frame), 0);
            if (size < 74 || Read16(frame + 12) != ETH_P_IPV6 || frame[20] != IPPROTO_TCP) continue;
            const auto* ip = frame + 14; const auto* tcp = ip + 40;
            if (memcmp(ip + 8, local_.bytes.data(), 16) || memcmp(ip + 24, wire_peer_.bytes.data(), 16)) continue;
            if (Read16(tcp) != ntohs(listen_port_) || Read16(tcp + 2) != 19779) continue;
            ASSERT_EQ(tcp[13] & 4, 0) << "unexpected reset";
            size_t header = (tcp[12] >> 4) * 4;
            if (header < 20 || 54 + header > static_cast<size_t>(size)) continue;
            size_t segment = Read16(ip + 4);
            if (segment < header || 54 + segment > static_cast<size_t>(size)) continue;
            size_t payload = segment - header;
            if (syn && !(tcp[13] & 2)) continue;
            if (fin && !(tcp[13] & 1)) continue;
            if (data && !payload) continue;
            if (Read32(tcp + 8) != peer_next_) continue;
            server_next_ = Read32(tcp + 4) + payload + ((tcp[13] & 3) ? 1 : 0);
            if (data) { ASSERT_EQ(tcp[header], 'R'); }
            return;
        }
        FAIL() << "timed out receiving synthetic IPv6 peer response";
    }
    void Handshake() {
        ASSERT_NO_FATAL_FAILURE(Send(2));
        ASSERT_NO_FATAL_FAILURE(Receive(true, false));
        ASSERT_NO_FATAL_FAILURE(Send(16));
        ASSERT_NO_FATAL_FAILURE(Accept());
    }
    void WireExchange() {
        ASSERT_NO_FATAL_FAILURE(Send(24, true));
        ASSERT_NO_FATAL_FAILURE(Wait(accepted_.get(), POLLIN));
        char value = 0;
        ASSERT_EQ(recv(accepted_.get(), &value, 1, 0), 1); ASSERT_EQ(value, 'W');
        value = 'R'; ASSERT_EQ(send(accepted_.get(), &value, 1, MSG_NOSIGNAL), 1);
        ASSERT_NO_FATAL_FAILURE(Receive(false, true));
        ASSERT_NO_FATAL_FAILURE(Send(16));
    }
};

TEST_F(TcpAddressMigrationWire, ConcreteIpv6ListenerAcceptsAfterMove) {
    ASSERT_NO_FATAL_FAILURE(ConfigureWire());
    ASSERT_NO_FATAL_FAILURE(Listen(local_));
    ASSERT_NO_FATAL_FAILURE(MoveLocal());
    ASSERT_NO_FATAL_FAILURE(SelectIngress(false));
    ASSERT_NO_FATAL_FAILURE(Handshake());
    ASSERT_NO_FATAL_FAILURE(WireExchange());
}

TEST_F(TcpAddressMigrationWire, EstablishedIpv6SurvivesMoveWithExternalPeer) {
    ASSERT_NO_FATAL_FAILURE(ConfigureWire());
    ASSERT_NO_FATAL_FAILURE(Listen(local_));
    ASSERT_NO_FATAL_FAILURE(SelectIngress(true));
    ASSERT_NO_FATAL_FAILURE(Handshake());
    ASSERT_NO_FATAL_FAILURE(WireExchange());
    ASSERT_NO_FATAL_FAILURE(MoveLocal());
    ASSERT_NO_FATAL_FAILURE(SelectIngress(false));
    ASSERT_NO_FATAL_FAILURE(WireExchange());
}

TEST_F(TcpAddressMigrationWire, OldIpv6FinStillFindsTimeWaitAfterMove) {
    ASSERT_NO_FATAL_FAILURE(ConfigureWire());
    ASSERT_NO_FATAL_FAILURE(Listen(local_));
    ASSERT_NO_FATAL_FAILURE(SelectIngress(true));
    ASSERT_NO_FATAL_FAILURE(Handshake());
    ASSERT_NO_FATAL_FAILURE(WireExchange());
    ASSERT_EQ(shutdown(accepted_.get(), SHUT_WR), 0);
    ASSERT_NO_FATAL_FAILURE(Receive(false, false, true));
    ASSERT_NO_FATAL_FAILURE(Send(16));
    ASSERT_NO_FATAL_FAILURE(Send(17));
    ASSERT_NO_FATAL_FAILURE(Receive(false, false));
    ASSERT_NO_FATAL_FAILURE(Wait(accepted_.get(), POLLIN));
    char byte;
    ASSERT_EQ(recv(accepted_.get(), &byte, 1, 0), 0);
    accepted_.Reset(); listener_.Reset();
    ASSERT_NO_FATAL_FAILURE(MoveLocal());
    ASSERT_NO_FATAL_FAILURE(SelectIngress(false));
    // Retransmit exactly the old FIN. There is no listener or full FD left:
    // the retained TIME_WAIT state must ACK it rather than generating RST.
    --peer_next_;
    ASSERT_NO_FATAL_FAILURE(Send(17));
    ASSERT_NO_FATAL_FAILURE(Receive(false, false));
}
}  // namespace
