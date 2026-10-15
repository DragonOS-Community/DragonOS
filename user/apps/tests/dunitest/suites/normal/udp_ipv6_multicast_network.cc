#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include "rtnetlink_route_test_support.h"
#include <linux/if_link.h>
#include <linux/veth.h>
#include <poll.h>
#include <sys/stat.h>

namespace {
void Attribute(std::vector<uint8_t>& bytes, uint16_t type, const void* data, size_t length) {
    const size_t start = bytes.size();
    bytes.resize(start + RTA_ALIGN(RTA_LENGTH(length)), 0);
    auto* attr = reinterpret_cast<rtattr*>(bytes.data() + start);
    attr->rta_type = type;
    attr->rta_len = RTA_LENGTH(length);
    if (length) memcpy(RTA_DATA(attr), data, length);
}

class Ipv6MulticastNetwork : public testing::Test {
 protected:
    FdGuard route;
    uint32_t seq = 0;
    unsigned receive_calls = 0;
    unsigned a = 0, b = 0, c = 0, d = 0;

    int Link(uint16_t type, unsigned index, const std::vector<uint8_t>& attrs = {},
             uint16_t flags = 0) {
        std::vector<uint8_t> message(NLMSG_SPACE(sizeof(ifinfomsg)), 0);
        auto* h = reinterpret_cast<nlmsghdr*>(message.data());
        h->nlmsg_len = message.size() + attrs.size();
        h->nlmsg_type = type;
        h->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | flags;
        h->nlmsg_seq = ++seq;
        reinterpret_cast<ifinfomsg*>(NLMSG_DATA(h))->ifi_index = index;
        message.insert(message.end(), attrs.begin(), attrs.end());
        if (send(route.Get(), message.data(), message.size(), 0) !=
            static_cast<ssize_t>(message.size())) return errno;
        return RecvAck(route.Get(), seq);
    }
    int Pair(const char* first, const char* second) {
        std::vector<uint8_t> peer(sizeof(ifinfomsg), 0), data, info, attrs;
        Attribute(peer, IFLA_IFNAME, second, strlen(second) + 1);
        Attribute(data, VETH_INFO_PEER | NLA_F_NESTED, peer.data(), peer.size());
        Attribute(info, IFLA_INFO_KIND, "veth", 5);
        Attribute(info, IFLA_INFO_DATA | NLA_F_NESTED, data.data(), data.size());
        Attribute(attrs, IFLA_IFNAME, first, strlen(first) + 1);
        Attribute(attrs, IFLA_LINKINFO | NLA_F_NESTED, info.data(), info.size());
        return Link(RTM_NEWLINK, 0, attrs, NLM_F_CREATE | NLM_F_EXCL);
    }
    int LinkFlags(unsigned index, unsigned& flags) {
        struct { nlmsghdr header; ifinfomsg link; } request{};
        request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
        request.header.nlmsg_type = RTM_GETLINK;
        request.header.nlmsg_flags = NLM_F_REQUEST;
        request.header.nlmsg_seq = ++seq;
        request.link.ifi_index = index;
        if (send(route.Get(), &request, request.header.nlmsg_len, 0) !=
            static_cast<ssize_t>(request.header.nlmsg_len)) return errno;
        alignas(nlmsghdr) char reply[4096];
        for (;;) {
            int length = recv(route.Get(), reply, sizeof(reply), 0);
            if (length < 0) return errno;
            for (auto* h = reinterpret_cast<nlmsghdr*>(reply); NLMSG_OK(h, length);
                 h = NLMSG_NEXT(h, length)) {
                if (h->nlmsg_seq != seq) continue;
                if (h->nlmsg_type == NLMSG_ERROR) {
                    if (h->nlmsg_len < NLMSG_LENGTH(sizeof(nlmsgerr))) return EPROTO;
                    return -reinterpret_cast<nlmsgerr*>(NLMSG_DATA(h))->error;
                }
                if (h->nlmsg_type == RTM_NEWLINK &&
                    h->nlmsg_len >= NLMSG_LENGTH(sizeof(ifinfomsg))) {
                    flags = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(h))->ifi_flags;
                    return 0;
                }
            }
        }
    }
    void WaitReady(unsigned index) {
        // RTM_NEWLINK ACK publishes administrative UP, not completion of
        // Linux's asynchronous linkwatch/qdisc activation after peer UP.
        // Observe readiness through RTNL, not lockless SIOCGIFFLAGS (which
        // can observe operstate before dev_activate has attached the qdisc).
        for (unsigned attempt = 0; attempt < 1000; ++attempt) {
            unsigned flags = 0;
            ASSERT_EQ(LinkFlags(index, flags), 0);
            if ((flags & (IFF_UP | IFF_RUNNING)) == (IFF_UP | IFF_RUNNING)) return;
            usleep(1000);
        }
        FAIL() << "interface did not become operational: " << index;
    }
    void SetUp() override {
        ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
        route.Reset(OpenRouteSocket());
        ASSERT_GE(route.Get(), 0);
        timeval timeout{2, 0};
        ASSERT_EQ(setsockopt(route.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
        ASSERT_EQ(SetLinkUp(route.Get(), if_nametoindex("lo"), true, ++seq), 0);
        ASSERT_EQ(Pair("mc90a", "mc90b"), 0);
        ASSERT_EQ(Pair("mc90c", "mc90d"), 0);
        a = if_nametoindex("mc90a"); b = if_nametoindex("mc90b");
        c = if_nametoindex("mc90c"); d = if_nametoindex("mc90d");
        const unsigned indices[] = {a, b, c, d};
        const char* addresses[] = {"fe80::90a", "fe80::90b", "fe80::90c", "fe80::90d"};
        for (size_t i = 0; i < 4; ++i) {
            ASSERT_NE(indices[i], 0U);
            ASSERT_EQ(SetLinkUp(route.Get(), indices[i], true, ++seq), 0);
            ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
                NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                indices[i], addresses[i], 64, ++seq), 0);
        }
        for (unsigned index : indices) { WaitReady(index); ASSERT_FALSE(HasFatalFailure()); }
    }
    sockaddr_in6 Address(const char* group, uint16_t port = 0) {
        sockaddr_in6 addr{};
        addr.sin6_family = AF_INET6; addr.sin6_port = htons(port);
        EXPECT_EQ(inet_pton(AF_INET6, group, &addr.sin6_addr), 1);
        return addr;
    }
    int Receiver(uint16_t port, const char* local = "::", const char* device = nullptr,
                 bool reuse_port = false) {
        const int fd = socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0);
        EXPECT_GE(fd, 0);
        const int on = 1;
        EXPECT_EQ(setsockopt(fd, SOL_SOCKET, SO_REUSEADDR, &on, sizeof(on)), 0);
        if (reuse_port) EXPECT_EQ(setsockopt(fd, SOL_SOCKET, SO_REUSEPORT, &on, sizeof(on)), 0);
        // A link-scoped concrete IPv6 bind needs an interface on Linux.
        if (device == nullptr && local[0] == 'f') device = "mc90b";
        if (device) EXPECT_EQ(setsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE,
                                        device, strlen(device) + 1), 0) << errno;
        auto addr = Address(local, port);
        EXPECT_EQ(bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0) << errno;
        return fd;
    }
    int Member(int fd, unsigned index, bool join = true, const char* group = "ff02::9090") {
        ipv6_mreq req{};
        EXPECT_EQ(inet_pton(AF_INET6, group, &req.ipv6mr_multiaddr), 1);
        req.ipv6mr_interface = index;
        return setsockopt(fd, IPPROTO_IPV6, join ? IPV6_JOIN_GROUP : IPV6_LEAVE_GROUP,
                          &req, sizeof(req));
    }
    void All(int fd, bool enabled) {
        const int value = enabled;
        ASSERT_EQ(setsockopt(fd, IPPROTO_IPV6, IPV6_MULTICAST_ALL, &value, sizeof(value)), 0);
    }
    void Transmit(unsigned index, uint16_t port, const char* group = "ff02::9090",
                  bool loop = false, bool connected = false) {
        FdGuard tx(socket(AF_INET6, SOCK_DGRAM, 0));
        ASSERT_GE(tx.Get(), 0);
        ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_IF, &index, sizeof(index)), 0);
        const int enabled = loop;
        ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &enabled, sizeof(enabled)), 0);
        auto addr = Address(group, port);
        if (connected) {
            ASSERT_EQ(connect(tx.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0) << errno;
            ASSERT_EQ(send(tx.Get(), "M", 1, 0), 1) << errno;
        } else {
            ASSERT_EQ(sendto(tx.Get(), "M", 1, 0, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 1)
                << ErrnoString(errno);
        }
    }
    void Receive(int fd, bool expected) {
        SCOPED_TRACE("receive=" + std::to_string(++receive_calls) + " fd=" + std::to_string(fd));
        pollfd ready{fd, POLLIN, 0};
        ASSERT_EQ(poll(&ready, 1, expected ? 2000 : 50), expected ? 1 : 0) << errno;
        if (expected) {
            char byte = 0;
            ASSERT_EQ(recv(fd, &byte, 1, 0), 1) << errno;
            EXPECT_EQ(byte, 'M');
        }
    }
};

