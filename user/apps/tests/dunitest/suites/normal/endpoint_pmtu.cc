#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <gtest/gtest.h>
#include <linux/errqueue.h>
#include <linux/ethtool.h>
#include <linux/sockios.h>
#include <linux/neighbour.h>
#include <linux/veth.h>
#include <netinet/tcp.h>
#include <poll.h>
#include <signal.h>
#include <sys/ioctl.h>
#include <sys/utsname.h>
#include <array>
#include <atomic>
#include <chrono>
#include <functional>
#include <thread>

#include "rtnetlink_route_test_support.h"

namespace {
bool DragonOS() {
    utsname name{};
    return uname(&name) == 0 &&
           (strstr(name.release, "dragonos") || strstr(name.sysname, "DragonOS"));
}
int Level(int family) { return family == AF_INET ? IPPROTO_IP : IPPROTO_IPV6; }
int Discover(int family) { return family == AF_INET ? IP_MTU_DISCOVER : IPV6_MTU_DISCOVER; }
int MtuOption(int family) { return family == AF_INET ? IP_MTU : IPV6_MTU; }
int RecvError(int family) { return family == AF_INET ? IP_RECVERR : IPV6_RECVERR; }
int IntOption(int fd, int level, int option) {
    int result = -1;
    socklen_t size = sizeof(result);
    if (getsockopt(fd, level, option, &result, &size) != 0 || size != sizeof(result)) return -1;
    return result;
}
struct Address {
    sockaddr_storage storage{};
    socklen_t size;
    Address(int family, const char* text, uint16_t port = 0) {
        if (family == AF_INET) {
            auto* a = reinterpret_cast<sockaddr_in*>(&storage);
            a->sin_family = family; a->sin_port = htons(port);
            EXPECT_EQ(inet_pton(family, text, &a->sin_addr), 1);
            size = sizeof(*a);
        } else {
            auto* a = reinterpret_cast<sockaddr_in6*>(&storage);
            a->sin6_family = family; a->sin6_port = htons(port);
            EXPECT_EQ(inet_pton(family, text, &a->sin6_addr), 1);
            size = sizeof(*a);
        }
    }
    const sockaddr* get() const { return reinterpret_cast<const sockaddr*>(&storage); }
};

class EndpointPmtuOptions : public testing::TestWithParam<std::pair<int, int>> {};
TEST_P(EndpointPmtuOptions, ModesValidateAndRoundTrip) {
    const auto [family, type] = GetParam();
    FdGuard socket_fd(socket(family, type, type == SOCK_RAW ? 253 : 0));
    ASSERT_GE(socket_fd.Get(), 0) << ErrnoString(errno);
    for (int mode = 0; mode <= 5; ++mode) {
        ASSERT_EQ(setsockopt(socket_fd.Get(), Level(family), Discover(family), &mode, sizeof(mode)), 0)
            << "family=" << family << " type=" << type << " mode=" << mode << " " << ErrnoString(errno);
        EXPECT_EQ(IntOption(socket_fd.Get(), Level(family), Discover(family)), mode);
    }
    for (int invalid : {-1, 6, 255}) {
        errno = 0;
        EXPECT_EQ(setsockopt(socket_fd.Get(), Level(family), Discover(family), &invalid, sizeof(invalid)), -1);
        EXPECT_EQ(errno, EINVAL);
        EXPECT_EQ(IntOption(socket_fd.Get(), Level(family), Discover(family)), 5);
    }
    int mtu = 0;
    socklen_t length = sizeof(mtu);
    errno = 0;
    EXPECT_EQ(getsockopt(socket_fd.Get(), Level(family), MtuOption(family), &mtu, &length), -1);
    EXPECT_EQ(errno, ENOTCONN);
}
INSTANTIATE_TEST_SUITE_P(SocketFamilies, EndpointPmtuOptions, testing::Values(
    std::make_pair(AF_INET, SOCK_DGRAM), std::make_pair(AF_INET6, SOCK_DGRAM),
    std::make_pair(AF_INET, SOCK_RAW), std::make_pair(AF_INET6, SOCK_RAW),
    std::make_pair(AF_INET, SOCK_STREAM), std::make_pair(AF_INET6, SOCK_STREAM)));

TEST(EndpointPmtu, TcpDualStackPoliciesAndPassiveOpenSnapshot) {
    FdGuard listener(socket(AF_INET6, SOCK_STREAM, 0));
    ASSERT_GE(listener.Get(), 0);
    int v4 = 2, v6 = 3;
    ASSERT_EQ(setsockopt(listener.Get(), IPPROTO_IP, IP_MTU_DISCOVER, &v4, sizeof(v4)), 0);
    ASSERT_EQ(setsockopt(listener.Get(), IPPROTO_IPV6, IPV6_MTU_DISCOVER, &v6, sizeof(v6)), 0);
    EXPECT_EQ(IntOption(listener.Get(), IPPROTO_IP, IP_MTU_DISCOVER), v4);
    EXPECT_EQ(IntOption(listener.Get(), IPPROTO_IPV6, IPV6_MTU_DISCOVER), v6);
    Address local(AF_INET6, "::1");
    ASSERT_EQ(bind(listener.Get(), local.get(), local.size), 0);
    ASSERT_EQ(listen(listener.Get(), 2), 0);
    socklen_t size = local.size;
    ASSERT_EQ(getsockname(listener.Get(), reinterpret_cast<sockaddr*>(&local.storage), &size), 0);
    FdGuard client(socket(AF_INET6, SOCK_STREAM, 0));
    ASSERT_GE(client.Get(), 0);
    timeval timeout{2, 0};
    ASSERT_EQ(setsockopt(client.Get(), SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)), 0);
    ASSERT_EQ(connect(client.Get(), local.get(), local.size), 0) << ErrnoString(errno);
    // The child already exists. Listener updates must apply to future opens,
    // without rewriting either option snapshot of this pending child.
    int updated = 5;
    ASSERT_EQ(setsockopt(listener.Get(), IPPROTO_IP, IP_MTU_DISCOVER, &updated, sizeof(updated)), 0);
    ASSERT_EQ(setsockopt(listener.Get(), IPPROTO_IPV6, IPV6_MTU_DISCOVER, &updated, sizeof(updated)), 0);
    FdGuard accepted(accept(listener.Get(), nullptr, nullptr));
    ASSERT_GE(accepted.Get(), 0);
    EXPECT_EQ(IntOption(accepted.Get(), IPPROTO_IP, IP_MTU_DISCOVER), v4);
    EXPECT_EQ(IntOption(accepted.Get(), IPPROTO_IPV6, IPV6_MTU_DISCOVER), v6);
    EXPECT_GT(IntOption(accepted.Get(), IPPROTO_IPV6, IPV6_MTU), 0);
    tcp_info info{};
    size = sizeof(info);
    ASSERT_EQ(getsockopt(accepted.Get(), IPPROTO_TCP, TCP_INFO, &info, &size), 0);
    EXPECT_GT(info.tcpi_pmtu, 0u);
    std::vector<uint8_t> sent(4096, 0x57), received(sent.size());
    ASSERT_EQ(send(client.Get(), sent.data(), sent.size(), 0), static_cast<ssize_t>(sent.size()));
    ASSERT_EQ(setsockopt(accepted.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
    size_t consumed = 0;
    while (consumed < received.size()) {
        ssize_t n = recv(accepted.Get(), received.data() + consumed, received.size() - consumed, 0);
        ASSERT_GT(n, 0) << ErrnoString(errno);
        consumed += n;
    }
    EXPECT_EQ(received, sent);
}

// Every packet-injection case runs in its own child netns. Neither existing
// interfaces nor the host PMTU cache, routes, or nftables rules are touched.
struct LinkRequest {
    nlmsghdr header{};
    std::array<uint8_t, 768> body{};
    LinkRequest(uint16_t type, size_t payload, uint32_t sequence, uint16_t flags = 0) {
        header.nlmsg_len = NLMSG_LENGTH(payload);
        header.nlmsg_type = type;
        header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
        header.nlmsg_seq = sequence;
    }
    void attr(uint16_t type, const void* value, size_t size) {
        AddAttr(&header, sizeof(*this), type, value, size);
    }
    int submit(int fd) {
        if (send(fd, this, header.nlmsg_len, 0) != static_cast<ssize_t>(header.nlmsg_len)) return errno;
        return RecvAck(fd, header.nlmsg_seq);
    }
};
int CreatePair(int fd) {
    LinkRequest request(RTM_NEWLINK, sizeof(ifinfomsg), 1, NLM_F_CREATE | NLM_F_EXCL);
    const char own[] = "pmtu-a", peer[] = "pmtu-b", kind[] = "veth";
    request.attr(IFLA_IFNAME, own, sizeof(own));
    // Nested LINKINFO/DATA/PEER; peer starts with its ifinfomsg.
    const size_t info_start = NLMSG_ALIGN(request.header.nlmsg_len);
    request.attr(IFLA_LINKINFO | NLA_F_NESTED, nullptr, 0);
    request.attr(IFLA_INFO_KIND, kind, sizeof(kind));
    const size_t data_start = NLMSG_ALIGN(request.header.nlmsg_len);
    request.attr(IFLA_INFO_DATA | NLA_F_NESTED, nullptr, 0);
    const size_t peer_start = NLMSG_ALIGN(request.header.nlmsg_len);
    ifinfomsg peer_info{};
    request.attr(VETH_INFO_PEER | NLA_F_NESTED, &peer_info, sizeof(peer_info));
    request.attr(IFLA_IFNAME, peer, sizeof(peer));
    auto* bytes = reinterpret_cast<uint8_t*>(&request);
    for (size_t start : {peer_start, data_start, info_start})
        reinterpret_cast<rtattr*>(bytes + start)->rta_len = request.header.nlmsg_len - start;
    return request.submit(fd);
}
void Put16(uint8_t* bytes, uint16_t value) { bytes[0] = value >> 8; bytes[1] = value; }
void Put32(uint8_t* bytes, uint32_t value) { Put16(bytes, value >> 16); Put16(bytes + 2, value); }
uint16_t Get16(const uint8_t* bytes) { return uint16_t(bytes[0]) << 8 | bytes[1]; }
uint32_t Get32(const uint8_t* bytes) { return uint32_t(Get16(bytes)) << 16 | Get16(bytes + 2); }
uint32_t Sum(const uint8_t* bytes, size_t size) {
    uint32_t result = 0;
    for (size_t i = 0; i < size; i += 2) result += uint16_t(bytes[i]) << 8 | (i + 1 < size ? bytes[i + 1] : 0);
    return result;
}
uint16_t FinishChecksum(uint32_t sum) {
    while (sum >> 16) sum = (sum & 0xffff) + (sum >> 16);
    return ~sum;
}

class EndpointPath {
public:
    explicit EndpointPath(int family) : family_(family), own_address_(family, family == AF_INET ? "198.18.57.1" : "fd57::1"),
        peer_address_(family, family == AF_INET ? "198.18.57.2" : "fd57::2", 19857) {}
    int Setup() {
        netlink_.Reset(OpenRouteSocket());
        if (netlink_.Get() < 0) return errno;
        timeval timeout{2, 0};
        if (setsockopt(netlink_.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout))) return errno;
        if (int error = CreatePair(netlink_.Get())) return error;
        own_ = if_nametoindex("pmtu-a"); peer_ = if_nametoindex("pmtu-b");
        if (!own_ || !peer_) return ENODEV;
        for (unsigned index : {own_, peer_}) {
            LinkRequest link(RTM_SETLINK, sizeof(ifinfomsg), ++sequence_);
            auto* msg = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(&link.header));
            msg->ifi_index = index; msg->ifi_flags = IFF_UP; msg->ifi_change = IFF_UP;
            uint32_t mtu = 1500;
            link.attr(IFLA_MTU, &mtu, sizeof(mtu));
            if (int error = link.submit(netlink_.Get())) return error;
        }
        int error = family_ == AF_INET ?
            SendAddrRequest(netlink_.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                            own_, "198.18.57.1", 24, ++sequence_) :
            SendIpv6AddrRequest(netlink_.Get(), RTM_NEWADDR, NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                own_, "fd57::1", 64, ++sequence_);
        if (error) return error;
        FdGuard ioctl_fd(socket(AF_INET, SOCK_DGRAM, 0));
        if (ioctl_fd.Get() < 0) return errno;
        ifreq request{};
        // AF_PACKET sees Linux veth GSO skbs before segmentation. Disable
        // offloads so packet-length assertions measure actual IP segments.
        // DragonOS has no GSO/TSO implementation or ethtool setter.
        if (!DragonOS()) {
            for (const char* name : {"pmtu-a", "pmtu-b"}) {
                strcpy(request.ifr_name, name);
                for (uint32_t command : {ETHTOOL_STSO, ETHTOOL_SGSO, ETHTOOL_SGRO}) {
                    ethtool_value value{command, 0};
                    request.ifr_data = reinterpret_cast<char*>(&value);
                    if (ioctl(ioctl_fd.Get(), SIOCETHTOOL, &request)) return errno;
                }
            }
        }
        strcpy(request.ifr_name, "pmtu-a");
        if (ioctl(ioctl_fd.Get(), SIOCGIFHWADDR, &request)) return errno;
        memcpy(own_mac_.data(), request.ifr_hwaddr.sa_data, 6);
        strcpy(request.ifr_name, "pmtu-b");
        if (ioctl(ioctl_fd.Get(), SIOCGIFHWADDR, &request)) return errno;
        std::array<uint8_t, 6> peer_mac{};
        memcpy(peer_mac.data(), request.ifr_hwaddr.sa_data, 6);
        LinkRequest neighbor(RTM_NEWNEIGH, sizeof(ndmsg), ++sequence_, NLM_F_CREATE | NLM_F_REPLACE);
        auto* msg = reinterpret_cast<ndmsg*>(NLMSG_DATA(&neighbor.header));
        msg->ndm_family = family_; msg->ndm_ifindex = own_; msg->ndm_state = NUD_PERMANENT;
        if (family_ == AF_INET) neighbor.attr(NDA_DST, &reinterpret_cast<const sockaddr_in*>(peer_address_.get())->sin_addr, 4);
        else neighbor.attr(NDA_DST, &reinterpret_cast<const sockaddr_in6*>(peer_address_.get())->sin6_addr, 16);
        neighbor.attr(NDA_LLADDR, peer_mac.data(), peer_mac.size());
        if ((error = neighbor.submit(netlink_.Get()))) return error;
        packet_.Reset(socket(AF_PACKET, SOCK_DGRAM | SOCK_NONBLOCK, htons(family_ == AF_INET ? ETH_P_IP : ETH_P_IPV6)));
        if (packet_.Get() < 0) return errno;
        sockaddr_ll link{};
        link.sll_family = AF_PACKET; link.sll_ifindex = peer_;
        link.sll_protocol = htons(family_ == AF_INET ? ETH_P_IP : ETH_P_IPV6);
        if (bind(packet_.Get(), reinterpret_cast<sockaddr*>(&link), sizeof(link))) return errno;
        return 0;
    }
    int Open(int type) {
        int fd = socket(family_, type, type == SOCK_RAW ? 253 : 0);
        if (fd < 0) return -1;
        if (bind(fd, own_address_.get(), own_address_.size) ||
            connect(fd, peer_address_.get(), peer_address_.size)) {
            const int saved = errno; close(fd); errno = saved; return -1;
        }
        return fd;
    }
    int OpenTcp() {
        int fd = socket(family_, SOCK_STREAM | SOCK_NONBLOCK, 0);
        if (fd < 0) return -1;
        if (bind(fd, own_address_.get(), own_address_.size) ||
            (connect(fd, peer_address_.get(), peer_address_.size) && errno != EINPROGRESS)) {
            const int saved = errno; close(fd); errno = saved; return -1;
        }
        return fd;
    }
    std::vector<uint8_t> Capture(uint8_t protocol) {
        const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(2);
        std::array<uint8_t, 4096> bytes{};
        while (std::chrono::steady_clock::now() < deadline) {
            pollfd p{packet_.Get(), POLLIN, 0};
            if (poll(&p, 1, 20) < 0 && errno != EINTR) break;
            ssize_t size = recv(packet_.Get(), bytes.data(), bytes.size(), MSG_DONTWAIT);
            if (size < 0) continue;
            const size_t header = family_ == AF_INET ? 20 : 40;
            if (size >= static_cast<ssize_t>(header) && bytes[family_ == AF_INET ? 9 : 6] == protocol)
                return std::vector<uint8_t>(bytes.begin(), bytes.begin() + size);
        }
        return {};
    }
    int InjectError(const std::vector<uint8_t>& original, bool bad_checksum = false) {
        const size_t header = family_ == AF_INET ? 20 : 40;
        const size_t quote = std::min<size_t>(original.size(), 256);
        std::vector<uint8_t> bytes(header + 8 + quote, 0);
        memcpy(bytes.data() + header + 8, original.data(), quote);
        uint8_t* icmp = bytes.data() + header;
        if (family_ == AF_INET) {
            bytes[0] = 0x45; Put16(bytes.data() + 2, bytes.size()); bytes[8] = 64; bytes[9] = IPPROTO_ICMP;
            inet_pton(AF_INET, "198.18.57.254", bytes.data() + 12);
            memcpy(bytes.data() + 16, original.data() + 12, 4);
            icmp[0] = 3; icmp[1] = 4; Put16(icmp + 6, 1280);
            Put16(icmp + 2, FinishChecksum(Sum(icmp, 8 + quote)));
            Put16(bytes.data() + 10, FinishChecksum(Sum(bytes.data(), header)));
        } else {
            bytes[0] = 0x60; Put16(bytes.data() + 4, 8 + quote); bytes[6] = IPPROTO_ICMPV6; bytes[7] = 64;
            inet_pton(AF_INET6, "fd57::fe", bytes.data() + 8);
            memcpy(bytes.data() + 24, original.data() + 8, 16);
            icmp[0] = 2; Put32(icmp + 4, 1280);
            Put16(icmp + 2, FinishChecksum(Sum(bytes.data() + 8, 32) + (8 + quote) + IPPROTO_ICMPV6 + Sum(icmp, 8 + quote)));
        }
        if (bad_checksum) icmp[2] ^= 1;
        return Inject(bytes);
    }
    int ReplyTcp(const std::vector<uint8_t>& original, uint32_t sequence, uint32_t ack, uint8_t flags) {
        const size_t header = family_ == AF_INET ? 20 : 40;
        const size_t tcp_size = flags & 2 ? 24 : 20;
        std::vector<uint8_t> bytes(header + tcp_size, 0);
        auto* tcp = bytes.data() + header;
        memcpy(tcp, original.data() + header + 2, 2);
        memcpy(tcp + 2, original.data() + header, 2);
        Put32(tcp + 4, sequence); Put32(tcp + 8, ack);
        tcp[12] = (tcp_size / 4) << 4; tcp[13] = flags; Put16(tcp + 14, 65535);
        if (flags & 2) { tcp[20] = 2; tcp[21] = 4; Put16(tcp + 22, 1500 - header - 20); }
        uint32_t pseudo;
        if (family_ == AF_INET) {
            bytes[0] = 0x45; Put16(bytes.data() + 2, bytes.size()); bytes[8] = 64; bytes[9] = IPPROTO_TCP;
            memcpy(bytes.data() + 12, original.data() + 16, 4);
            memcpy(bytes.data() + 16, original.data() + 12, 4);
            pseudo = Sum(bytes.data() + 12, 8) + IPPROTO_TCP + tcp_size;
            Put16(bytes.data() + 10, FinishChecksum(Sum(bytes.data(), header)));
        } else {
            bytes[0] = 0x60; Put16(bytes.data() + 4, tcp_size); bytes[6] = IPPROTO_TCP; bytes[7] = 64;
            memcpy(bytes.data() + 8, original.data() + 24, 16);
            memcpy(bytes.data() + 24, original.data() + 8, 16);
            pseudo = Sum(bytes.data() + 8, 32) + IPPROTO_TCP + tcp_size;
        }
        Put16(tcp + 16, FinishChecksum(pseudo + Sum(tcp, tcp_size)));
        return Inject(bytes);
    }
    int Inject(const std::vector<uint8_t>& bytes) {
        sockaddr_ll destination{};
        destination.sll_family = AF_PACKET; destination.sll_ifindex = peer_;
        destination.sll_protocol = htons(family_ == AF_INET ? ETH_P_IP : ETH_P_IPV6);
        destination.sll_halen = 6; memcpy(destination.sll_addr, own_mac_.data(), 6);
        return sendto(packet_.Get(), bytes.data(), bytes.size(), 0,
                      reinterpret_cast<sockaddr*>(&destination), sizeof(destination)) == static_cast<ssize_t>(bytes.size()) ? 0 : errno;
    }
    int family() const { return family_; }
    const Address& peer() const { return peer_address_; }
    int RemovePeerNeighbor() {
        LinkRequest request(RTM_DELNEIGH, sizeof(ndmsg), ++sequence_);
        auto* msg = reinterpret_cast<ndmsg*>(NLMSG_DATA(&request.header));
        msg->ndm_family = family_; msg->ndm_ifindex = own_;
        if (family_ == AF_INET)
            request.attr(NDA_DST, &reinterpret_cast<const sockaddr_in*>(peer_address_.get())->sin_addr, 4);
        else
            request.attr(NDA_DST, &reinterpret_cast<const sockaddr_in6*>(peer_address_.get())->sin6_addr, 16);
        return request.submit(netlink_.Get());
    }
private:
    int family_;
    Address own_address_, peer_address_;
    unsigned own_ = 0, peer_ = 0, sequence_ = 1;
    std::array<uint8_t, 6> own_mac_{};
    FdGuard netlink_, packet_;
};

void CheckError(int fd, int family, int type, uint8_t origin, size_t data_size, size_t control_size,
                bool expect_payload, bool expect_ctrunc = false) {
    std::array<uint8_t, 512> data{};
    alignas(cmsghdr) std::array<uint8_t, 256> control{};
    sockaddr_storage destination{};
    iovec io{data.data(), data_size};
    msghdr message{};
    message.msg_name = &destination; message.msg_namelen = sizeof(destination);
    message.msg_iov = &io; message.msg_iovlen = 1;
    message.msg_control = control.data(); message.msg_controllen = control_size;
    pollfd p{fd, POLLERR, 0};
    ASSERT_EQ(poll(&p, 1, 2000), 1);
    ssize_t size = recvmsg(fd, &message, MSG_ERRQUEUE | MSG_DONTWAIT);
    ASSERT_GE(size, 0) << ErrnoString(errno);
    EXPECT_NE(message.msg_flags & MSG_ERRQUEUE, 0);
    EXPECT_EQ((message.msg_flags & MSG_CTRUNC) != 0, expect_ctrunc);
    EXPECT_EQ(destination.ss_family, family);
    // Linux raw IPv4 preserves connect's sin_port for LOCAL errors, but
    // ip_icmp_error passes zero for remote raw errors; IPv6 raw has no port.
    const bool has_port = type == SOCK_DGRAM || (family == AF_INET && origin == SO_EE_ORIGIN_LOCAL);
    Address expected(family, family == AF_INET ? "198.18.57.2" : "fd57::2", has_port ? 19857 : 0);
    EXPECT_EQ(message.msg_namelen, expected.size);
    if (family == AF_INET) {
        auto* actual = reinterpret_cast<const sockaddr_in*>(&destination);
        auto* wanted = reinterpret_cast<const sockaddr_in*>(expected.get());
        EXPECT_EQ(actual->sin_addr.s_addr, wanted->sin_addr.s_addr);
        EXPECT_EQ(actual->sin_port, wanted->sin_port);
    } else {
        auto* actual = reinterpret_cast<const sockaddr_in6*>(&destination);
        auto* wanted = reinterpret_cast<const sockaddr_in6*>(expected.get());
        EXPECT_EQ(memcmp(&actual->sin6_addr, &wanted->sin6_addr, sizeof(in6_addr)), 0);
        EXPECT_EQ(actual->sin6_port, wanted->sin6_port);
    }
    if (expect_payload) {
        EXPECT_EQ(size, static_cast<ssize_t>(data_size));
        EXPECT_NE(message.msg_flags & MSG_TRUNC, 0);
        for (size_t i = 0; i < data_size; ++i) EXPECT_EQ(data[i], 0x57);
    } else EXPECT_EQ(size, 0);
    if (expect_ctrunc) return;
    const cmsghdr* cmsg = CMSG_FIRSTHDR(&message);
    ASSERT_NE(cmsg, nullptr);
    EXPECT_EQ(cmsg->cmsg_level, Level(family));
    EXPECT_EQ(cmsg->cmsg_type, RecvError(family));
    ASSERT_GE(cmsg->cmsg_len, CMSG_LEN(sizeof(sock_extended_err)));
    sock_extended_err error{};
    memcpy(&error, CMSG_DATA(cmsg), sizeof(error));
    EXPECT_EQ(error.ee_errno, static_cast<uint32_t>(EMSGSIZE));
    EXPECT_EQ(error.ee_origin, origin);
    EXPECT_EQ(error.ee_info, 1280u);
    EXPECT_EQ(error.ee_pad, 0);
    EXPECT_EQ(error.ee_data, 0u);
    if (origin != SO_EE_ORIGIN_LOCAL) {
        EXPECT_EQ(error.ee_type, family == AF_INET ? 3 : 2);
        EXPECT_EQ(error.ee_code, family == AF_INET ? 4 : 0);
        ASSERT_GE(cmsg->cmsg_len, CMSG_LEN(sizeof(error) + (family == AF_INET ? sizeof(sockaddr_in) : sizeof(sockaddr_in6))));
        const auto* offender = reinterpret_cast<const sockaddr*>(CMSG_DATA(cmsg) + sizeof(error));
        EXPECT_EQ(offender->sa_family, family);
        Address router(family, family == AF_INET ? "198.18.57.254" : "fd57::fe");
        if (family == AF_INET) {
            EXPECT_EQ(reinterpret_cast<const sockaddr_in*>(offender)->sin_addr.s_addr,
                      reinterpret_cast<const sockaddr_in*>(router.get())->sin_addr.s_addr);
        } else {
            EXPECT_EQ(memcmp(&reinterpret_cast<const sockaddr_in6*>(offender)->sin6_addr,
                             &reinterpret_cast<const sockaddr_in6*>(router.get())->sin6_addr, sizeof(in6_addr)), 0);
        }
    }
}