TEST_F(Ipv6MulticastNetwork, RealVethReceiveAndLeave) {
    FdGuard rx(Receiver(31001, "::", "mc90b"));
    ASSERT_EQ(Member(rx.Get(), b), 0) << errno;
    Transmit(a, 31001); Receive(rx.Get(), true);
    ASSERT_EQ(Member(rx.Get(), b, false), 0);
    Transmit(a, 31001); Receive(rx.Get(), false);
}
TEST_F(Ipv6MulticastNetwork, FanoutUsesAllAndIndependentReferences) {
    FdGuard first(Receiver(31002)), second(Receiver(31002)), unrelated(Receiver(31002));
    ASSERT_EQ(Member(first.Get(), b), 0);
    ASSERT_EQ(Member(second.Get(), b), 0);
    All(unrelated.Get(), false);
    Transmit(a, 31002);
    Receive(first.Get(), true); Receive(second.Get(), true); Receive(unrelated.Get(), false);
    first.Reset();
    Transmit(a, 31002); Receive(second.Get(), true);
    All(unrelated.Get(), true);
    Transmit(a, 31002); Receive(second.Get(), true); Receive(unrelated.Get(), true);
    second.Reset();
    Transmit(a, 31002); Receive(unrelated.Get(), false);
}
TEST_F(Ipv6MulticastNetwork, MulticastAllDoesNotBypassDeviceMembership) {
    FdGuard rx(Receiver(31003)), owner(Receiver(31004));
    ASSERT_EQ(Member(owner.Get(), b), 0);
    Transmit(c, 31003); Receive(rx.Get(), false);
    Transmit(a, 31003); Receive(rx.Get(), true);
}
TEST_F(Ipv6MulticastNetwork, GroupOnlySocketMembershipAcrossTwoIngressInterfaces) {
    FdGuard rx(Receiver(31005)), owner(Receiver(31006));
    All(rx.Get(), false);
    ASSERT_EQ(Member(rx.Get(), b), 0);
    ASSERT_EQ(Member(owner.Get(), d), 0);
    Transmit(c, 31005); Receive(rx.Get(), true);
    Transmit(a, 31005); Receive(rx.Get(), true);
}
TEST_F(Ipv6MulticastNetwork, ConcreteBindAndDeviceFilterDoNotWiden) {
    FdGuard exact(Receiver(31007, "ff02::9090")), wrong(Receiver(31007, "ff02::9091"));
    FdGuard bound(Receiver(31007, "::", "mc90d"));
    ASSERT_EQ(Member(exact.Get(), b), 0);
    ASSERT_EQ(Member(bound.Get(), d), 0);
    Transmit(a, 31007);
    Receive(exact.Get(), true); Receive(wrong.Get(), false); Receive(bound.Get(), false);
}
TEST_F(Ipv6MulticastNetwork, ConnectedSendUsesMulticastInterface) {
    FdGuard rx(Receiver(31008, "::", "mc90d"));
    ASSERT_EQ(Member(rx.Get(), d), 0);
    Transmit(c, 31008, "ff02::9090", false, true); Receive(rx.Get(), true);
}
TEST_F(Ipv6MulticastNetwork, LoopCloneCanBeDisabledWithoutSuppressingWire) {
    FdGuard local(Receiver(31009, "::", "mc90a")), peer(Receiver(31009, "::", "mc90b"));
    ASSERT_EQ(Member(local.Get(), a), 0); ASSERT_EQ(Member(peer.Get(), b), 0);
    Transmit(a, 31009, "ff02::9090", false);
    Receive(local.Get(), false); Receive(peer.Get(), true);
    Transmit(a, 31009, "ff02::9090", true);
    Receive(local.Get(), true); Receive(peer.Get(), true);
}
TEST_F(Ipv6MulticastNetwork, ImplicitAllNodesHonorsMulticastAll) {
    FdGuard yes(Receiver(31010)), no(Receiver(31010));
    All(no.Get(), false);
    Transmit(a, 31010, "ff02::1");
    Receive(yes.Get(), true); Receive(no.Get(), false);
}
TEST_F(Ipv6MulticastNetwork, LastDupCloseReleasesMembership) {
    FdGuard owner(Receiver(31015)), all(Receiver(31015));
    ASSERT_EQ(Member(owner.Get(), b), 0);
    FdGuard duplicate(dup(owner.Get()));
    ASSERT_GE(duplicate.Get(), 0);
    owner.Reset();
    Transmit(a, 31015); Receive(duplicate.Get(), true); Receive(all.Get(), true);
    duplicate.Reset();
    Transmit(a, 31015); Receive(all.Get(), false);
}
TEST_F(Ipv6MulticastNetwork, ReuseportStillFansOutEveryMatchingSocket) {
    FdGuard first(Receiver(31020, "::", nullptr, true));
    FdGuard second(Receiver(31020, "::", nullptr, true));
    FdGuard third(Receiver(31020, "::", nullptr, true));
    ASSERT_EQ(Member(first.Get(), b), 0);
    Transmit(a, 31020);
    Receive(first.Get(), true); Receive(second.Get(), true); Receive(third.Get(), true);
}
TEST_F(Ipv6MulticastNetwork, ConnectedGroupBindKeepsReceiveAddressAndPeerFilter) {
    FdGuard rx(Receiver(31019, "ff02::9090", "mc90b"));
    FdGuard tx(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(tx.Get(), 0);
    ASSERT_EQ(setsockopt(tx.Get(), SOL_SOCKET, SO_BINDTODEVICE, "mc90a", 7), 0);
    ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_IF, &a, sizeof(a)), 0);
    const int loop = 0;
    ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &loop, sizeof(loop)), 0);
    auto source = Address("fe80::90a");
    ASSERT_EQ(bind(tx.Get(), reinterpret_cast<sockaddr*>(&source), sizeof(source)), 0) << errno;
    socklen_t length = sizeof(source);
    ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&source), &length), 0);
    source.sin6_scope_id = b;
    ASSERT_EQ(Member(rx.Get(), b), 0);
    ASSERT_EQ(connect(rx.Get(), reinterpret_cast<sockaddr*>(&source), sizeof(source)), 0) << errno;
    auto destination = Address("ff02::9090", 31019);
    ASSERT_EQ(sendto(tx.Get(), "M", 1, 0, reinterpret_cast<sockaddr*>(&destination),
                     sizeof(destination)), 1) << errno;
    Receive(rx.Get(), true);
    // Same source address but a different UDP peer port must still be rejected.
    Transmit(a, 31019); Receive(rx.Get(), false);
}
TEST_F(Ipv6MulticastNetwork, OutputInterfaceConflictDoesNotCommit) {
    FdGuard rx(Receiver(31021, "::", "mc90b"));
    EXPECT_EQ(setsockopt(rx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_IF, &a, sizeof(a)), -1);
    EXPECT_EQ(errno, EINVAL);
    unsigned actual = 99;
    socklen_t length = sizeof(actual);
    ASSERT_EQ(getsockopt(rx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_IF, &actual, &length), 0);
    EXPECT_EQ(actual, 0U);
}
TEST_F(Ipv6MulticastNetwork, GroupPortZeroDisconnectSendtoUsesUnicastSource) {
    FdGuard rx(Receiver(31022, "::", "mc90b"));
    FdGuard tx(Receiver(0, "ff02::9090", "mc90a"));
    ASSERT_EQ(Member(rx.Get(), b), 0);
    ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_IF, &a, sizeof(a)), 0);
    const int loop = 0;
    ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &loop, sizeof(loop)), 0);
    auto destination = Address("ff02::9090", 31022);
    ASSERT_EQ(connect(tx.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0)
        << ErrnoString(errno);
    sockaddr disconnect{};
    disconnect.sa_family = AF_UNSPEC;
    ASSERT_EQ(connect(tx.Get(), &disconnect, sizeof(disconnect)), 0) << errno;
    sockaddr_in6 local{};
    socklen_t length = sizeof(local);
    ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
    EXPECT_EQ(local.sin6_port, 0);
    auto group = Address("ff02::9090");
    EXPECT_EQ(memcmp(&local.sin6_addr, &group.sin6_addr, sizeof(in6_addr)), 0);
    ASSERT_EQ(sendto(tx.Get(), "M", 1, 0, reinterpret_cast<sockaddr*>(&destination),
                     sizeof(destination)), 1) << ErrnoString(errno);
    pollfd ready{rx.Get(), POLLIN, 0};
    ASSERT_EQ(poll(&ready, 1, 2000), 1);
    sockaddr_in6 source{};
    length = sizeof(source);
    char byte = 0;
    ASSERT_EQ(recvfrom(rx.Get(), &byte, 1, 0, reinterpret_cast<sockaddr*>(&source), &length), 1);
    EXPECT_EQ(byte, 'M');
    auto unicast = Address("fe80::90a");
    EXPECT_EQ(memcmp(&source.sin6_addr, &unicast.sin6_addr, sizeof(in6_addr)), 0);
    ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
    EXPECT_NE(local.sin6_port, 0);
    EXPECT_EQ(local.sin6_port, source.sin6_port);
    EXPECT_EQ(memcmp(&local.sin6_addr, &group.sin6_addr, sizeof(in6_addr)), 0);
    // A second send follows the now-bound path, with the same receive bind.
    ASSERT_EQ(sendto(tx.Get(), "M", 1, 0, reinterpret_cast<sockaddr*>(&destination),
                     sizeof(destination)), 1) << ErrnoString(errno);
    Receive(rx.Get(), true);
}
TEST_F(Ipv6MulticastNetwork, ExplicitSolicitedGroupSurvivesAddressDeletion) {
    constexpr const char* group = "ff02::1:ff00:90b";
    FdGuard rx(Receiver(31016));
    ASSERT_EQ(Member(rx.Get(), b, true, group), 0);
    Transmit(a, 31016, group); Receive(rx.Get(), true);
    ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK,
                                  b, "fe80::90b", 64, ++seq), 0);
    Transmit(a, 31016, group); Receive(rx.Get(), true);
    ASSERT_EQ(Member(rx.Get(), b, false, group), 0);
    Transmit(a, 31016, group); Receive(rx.Get(), false);
}
TEST_F(Ipv6MulticastNetwork, ExplicitUnrelatedSolicitedGroupSurvivesAddressAddition) {
    constexpr const char* group = "ff02::1:ff00:1234";
    FdGuard rx(Receiver(31017));
    ASSERT_EQ(Member(rx.Get(), b, true, group), 0);
    Transmit(a, 31017, group); Receive(rx.Get(), true);
    ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        b, "fe80::90e", 64, ++seq), 0);
    Transmit(a, 31017, group); Receive(rx.Get(), true);
}
TEST_F(Ipv6MulticastNetwork, AddressKeepsSolicitedGroupAfterExplicitLeave) {
    constexpr const char* group = "ff02::1:ff00:90b";
    FdGuard rx(Receiver(31018));
    ASSERT_EQ(Member(rx.Get(), b, true, group), 0);
    ASSERT_EQ(Member(rx.Get(), b, false, group), 0);
    Transmit(a, 31018, group); Receive(rx.Get(), true);
}
TEST_F(Ipv6MulticastNetwork, ZeroJoinUsesFibNotSocketOutputPreference) {
    FdGuard rx(Receiver(31011));
    const unsigned impossible = 0x7fffffff;
    // A failed IF update must not affect route-selected group admission.
    EXPECT_EQ(setsockopt(rx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_IF,
                        &impossible, sizeof(impossible)), -1);
    ASSERT_EQ(Member(rx.Get(), 0), 0) << errno;
    ASSERT_EQ(Member(rx.Get(), 0, false), 0);
}
TEST_F(Ipv6MulticastNetwork, LeaveAfterDeviceDeletionSucceeds) {
    FdGuard rx(Receiver(31012));
    ASSERT_EQ(Member(rx.Get(), b), 0);
    ASSERT_EQ(Link(RTM_DELLINK, a), 0);
    ASSERT_EQ(Member(rx.Get(), b, false), 0) << errno;
    EXPECT_EQ(Member(rx.Get(), b), -1); EXPECT_EQ(errno, ENODEV);
}