void TcpFeedbackScenario(int family) {
    EndpointPath path(family);
    ASSERT_EQ(path.Setup(), 0);
    FdGuard sender(path.OpenTcp());
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);
    const size_t header = family == AF_INET ? 20 : 40;
    const auto syn = path.Capture(IPPROTO_TCP);
    ASSERT_GE(syn.size(), header + 20);
    ASSERT_NE(syn[header + 13] & 2, 0);
    const uint32_t start = Get32(syn.data() + header + 4) + 1;
    constexpr uint32_t peer_seq = 57000;
    ASSERT_EQ(path.ReplyTcp(syn, peer_seq, start, 0x12), 0);
    pollfd ready{sender.Get(), POLLOUT, 0};
    ASSERT_EQ(poll(&ready, 1, 2000), 1);
    ASSERT_EQ(IntOption(sender.Get(), SOL_SOCKET, SO_ERROR), 0);
    std::vector<uint8_t> payload(1400, 0x57);
    ASSERT_EQ(send(sender.Get(), payload.data(), payload.size(), 0), static_cast<ssize_t>(payload.size()));
    std::vector<uint8_t> original;
    for (int i = 0; i < 4; ++i) {
        auto packet = path.Capture(IPPROTO_TCP);
        ASSERT_GE(packet.size(), header + 20);
        if (packet.size() > header + ((packet[header + 12] >> 4) * 4u)) {
            original = std::move(packet); break;
        }
    }
    ASSERT_GT(original.size(), 1280u);
    ASSERT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)), 1500);
    // An unsent sequence must not update the route or trigger retransmission.
    auto forged = original;
    Put32(forged.data() + header + 4, start + 0x100000);
    ASSERT_EQ(path.InjectError(forged), 0);
    usleep(30000);
    EXPECT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)), 1500);
    ASSERT_EQ(path.InjectError(original), 0);
    uint32_t acknowledged = start;
    while (acknowledged != start + payload.size()) {
        const auto packet = path.Capture(IPPROTO_TCP);
        ASSERT_GE(packet.size(), header + 20);
        const size_t tcp_header = (packet[header + 12] >> 4) * 4u;
        ASSERT_GE(packet.size(), header + tcp_header);
        const size_t data_size = packet.size() - header - tcp_header;
        if (!data_size) continue;
        EXPECT_LE(packet.size(), 1280u);
        ASSERT_EQ(Get32(packet.data() + header + 4), acknowledged);
        ASSERT_LE(data_size, start + payload.size() - acknowledged);
        for (size_t i = header + tcp_header; i < packet.size(); ++i) EXPECT_EQ(packet[i], 0x57);
        acknowledged += data_size;
        ASSERT_EQ(path.ReplyTcp(packet, peer_seq + 1, acknowledged, 0x10), 0);
    }
    EXPECT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)), 1280);
    tcp_info info{};
    socklen_t size = sizeof(info);
    ASSERT_EQ(getsockopt(sender.Get(), IPPROTO_TCP, TCP_INFO, &info, &size), 0);
    EXPECT_EQ(info.tcpi_pmtu, 1280u);
    EXPECT_EQ(IntOption(sender.Get(), SOL_SOCKET, SO_ERROR), 0);
}

void FeedbackScenario(int family, int type) {
    EndpointPath path(family);
    ASSERT_EQ(path.Setup(), 0) << ErrnoString(errno);
    FdGuard sender(path.Open(type));
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);
    int on = 1, mode = 2;
    ASSERT_EQ(setsockopt(sender.Get(), Level(family), RecvError(family), &on, sizeof(on)), 0);
    ASSERT_EQ(setsockopt(sender.Get(), Level(family), Discover(family), &mode, sizeof(mode)), 0);
    ASSERT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)), 1500);
    const uint8_t protocol = type == SOCK_RAW ? 253 : IPPROTO_UDP;
    std::vector<uint8_t> large(1400, 0x57);
    ASSERT_EQ(send(sender.Get(), large.data(), large.size(), 0), static_cast<ssize_t>(large.size()));
    const auto first = path.Capture(protocol);
    ASSERT_GT(first.size(), 1280u);
    ASSERT_EQ(path.InjectError(first, true), 0);
    usleep(30000);
    EXPECT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)), 1500);
    ASSERT_EQ(path.InjectError(first), 0);
    pollfd ready{sender.Get(), POLLERR, 0};
    ASSERT_EQ(poll(&ready, 1, 2000), 1);
    EXPECT_NE(ready.revents & POLLERR, 0);
    ASSERT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)), 1280);
    EXPECT_EQ(IntOption(sender.Get(), SOL_SOCKET, SO_ERROR), EMSGSIZE);
    EXPECT_EQ(IntOption(sender.Get(), SOL_SOCKET, SO_ERROR), 0);
    CheckError(sender.Get(), family, type, family == AF_INET ? SO_EE_ORIGIN_ICMP : SO_EE_ORIGIN_ICMP6, 8, 256, true);
    ASSERT_EQ(path.InjectError(first), 0);
    CheckError(sender.Get(), family, type, family == AF_INET ? SO_EE_ORIGIN_ICMP : SO_EE_ORIGIN_ICMP6, 8, 1, true, true);
    errno = 0;
    ASSERT_EQ(send(sender.Get(), large.data(), large.size(), 0), -1);
    ASSERT_EQ(errno, EMSGSIZE);
    // LOCAL failures are already synchronous: queuing their extended error
    // must not poison SO_ERROR or the next operation with the same errno.
    EXPECT_EQ(IntOption(sender.Get(), SOL_SOCKET, SO_ERROR), 0);
    CheckError(sender.Get(), family, type, SO_EE_ORIGIN_LOCAL, 0, 256, false);
    std::vector<uint8_t> small(1200, 0x57);
    ASSERT_EQ(send(sender.Get(), small.data(), small.size(), 0), static_cast<ssize_t>(small.size()));
    auto reduced = path.Capture(protocol);
    ASSERT_FALSE(reduced.empty());
    EXPECT_LE(reduced.size(), 1280u);
    FdGuard shared(path.Open(type));
    ASSERT_GE(shared.Get(), 0);
    EXPECT_EQ(IntOption(shared.Get(), Level(family), MtuOption(family)), 1280);
    // PROBE ignores the cached send limit, while MTU query still reports dst MTU.
    mode = 3;
    ASSERT_EQ(setsockopt(sender.Get(), Level(family), Discover(family), &mode, sizeof(mode)), 0);
    ASSERT_EQ(send(sender.Get(), large.data(), large.size(), 0), static_cast<ssize_t>(large.size()));
    auto probe = path.Capture(protocol);
    ASSERT_FALSE(probe.empty());
    EXPECT_GT(probe.size(), 1280u);
    EXPECT_LE(probe.size(), 1500u);
}