TEST_F(Ipv6MulticastNetwork, MovedDeviceDoesNotCarryOldNamespaceMembership) {
    FdGuard original(open("/proc/self/ns/net", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(original.Get(), 0);
    FdGuard old_rx(Receiver(31013)), tx(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_EQ(Member(old_rx.Get(), b), 0);
    ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_IF, &a, sizeof(a)), 0);
    const int loop = 0;
    ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_MULTICAST_LOOP, &loop, sizeof(loop)), 0);
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    FdGuard other(open("/proc/self/ns/net", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(other.Get(), 0);
    struct stat original_stat{}, other_stat{};
    ASSERT_EQ(fstat(original.Get(), &original_stat), 0);
    ASSERT_EQ(fstat(other.Get(), &other_stat), 0);
    ASSERT_NE(original_stat.st_ino, other_stat.st_ino) << "unshare must produce a distinct namespace fd";
    ASSERT_EQ(setns(original.Get(), CLONE_NEWNET), 0);
    std::vector<uint8_t> attrs;
    const int other_fd = other.Get();
    Attribute(attrs, IFLA_NET_NS_FD, &other_fd, sizeof(other_fd));
    ASSERT_EQ(Link(RTM_NEWLINK, b, attrs), 0);
    ASSERT_EQ(if_nametoindex("mc90b"), 0U) << "source namespace must lose the moved device";
    auto dst = Address("ff02::9090", 31013);
    ASSERT_EQ(sendto(tx.Get(), "M", 1, 0, reinterpret_cast<sockaddr*>(&dst), sizeof(dst)), 1);
    Receive(old_rx.Get(), false);
    ASSERT_EQ(setns(other.Get(), CLONE_NEWNET), 0);
    // Linux may renumber a moved device; never use a foreign namespace index.
    const unsigned moved = if_nametoindex("mc90b");
    ASSERT_NE(moved, 0U);
    route.Reset(OpenRouteSocket());
    ASSERT_EQ(SetLinkUp(route.Get(), moved, true, ++seq), 0);
    ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        moved, "fe80::90b", 64, ++seq), 0);
    WaitReady(moved); ASSERT_FALSE(HasFatalFailure());
    ASSERT_EQ(setns(original.Get(), CLONE_NEWNET), 0);
    route.Reset(OpenRouteSocket());
    WaitReady(a); ASSERT_FALSE(HasFatalFailure());
    ASSERT_EQ(setns(other.Get(), CLONE_NEWNET), 0);
    route.Reset(OpenRouteSocket());
    FdGuard new_rx(Receiver(31013));
    ASSERT_EQ(Member(new_rx.Get(), moved), 0);
    ASSERT_EQ(sendto(tx.Get(), "M", 1, 0, reinterpret_cast<sockaddr*>(&dst), sizeof(dst)), 1);
    Receive(new_rx.Get(), true); Receive(old_rx.Get(), false);
    // Closing the old namespace socket must not drop the new device reference.
    old_rx.Reset();
    ASSERT_EQ(sendto(tx.Get(), "M", 1, 0, reinterpret_cast<sockaddr*>(&dst), sizeof(dst)), 1);
    Receive(new_rx.Get(), true);
}