enum class RawSendCall { Send, SendTo, SendMsg };
void MalformedFeedbackScenario(int family) {
    EndpointPath path(family);
    ASSERT_EQ(path.Setup(), 0);
    FdGuard sender(path.Open(SOCK_DGRAM));
    ASSERT_GE(sender.Get(), 0);
    int mode = 2, on = 1;
    ASSERT_EQ(setsockopt(sender.Get(), Level(family), Discover(family), &mode, sizeof(mode)), 0);
    ASSERT_EQ(setsockopt(sender.Get(), Level(family), RecvError(family), &on, sizeof(on)), 0);
    std::vector<uint8_t> payload(1400, 0x57);
    ASSERT_EQ(send(sender.Get(), payload.data(), payload.size(), 0), static_cast<ssize_t>(payload.size()));
    const auto original = path.Capture(IPPROTO_UDP);
    const size_t header = family == AF_INET ? 20 : 40;
    ASSERT_GT(original.size(), header + 8);
    auto short_quote = original;
    short_quote.resize(header + 7);
    ASSERT_EQ(path.InjectError(short_quote), 0);
    auto invalid_header = original;
    if (family == AF_INET) invalid_header[0] = 0x44;  // IHL shorter than the fixed header.
    else {
        invalid_header[6] = IPPROTO_DSTOPTS;
        invalid_header[header + 1] = 255;  // Extension length exceeds the quote.
    }
    ASSERT_EQ(path.InjectError(invalid_header), 0);
    if (family == AF_INET6) {
        auto nonfirst_fragment = original;
        nonfirst_fragment.insert(nonfirst_fragment.begin() + header, 8, 0);
        nonfirst_fragment[6] = IPPROTO_FRAGMENT;
        nonfirst_fragment[header] = IPPROTO_UDP;
        Put16(nonfirst_fragment.data() + header + 2, 8);
        ASSERT_EQ(path.InjectError(nonfirst_fragment), 0);
    }
    auto wrong_port = original;
    Put16(wrong_port.data() + header, 19858);
    ASSERT_EQ(path.InjectError(wrong_port), 0);
    pollfd error{sender.Get(), POLLERR, 0};
    EXPECT_EQ(poll(&error, 1, 100), 0);
    EXPECT_EQ(IntOption(sender.Get(), SOL_SOCKET, SO_ERROR), 0);
    EXPECT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)), 1500);
    // A genuinely truncated ICMP quote (not its advertised IP packet length)
    // still contains the complete transport prefix and must be accepted.
    auto valid_quote = original;
    valid_quote.resize(header + 8);
    ASSERT_EQ(path.InjectError(valid_quote), 0);
    ASSERT_EQ(poll(&error, 1, 2000), 1);
    EXPECT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)), 1280);
}

void RawBlockedSendFeedbackScenario(RawSendCall call) {
    EndpointPath path(AF_INET);
    ASSERT_EQ(path.Setup(), 0) << ErrnoString(errno);
    FdGuard sender(path.Open(SOCK_RAW));
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);
    int mode = IP_PMTUDISC_DO;
    ASSERT_EQ(setsockopt(sender.Get(), IPPROTO_IP, IP_MTU_DISCOVER, &mode, sizeof(mode)), 0);
    std::vector<uint8_t> payload(1200, 0x57);
    ASSERT_EQ(send(sender.Get(), payload.data(), payload.size(), 0), static_cast<ssize_t>(payload.size()));
    auto packet = path.Capture(253);
    ASSERT_EQ(packet.size(), 1220u);
    // HDRINCL makes Linux use sock_alloc_send_skb, which waits for socket
    // send memory. Non-HDRINCL raw instead returns ENOBUFS from ip_append_data.
    // DragonOS accounts the same complete datagrams in its prepared queue.
    int hdrincl = 1, sndbuf = 4096;
    ASSERT_EQ(setsockopt(sender.Get(), IPPROTO_IP, IP_HDRINCL, &hdrincl, sizeof(hdrincl)), 0);
    ASSERT_EQ(setsockopt(sender.Get(), SOL_SOCKET, SO_SNDBUF, &sndbuf, sizeof(sndbuf)), 0);
    timeval timeout{2, 0};
    ASSERT_EQ(setsockopt(sender.Get(), SOL_SOCKET, SO_SNDTIMEO, &timeout, sizeof(timeout)), 0);
    ASSERT_EQ(path.RemovePeerNeighbor(), 0) << ErrnoString(errno);
    // There is no address on pmtu-b to answer ARP. Observe actual admission
    // exhaustion, without assuming identical skb/datagram accounting sizes.
    bool exhausted = false;
    for (int i = 0; i < 64; ++i) {
        const ssize_t n = send(sender.Get(), packet.data(), packet.size(), MSG_DONTWAIT);
        if (n < 0) {
            ASSERT_EQ(errno, EAGAIN) << ErrnoString(errno);
            exhausted = true;
            break;
        }
        ASSERT_EQ(n, static_cast<ssize_t>(packet.size()));
    }
    ASSERT_TRUE(exhausted) << "unresolved-neighbor output did not exhaust socket send memory";
    std::atomic<bool> started{false}, done{false};
    ssize_t result = 0;
    int send_error = 0;
    std::chrono::steady_clock::time_point completed;
    std::thread blocked([&] {
        started.store(true, std::memory_order_release);
        if (call == RawSendCall::Send) {
            result = send(sender.Get(), packet.data(), packet.size(), 0);
        } else if (call == RawSendCall::SendTo) {
            result = sendto(sender.Get(), packet.data(), packet.size(), 0, path.peer().get(), path.peer().size);
        } else {
            iovec io{packet.data(), packet.size()};
            msghdr message{};
            message.msg_name = const_cast<sockaddr*>(path.peer().get());
            message.msg_namelen = path.peer().size;
            message.msg_iov = &io; message.msg_iovlen = 1;
            result = sendmsg(sender.Get(), &message, 0);
        }
        send_error = errno;
        completed = std::chrono::steady_clock::now();
        done.store(true, std::memory_order_release);
    });
    while (!started.load(std::memory_order_acquire)) std::this_thread::yield();
    usleep(100000);
    const bool was_blocked = !done.load(std::memory_order_acquire);
    const auto injected = std::chrono::steady_clock::now();
    const int inject_error = path.InjectError(packet);
    // Join before assertions so a failure cannot destroy a joinable thread.
    // SO_SNDTIMEO bounds even the pre-fix missed-error wakeup.
    blocked.join();
    ASSERT_TRUE(was_blocked) << "send returned before PMTU feedback";
    ASSERT_EQ(inject_error, 0) << ErrnoString(inject_error);
    EXPECT_EQ(result, -1);
    EXPECT_EQ(send_error, EMSGSIZE);
    EXPECT_LT(completed - injected, std::chrono::seconds(1));
}