TEST_F(Ipv6MulticastNetwork, MoveAwayAndBackOldCloseDoesNotReleaseNewEpoch) {
    FdGuard original(open("/proc/self/ns/net", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(original.Get(), 0);
    FdGuard old_rx(Receiver(31014));
    ASSERT_EQ(Member(old_rx.Get(), b), 0);
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    FdGuard other(open("/proc/self/ns/net", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(other.Get(), 0);
    struct stat original_stat{}, other_stat{};
    ASSERT_EQ(fstat(original.Get(), &original_stat), 0);
    ASSERT_EQ(fstat(other.Get(), &other_stat), 0);
    ASSERT_NE(original_stat.st_ino, other_stat.st_ino) << "unshare must produce a distinct namespace fd";
    ASSERT_EQ(setns(original.Get(), CLONE_NEWNET), 0);
    std::vector<uint8_t> attrs;
    int nsfd = other.Get();
    Attribute(attrs, IFLA_NET_NS_FD, &nsfd, sizeof(nsfd));
    ASSERT_EQ(Link(RTM_NEWLINK, b, attrs), 0);
    ASSERT_EQ(if_nametoindex("mc90b"), 0U) << "source namespace must lose the moved device";
    ASSERT_EQ(setns(other.Get(), CLONE_NEWNET), 0);
    route.Reset(OpenRouteSocket());
    const unsigned moved = if_nametoindex("mc90b");
    ASSERT_NE(moved, 0U);
    attrs.clear(); nsfd = original.Get();
    Attribute(attrs, IFLA_NET_NS_FD, &nsfd, sizeof(nsfd));
    ASSERT_EQ(Link(RTM_NEWLINK, moved, attrs), 0);
    ASSERT_EQ(if_nametoindex("mc90b"), 0U) << "temporary namespace must lose the returned device";
    ASSERT_EQ(setns(original.Get(), CLONE_NEWNET), 0);
    route.Reset(OpenRouteSocket());
    b = if_nametoindex("mc90b");
    ASSERT_NE(b, 0U);
    ASSERT_EQ(SetLinkUp(route.Get(), b, true, ++seq), 0);
    // Addresses are removed by a namespace move on Linux as well as DragonOS.
    ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        b, "fe80::90b", 64, ++seq), 0);
    WaitReady(a); WaitReady(b); ASSERT_FALSE(HasFatalFailure());
    FdGuard fresh(Receiver(31014));
    ASSERT_EQ(Member(fresh.Get(), b), 0);
    Transmit(a, 31014); Receive(fresh.Get(), true);
    old_rx.Reset();
    Transmit(a, 31014); Receive(fresh.Get(), true);
}
} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