void FeedbackIgnoredScenario(int family, int type) {
    EndpointPath path(family);
    ASSERT_EQ(path.Setup(), 0);
    FdGuard sender(path.Open(type));
    ASSERT_GE(sender.Get(), 0);
    int on = 1;
    ASSERT_EQ(setsockopt(sender.Get(), Level(family), RecvError(family), &on, sizeof(on)), 0);
    const uint8_t protocol = type == SOCK_RAW ? 253 : IPPROTO_UDP;
    std::vector<uint8_t> payload(1400, 0x57);
    // IPv4 INTERFACE/OMIT report errors without learning. Linux UDPv6 drops
    // PTB entirely (ip6_sk_accept_pmtu), whereas rawv6_error updates dst PMTU
    // without that gate and still reports it. Sending uses interface MTU.
    for (int mode : {4, 5}) {
        ASSERT_EQ(setsockopt(sender.Get(), Level(family), Discover(family), &mode, sizeof(mode)), 0);
        ASSERT_EQ(send(sender.Get(), payload.data(), payload.size(), 0), static_cast<ssize_t>(payload.size()));
        const auto packet = path.Capture(protocol);
        ASSERT_GT(packet.size(), 1280u);
        ASSERT_EQ(path.InjectError(packet), 0);
        if (family == AF_INET6 && type == SOCK_DGRAM) {
            pollfd error{sender.Get(), POLLERR, 0};
            EXPECT_EQ(poll(&error, 1, 100), 0);
            EXPECT_EQ(IntOption(sender.Get(), SOL_SOCKET, SO_ERROR), 0);
        } else {
            CheckError(sender.Get(), family, type, family == AF_INET ? SO_EE_ORIGIN_ICMP : SO_EE_ORIGIN_ICMP6,
                       8, 256, true);
        }
        EXPECT_EQ(IntOption(sender.Get(), Level(family), MtuOption(family)),
                  family == AF_INET6 && type == SOCK_RAW ? 1280 : 1500);
        ASSERT_EQ(send(sender.Get(), payload.data(), payload.size(), 0), static_cast<ssize_t>(payload.size()));
        const auto unchanged = path.Capture(protocol);
        EXPECT_GT(unchanged.size(), 1280u);
        EXPECT_LE(unchanged.size(), 1500u);
    }
}

void Isolated(const std::function<void()>& body) {
    int pipe_fds[2];
    ASSERT_EQ(pipe(pipe_fds), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (!child) {
        close(pipe_fds[0]);
        int result;
        if (unshare(CLONE_NEWUSER | CLONE_NEWNET)) result = -errno;
        else { body(); result = testing::Test::HasFailure() ? 1 : 0; }
        const ssize_t written = write(pipe_fds[1], &result, sizeof(result));
        (void)written;
        _exit(result == 0 ? 0 : 1);
    }
    close(pipe_fds[1]);
    int status = 0;
    const auto deadline = std::chrono::steady_clock::now() + std::chrono::seconds(15);
    while (waitpid(child, &status, WNOHANG) == 0) {
        if (std::chrono::steady_clock::now() > deadline) {
            kill(child, SIGKILL); waitpid(child, &status, 0); close(pipe_fds[0]);
            FAIL() << "isolated endpoint PMTU case timed out";
        }
        usleep(10000);
    }
    int result = 0;
    ASSERT_EQ(read(pipe_fds[0], &result, sizeof(result)), static_cast<ssize_t>(sizeof(result)));
    close(pipe_fds[0]);
    if (result < 0 && !DragonOS()) GTEST_SKIP() << "host cannot create isolated netns: " << ErrnoString(-result);
    ASSERT_EQ(result, 0) << "isolated case/namespace error=" << result;
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}
TEST(EndpointPmtu, UdpIpv4FeedbackAndErrors) { Isolated([] { FeedbackScenario(AF_INET, SOCK_DGRAM); }); }
TEST(EndpointPmtu, UdpIpv6FeedbackAndErrors) { Isolated([] { FeedbackScenario(AF_INET6, SOCK_DGRAM); }); }
TEST(EndpointPmtu, RawIpv4FeedbackAndErrors) { Isolated([] { FeedbackScenario(AF_INET, SOCK_RAW); }); }
TEST(EndpointPmtu, RawIpv6FeedbackAndErrors) { Isolated([] { FeedbackScenario(AF_INET6, SOCK_RAW); }); }
TEST(EndpointPmtu, Ipv4RejectsMalformedAndUnmatchedQuotes) { Isolated([] { MalformedFeedbackScenario(AF_INET); }); }
TEST(EndpointPmtu, Ipv6RejectsMalformedAndUnmatchedQuotes) { Isolated([] { MalformedFeedbackScenario(AF_INET6); }); }
TEST(EndpointPmtu, RawIpv4BlockedSendObservesFeedback) { Isolated([] { RawBlockedSendFeedbackScenario(RawSendCall::Send); }); }
TEST(EndpointPmtu, RawIpv4BlockedSendToObservesFeedback) { Isolated([] { RawBlockedSendFeedbackScenario(RawSendCall::SendTo); }); }
TEST(EndpointPmtu, RawIpv4BlockedSendMsgObservesFeedback) { Isolated([] { RawBlockedSendFeedbackScenario(RawSendCall::SendMsg); }); }
TEST(EndpointPmtu, UdpIpv4InterfaceAndOmitIgnoreFeedback) { Isolated([] { FeedbackIgnoredScenario(AF_INET, SOCK_DGRAM); }); }
TEST(EndpointPmtu, UdpIpv6InterfaceAndOmitIgnoreFeedback) { Isolated([] { FeedbackIgnoredScenario(AF_INET6, SOCK_DGRAM); }); }
TEST(EndpointPmtu, RawIpv4InterfaceAndOmitIgnoreFeedback) { Isolated([] { FeedbackIgnoredScenario(AF_INET, SOCK_RAW); }); }
TEST(EndpointPmtu, RawIpv6InterfaceAndOmitLearnPathButSendAtInterfaceMtu) { Isolated([] { FeedbackIgnoredScenario(AF_INET6, SOCK_RAW); }); }
TEST(EndpointPmtu, TcpIpv4FeedbackResegmentsOutstandingData) { Isolated([] { TcpFeedbackScenario(AF_INET); }); }
TEST(EndpointPmtu, TcpIpv6FeedbackResegmentsOutstandingData) { Isolated([] { TcpFeedbackScenario(AF_INET6); }); }
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
