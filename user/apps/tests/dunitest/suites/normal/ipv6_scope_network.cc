#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include "rtnetlink_route_test_support.h"
#include <linux/if_link.h>
#include <linux/errqueue.h>
#include <linux/veth.h>
#include <poll.h>
#include <sys/mman.h>
#include <linux/if_ether.h>
#include <atomic>
#include <thread>

namespace {
void AppendScopeAttr(std::vector<uint8_t>& bytes, uint16_t type, const void* data, size_t length) {
    const size_t start = bytes.size();
    bytes.resize(start + RTA_ALIGN(RTA_LENGTH(length)), 0);
    auto* attr = reinterpret_cast<rtattr*>(bytes.data() + start);
    attr->rta_type = type;
    attr->rta_len = RTA_LENGTH(length);
    if (length) memcpy(RTA_DATA(attr), data, length);
}

class Ipv6ScopeNetwork : public testing::Test {
 protected:
    FdGuard route;
    unsigned a = 0, b = 0, c = 0, d = 0;
    uint32_t seq = 0;
    int Delete(unsigned index) {
        struct { nlmsghdr h; ifinfomsg link; } request{};
        request.h.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
        request.h.nlmsg_type = RTM_DELLINK;
        request.h.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
        request.h.nlmsg_seq = ++seq;
        request.link.ifi_index = index;
        if (send(route.Get(), &request, request.h.nlmsg_len, 0) !=
            static_cast<ssize_t>(request.h.nlmsg_len)) return errno;
        return RecvAck(route.Get(), seq);
    }
    int Pair(const char* first, const char* second) {
        std::vector<uint8_t> peer(sizeof(ifinfomsg), 0), data, info, attrs;
        AppendScopeAttr(peer, IFLA_IFNAME, second, strlen(second) + 1);
        AppendScopeAttr(data, VETH_INFO_PEER | NLA_F_NESTED, peer.data(), peer.size());
        AppendScopeAttr(info, IFLA_INFO_KIND, "veth", 5);
        AppendScopeAttr(info, IFLA_INFO_DATA | NLA_F_NESTED, data.data(), data.size());
        AppendScopeAttr(attrs, IFLA_IFNAME, first, strlen(first) + 1);
        AppendScopeAttr(attrs, IFLA_LINKINFO | NLA_F_NESTED, info.data(), info.size());
        std::vector<uint8_t> message(NLMSG_SPACE(sizeof(ifinfomsg)), 0);
        auto* h = reinterpret_cast<nlmsghdr*>(message.data());
        h->nlmsg_len = message.size() + attrs.size();
        h->nlmsg_type = RTM_NEWLINK;
        h->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
        h->nlmsg_seq = ++seq;
        message.insert(message.end(), attrs.begin(), attrs.end());
        if (send(route.Get(), message.data(), message.size(), 0) !=
            static_cast<ssize_t>(message.size())) return errno;
        return RecvAck(route.Get(), seq);
    }
    int Flags(unsigned index, unsigned& flags) {
        struct { nlmsghdr h; ifinfomsg link; } request{};
        request.h.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
        request.h.nlmsg_type = RTM_GETLINK;
        request.h.nlmsg_flags = NLM_F_REQUEST;
        request.h.nlmsg_seq = ++seq;
        request.link.ifi_index = index;
        if (send(route.Get(), &request, request.h.nlmsg_len, 0) !=
            static_cast<ssize_t>(request.h.nlmsg_len)) return errno;
        alignas(nlmsghdr) char bytes[4096];
        for (;;) {
            int size = recv(route.Get(), bytes, sizeof(bytes), 0);
            if (size < 0) return errno;
            for (auto* h = reinterpret_cast<nlmsghdr*>(bytes); NLMSG_OK(h, size);
                 h = NLMSG_NEXT(h, size)) {
                if (h->nlmsg_seq != seq) continue;
                if (h->nlmsg_type == NLMSG_ERROR) {
                    if (h->nlmsg_len < NLMSG_LENGTH(sizeof(nlmsgerr))) return EPROTO;
                    return -reinterpret_cast<nlmsgerr*>(NLMSG_DATA(h))->error;
                }
                if (h->nlmsg_type == RTM_NEWLINK && h->nlmsg_len >= NLMSG_LENGTH(sizeof(ifinfomsg))) {
                    flags = reinterpret_cast<ifinfomsg*>(NLMSG_DATA(h))->ifi_flags;
                    return 0;
                }
            }
        }
    }
    void Ready(unsigned index) {
        // Observe RTNL-serialized qdisc activation, not just admin-UP ACK.
        for (unsigned retry = 0; retry < 1000; ++retry) {
            unsigned flags = 0;
            ASSERT_EQ(Flags(index, flags), 0);
            if ((flags & (IFF_UP | IFF_RUNNING)) == (IFF_UP | IFF_RUNNING)) return;
            usleep(1000);
        }
        FAIL() << "interface not operational: " << index;
    }
    void SetUp() override {
        ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
        route.Reset(OpenRouteSocket());
        ASSERT_GE(route.Get(), 0);
        timeval timeout{2, 0};
        ASSERT_EQ(setsockopt(route.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
        ASSERT_EQ(SetLinkUp(route.Get(), if_nametoindex("lo"), true, ++seq), 0);
        ASSERT_EQ(Pair("sc92a", "sc92b"), 0);
        ASSERT_EQ(Pair("sc92c", "sc92d"), 0);
        a = if_nametoindex("sc92a"); b = if_nametoindex("sc92b");
        c = if_nametoindex("sc92c"); d = if_nametoindex("sc92d");
        const unsigned indices[] = {a, b, c, d};
        // Identical addresses on two independent links make scope observable.
        const char* addresses[] = {"fe80::92a", "fe80::92b", "fe80::92a", "fe80::92b"};
        for (size_t i = 0; i < 4; ++i) {
            ASSERT_NE(indices[i], 0U);
            ASSERT_EQ(SetLinkUp(route.Get(), indices[i], true, ++seq), 0);
            ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
                NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                indices[i], addresses[i], 64, ++seq), 0);
        }
        for (unsigned index : indices) { Ready(index); ASSERT_FALSE(HasFatalFailure()); }
    }
    sockaddr_in6 Address(const char* text, unsigned scope, uint16_t port = 0) {
        sockaddr_in6 address{};
        address.sin6_family = AF_INET6; address.sin6_port = htons(port);
        address.sin6_scope_id = scope;
        EXPECT_EQ(inet_pton(AF_INET6, text, &address.sin6_addr), 1);
        return address;
    }
    int Bound(int type, const char* text, unsigned scope, uint16_t port = 0) {
        int fd = socket(AF_INET6, type | SOCK_NONBLOCK, type == SOCK_RAW ? 253 : 0);
        EXPECT_GE(fd, 0);
        auto address = Address(text, scope, port);
        EXPECT_EQ(bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << ErrnoString(errno);
        return fd;
    }
    void Receive(int fd, unsigned scope, bool message = false) {
        SCOPED_TRACE("scope=" + std::to_string(scope) + " fd=" + std::to_string(fd));
        pollfd ready{fd, POLLIN, 0};
        ASSERT_EQ(poll(&ready, 1, 2000), 1) << errno;
        sockaddr_in6 source{};
        char byte = 0;
        socklen_t length = sizeof(source);
        if (message) {
            iovec vector{&byte, 1};
            msghdr header{};
            header.msg_name = &source; header.msg_namelen = sizeof(source);
            header.msg_iov = &vector; header.msg_iovlen = 1;
            ASSERT_EQ(recvmsg(fd, &header, 0), 1) << ErrnoString(errno);
            length = header.msg_namelen;
        } else {
            ASSERT_EQ(recvfrom(fd, &byte, 1, 0, reinterpret_cast<sockaddr*>(&source), &length), 1)
                << ErrnoString(errno);
        }
        EXPECT_EQ(length, sizeof(source));
        EXPECT_EQ(source.sin6_scope_id, scope);
        auto expected = Address("fe80::92a", 0);
        EXPECT_EQ(memcmp(&source.sin6_addr, &expected.sin6_addr, sizeof(in6_addr)), 0);
        EXPECT_EQ(byte, 'S');
    }
    void Send(int fd, unsigned scope, uint16_t port, bool message = false) {
        auto destination = Address("fe80::92b", scope, port);
        if (message) {
            char byte = 'S'; iovec vector{&byte, 1}; msghdr header{};
            header.msg_name = &destination; header.msg_namelen = sizeof(destination);
            header.msg_iov = &vector; header.msg_iovlen = 1;
            ASSERT_EQ(sendmsg(fd, &header, 0), 1) << ErrnoString(errno);
        } else {
            ASSERT_EQ(sendto(fd, "S", 1, 0, reinterpret_cast<sockaddr*>(&destination),
                             sizeof(destination)), 1) << ErrnoString(errno);
        }
    }
};

TEST_F(Ipv6ScopeNetwork, UdpPerMessageScopeSelectsDuplicateAddressLinkAndIngress) {
    FdGuard first(Bound(SOCK_DGRAM, "fe80::92b", b, 31092));
    FdGuard second(Bound(SOCK_DGRAM, "fe80::92b", d, 31092));
    FdGuard tx(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(tx.Get(), 0);
    Send(tx.Get(), a, 31092); Receive(first.Get(), b);
    Send(tx.Get(), c, 31092, true); Receive(second.Get(), d, true);
    char name[IFNAMSIZ]{}; socklen_t length = sizeof(name);
    ASSERT_EQ(getsockopt(tx.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, &length), 0);
    EXPECT_EQ(length, 0U);
}
TEST_F(Ipv6ScopeNetwork, PerMessageScopeOverridesBoundDeviceWithoutChangingIt) {
    FdGuard rx(Bound(SOCK_DGRAM, "fe80::92b", d, 31093));
    FdGuard tx(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(tx.Get(), 0);
    ASSERT_EQ(setsockopt(tx.Get(), SOL_SOCKET, SO_BINDTODEVICE, "sc92a", 6), 0);
    Send(tx.Get(), c, 31093); Receive(rx.Get(), d);
    char name[IFNAMSIZ]{}; socklen_t length = sizeof(name);
    ASSERT_EQ(getsockopt(tx.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, &length), 0);
    EXPECT_STREQ(name, "sc92a");
}
TEST_F(Ipv6ScopeNetwork, RawScopeUsesPacketIngressForBothReceiveApis) {
    FdGuard first(Bound(SOCK_RAW, "fe80::92b", b));
    FdGuard second(Bound(SOCK_RAW, "fe80::92b", d));
    FdGuard tx(socket(AF_INET6, SOCK_RAW, 253));
    ASSERT_GE(tx.Get(), 0);
    Send(tx.Get(), a, 0); Receive(first.Get(), b);
    Send(tx.Get(), c, 0, true); Receive(second.Get(), d, true);
}
TEST_F(Ipv6ScopeNetwork, WildcardTcpAcceptUsesChildIngressScope) {
    FdGuard server(Bound(SOCK_STREAM, "::", 0, 31094));
    ASSERT_EQ(listen(server.Get(), 8), 0);
    for (const auto& indices : {std::make_pair(a, b), std::make_pair(c, d)}) {
        const auto outgoing = indices.first, incoming = indices.second;
        FdGuard client(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
        ASSERT_GE(client.Get(), 0);
        auto destination = Address("fe80::92b", outgoing, 31094);
        int result = connect(client.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination));
        ASSERT_TRUE(result == 0 || (result == -1 && errno == EINPROGRESS)) << ErrnoString(errno);
        pollfd ready{client.Get(), POLLOUT, 0};
        ASSERT_EQ(poll(&ready, 1, 2000), 1);
        int error = -1; socklen_t length = sizeof(error);
        ASSERT_EQ(getsockopt(client.Get(), SOL_SOCKET, SO_ERROR, &error, &length), 0);
        ASSERT_EQ(error, 0);
        ready = {server.Get(), POLLIN, 0};
        ASSERT_EQ(poll(&ready, 1, 2000), 1);
        sockaddr_in6 peer{}; length = sizeof(peer);
        FdGuard child(accept4(server.Get(), reinterpret_cast<sockaddr*>(&peer), &length, SOCK_NONBLOCK));
        ASSERT_GE(child.Get(), 0) << ErrnoString(errno);
        EXPECT_EQ(peer.sin6_scope_id, incoming);
        ASSERT_EQ(getpeername(child.Get(), reinterpret_cast<sockaddr*>(&peer), &length), 0);
        EXPECT_EQ(peer.sin6_scope_id, incoming);
        sockaddr_in6 local{}; length = sizeof(local);
        ASSERT_EQ(getsockname(child.Get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
        EXPECT_EQ(local.sin6_scope_id, incoming);
        ASSERT_EQ(getsockname(client.Get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
        EXPECT_EQ(local.sin6_scope_id, outgoing);
        ASSERT_EQ(getpeername(client.Get(), reinterpret_cast<sockaddr*>(&peer), &length), 0);
        EXPECT_EQ(peer.sin6_scope_id, outgoing);
        ASSERT_EQ(send(client.Get(), "S", 1, 0), 1);
        ready = {child.Get(), POLLIN, 0}; ASSERT_EQ(poll(&ready, 1, 2000), 1);
        char byte = 0; ASSERT_EQ(recv(child.Get(), &byte, 1, 0), 1); EXPECT_EQ(byte, 'S');
    }
}
TEST_F(Ipv6ScopeNetwork, BindOverridesButConnectRejectsConflictingScope) {
    for (int type : {SOCK_DGRAM, SOCK_STREAM, SOCK_RAW}) {
        SCOPED_TRACE("type=" + std::to_string(type));
        FdGuard bound(socket(AF_INET6, type | SOCK_NONBLOCK, type == SOCK_RAW ? 253 : 0));
        ASSERT_GE(bound.Get(), 0);
        ASSERT_EQ(setsockopt(bound.Get(), SOL_SOCKET, SO_BINDTODEVICE, "sc92a", 6), 0);
        auto address = Address("fe80::92b", b);
        ASSERT_EQ(bind(bound.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << ErrnoString(errno);
        char name[IFNAMSIZ]{}; socklen_t length = sizeof(name);
        ASSERT_EQ(getsockopt(bound.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, &length), 0);
        EXPECT_STREQ(name, "sc92b");
        FdGuard connecting(socket(AF_INET6, type | SOCK_NONBLOCK, type == SOCK_RAW ? 253 : 0));
        ASSERT_GE(connecting.Get(), 0);
        ASSERT_EQ(setsockopt(connecting.Get(), SOL_SOCKET, SO_BINDTODEVICE, "sc92a", 6), 0);
        address = Address("fe80::92b", c, 31096);
        ASSERT_EQ(connect(connecting.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), -1);
        EXPECT_EQ(errno, EINVAL);
        length = sizeof(name);
        ASSERT_EQ(getsockopt(connecting.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, &length), 0);
        EXPECT_STREQ(name, "sc92a");
    }
}
TEST_F(Ipv6ScopeNetwork, FailedConnectRetainsScopeAndPerPacketScopeCanOverrideIt) {
    for (int type : {SOCK_DGRAM, SOCK_STREAM, SOCK_RAW}) {
        SCOPED_TRACE("type=" + std::to_string(type));
        FdGuard fd(socket(AF_INET6, type | SOCK_NONBLOCK, type == SOCK_RAW ? 253 : 0));
        ASSERT_GE(fd.Get(), 0);
        auto destination = Address("fe80::92b", 0x7ffffffeU, 31097);
        ASSERT_EQ(connect(fd.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), -1);
        EXPECT_EQ(errno, ENETUNREACH);
        char name[IFNAMSIZ]{}; socklen_t length = sizeof(name);
        ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, &length), -1);
        EXPECT_EQ(errno, ENODEV);
        if (type == SOCK_DGRAM || type == SOCK_RAW) {
            FdGuard rx(Bound(type, "fe80::92b", d, type == SOCK_DGRAM ? 31097 : 0));
            Send(fd.Get(), c, type == SOCK_DGRAM ? 31097 : 0, true);
            Receive(rx.Get(), d, true);
            length = sizeof(name);
            ASSERT_EQ(getsockopt(fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, &length), -1);
            EXPECT_EQ(errno, ENODEV);
        }
    }
}
TEST_F(Ipv6ScopeNetwork, UdpConnectNamesAndDisconnectClearScope) {
    FdGuard first(Bound(SOCK_DGRAM, "fe80::92b", b, 31098));
    FdGuard second(Bound(SOCK_DGRAM, "fe80::92b", d, 31098));
    FdGuard tx(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(tx.Get(), 0);
    auto destination = Address("fe80::92b", a, 31098);
    ASSERT_EQ(connect(tx.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0)
        << ErrnoString(errno);
    sockaddr_in6 actual{}; socklen_t length = sizeof(actual);
    ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&actual), &length), 0);
    EXPECT_EQ(actual.sin6_scope_id, a);
    ASSERT_EQ(getpeername(tx.Get(), reinterpret_cast<sockaddr*>(&actual), &length), 0);
    EXPECT_EQ(actual.sin6_scope_id, a);
    ASSERT_EQ(send(tx.Get(), "S", 1, 0), 1); Receive(first.Get(), b);
    sockaddr disconnect{}; disconnect.sa_family = AF_UNSPEC;
    ASSERT_EQ(connect(tx.Get(), &disconnect, sizeof(disconnect)), 0);
    char name[IFNAMSIZ]{}; length = sizeof(name);
    ASSERT_EQ(getsockopt(tx.Get(), SOL_SOCKET, SO_BINDTODEVICE, name, &length), 0);
    EXPECT_EQ(length, 0U);
    Send(tx.Get(), c, 31098); Receive(second.Get(), d);
}
TEST_F(Ipv6ScopeNetwork, RawConnectSelectsSourceAfterWildcardBindOrUnconnectedSend) {
    FdGuard rx(Bound(SOCK_RAW, "fe80::92b", b));
    for (unsigned mode : {0U, 1U, 2U}) {
        SCOPED_TRACE("mode=" + std::to_string(mode));
        FdGuard tx(mode == 1 ? Bound(SOCK_RAW, "::", 0) : socket(AF_INET6, SOCK_RAW, 253));
        ASSERT_GE(tx.Get(), 0);
        if (mode == 2) { Send(tx.Get(), a, 0); Receive(rx.Get(), b); }
        auto destination = Address("fe80::92b", a);
        ASSERT_EQ(connect(tx.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0);
        sockaddr_in6 local{}; socklen_t length = sizeof(local);
        ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
        auto expected = Address("fe80::92a", a);
        EXPECT_EQ(memcmp(&local.sin6_addr, &expected.sin6_addr, sizeof(in6_addr)), 0);
        EXPECT_EQ(local.sin6_scope_id, a);
        ASSERT_EQ(send(tx.Get(), "S", 1, 0), 1);
        Receive(rx.Get(), b);
    }
}
TEST_F(Ipv6ScopeNetwork, RawWildcardBindConnectsToBothLoopbackFamilies) {
    for (int family : {AF_INET, AF_INET6}) {
        SCOPED_TRACE("family=" + std::to_string(family));
        FdGuard tx(socket(family, SOCK_RAW, 253)); ASSERT_GE(tx.Get(), 0);
        sockaddr_storage local{}, remote{}; socklen_t size;
        if (family == AF_INET6) {
            auto any = Address("::", 0), loopback = Address("::1", 0);
            memcpy(&local, &any, sizeof(any)); memcpy(&remote, &loopback, sizeof(loopback));
            size = sizeof(any);
        } else {
            auto* any = reinterpret_cast<sockaddr_in*>(&local);
            auto* loopback = reinterpret_cast<sockaddr_in*>(&remote);
            any->sin_family = AF_INET; loopback->sin_family = AF_INET;
            loopback->sin_addr.s_addr = htonl(INADDR_LOOPBACK); size = sizeof(*any);
        }
        ASSERT_EQ(bind(tx.Get(), reinterpret_cast<sockaddr*>(&local), size), 0);
        ASSERT_EQ(connect(tx.Get(), reinterpret_cast<sockaddr*>(&remote), size), 0) << ErrnoString(errno);
        ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&local), &size), 0);
        if (family == AF_INET6) {
            EXPECT_EQ(memcmp(&reinterpret_cast<sockaddr_in6*>(&local)->sin6_addr,
                &reinterpret_cast<sockaddr_in6*>(&remote)->sin6_addr, sizeof(in6_addr)), 0);
            EXPECT_EQ(reinterpret_cast<sockaddr_in6*>(&local)->sin6_scope_id, 0U);
        } else {
            EXPECT_EQ(reinterpret_cast<sockaddr_in*>(&local)->sin_addr.s_addr, htonl(INADDR_LOOPBACK));
        }
        ASSERT_EQ(send(tx.Get(), "S", 1, 0), 1);
        pollfd ready{tx.Get(), POLLIN, 0}; ASSERT_EQ(poll(&ready, 1, 2000), 1);
        unsigned char bytes[128]{}; const auto received = recv(tx.Get(), bytes, sizeof(bytes), 0);
        ASSERT_GT(received, 0); EXPECT_EQ(bytes[received - 1], 'S');
    }
}
TEST_F(Ipv6ScopeNetwork, RawWildcardConnectSelectsNonScopedIpv6Source) {
    ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        a, "fd92::a", 64, ++seq), 0);
    ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
        NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
        b, "fd92::b", 64, ++seq), 0);
    FdGuard rx(Bound(SOCK_RAW, "fd92::b", 0));
    FdGuard tx(Bound(SOCK_RAW, "::", 0));
    ASSERT_EQ(setsockopt(tx.Get(), SOL_SOCKET, SO_BINDTODEVICE, "sc92a", 6), 0);
    auto destination = Address("fd92::b", 0);
    ASSERT_EQ(connect(tx.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0);
    sockaddr_in6 local{}; socklen_t size = sizeof(local);
    ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&local), &size), 0);
    // Both addresses are local in this namespace. Linux's weak-host source
    // policy may prefer the destination itself even with an output device;
    // the contract here is a concrete source, consistently used on the wire.
    EXPECT_FALSE(IN6_IS_ADDR_UNSPECIFIED(&local.sin6_addr));
    EXPECT_EQ(local.sin6_scope_id, 0U);
    ASSERT_EQ(send(tx.Get(), "S", 1, 0), 1);
    pollfd ready{rx.Get(), POLLIN, 0}; ASSERT_EQ(poll(&ready, 1, 2000), 1);
    char byte = 0; sockaddr_in6 source{}; size = sizeof(source);
    ASSERT_EQ(recvfrom(rx.Get(), &byte, 1, 0, reinterpret_cast<sockaddr*>(&source), &size), 1);
    EXPECT_EQ(byte, 'S'); EXPECT_EQ(source.sin6_scope_id, 0U);
    EXPECT_EQ(memcmp(&source.sin6_addr, &local.sin6_addr, sizeof(in6_addr)), 0);
}
TEST_F(Ipv6ScopeNetwork, ConcurrentRawBindsPublishMatchingAddressAndScope) {
    for (unsigned round = 0; round < 64; ++round) {
        FdGuard fd(socket(AF_INET6, SOCK_RAW, 253)); ASSERT_GE(fd.Get(), 0);
        auto first = Address("fe80::92a", a), second = Address("fe80::92b", b);
        std::atomic<unsigned> ready{0}; std::atomic<bool> go{false};
        int results[2]{}, errors[2]{};
        auto attempt = [&](unsigned slot, sockaddr_in6 address) {
            ready.fetch_add(1);
            while (!go.load()) std::this_thread::yield();
            results[slot] = bind(fd.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address));
            errors[slot] = errno;
        };
        std::thread left(attempt, 0, first), right(attempt, 1, second);
        while (ready.load() != 2) std::this_thread::yield();
        go.store(true); left.join(); right.join();
        ASSERT_TRUE(results[0] == 0 || results[1] == 0);
        for (unsigned slot = 0; slot < 2; ++slot) {
            if (results[slot] != 0) EXPECT_EQ(errors[slot], EINVAL);
        }
        sockaddr_in6 local{}; socklen_t size = sizeof(local);
        ASSERT_EQ(getsockname(fd.Get(), reinterpret_cast<sockaddr*>(&local), &size), 0);
        ASSERT_TRUE(local.sin6_scope_id == a || local.sin6_scope_id == b);
        const auto& expected = local.sin6_scope_id == a ? first : second;
        EXPECT_EQ(memcmp(&local.sin6_addr, &expected.sin6_addr, sizeof(in6_addr)), 0);
    }
}
TEST_F(Ipv6ScopeNetwork, SocketScopeUsesItsOwnNamespaceAfterCallerSetns) {
    FdGuard original(open("/proc/self/ns/net", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(original.Get(), 0);
    FdGuard rx(Bound(SOCK_DGRAM, "fe80::92b", b, 31099));
    FdGuard tx(socket(AF_INET6, SOCK_DGRAM, 0));
    ASSERT_GE(tx.Get(), 0);
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    Send(tx.Get(), a, 31099); Receive(rx.Get(), b);
    auto destination = Address("fe80::92b", a, 31099);
    EXPECT_EQ(connect(tx.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0)
        << ErrnoString(errno);
    sockaddr_in6 peer{}; socklen_t length = sizeof(peer);
    EXPECT_EQ(getpeername(tx.Get(), reinterpret_cast<sockaddr*>(&peer), &length), 0);
    EXPECT_EQ(peer.sin6_scope_id, a);
    EXPECT_EQ(setns(original.Get(), CLONE_NEWNET), 0);
}
TEST_F(Ipv6ScopeNetwork, DeletedDeviceKeepsNumericNameButCannotResolveOption) {
    FdGuard tx(Bound(SOCK_DGRAM, "fe80::92a", a));
    FdGuard rx(Bound(SOCK_DGRAM, "fe80::92b", d, 31100));
    ASSERT_EQ(Delete(a), 0);
    sockaddr_in6 name{}; socklen_t length = sizeof(name);
    ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&name), &length), 0);
    EXPECT_EQ(name.sin6_scope_id, a);
    char device[IFNAMSIZ]{}; length = sizeof(device);
    EXPECT_EQ(getsockopt(tx.Get(), SOL_SOCKET, SO_BINDTODEVICE, device, &length), -1);
    EXPECT_EQ(errno, ENODEV);
    // Same source address exists on C; a per-message scope bypasses stale A.
    Send(tx.Get(), c, 31100); Receive(rx.Get(), d);
    length = sizeof(name);
    ASSERT_EQ(getsockname(tx.Get(), reinterpret_cast<sockaddr*>(&name), &length), 0);
    EXPECT_EQ(name.sin6_scope_id, a);
}
TEST_F(Ipv6ScopeNetwork, MovedDeviceDoesNotResolveInSocketOriginalNamespace) {
    FdGuard udp(Bound(SOCK_DGRAM, "fe80::92a", a));
    FdGuard tcp(Bound(SOCK_STREAM, "fe80::92a", a));
    FdGuard raw(Bound(SOCK_RAW, "fe80::92a", a));
    FdGuard original(open("/proc/self/ns/net", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(original.Get(), 0);
    ASSERT_EQ(unshare(CLONE_NEWNET), 0);
    FdGuard destination(open("/proc/self/ns/net", O_RDONLY | O_CLOEXEC));
    ASSERT_GE(destination.Get(), 0);
    ASSERT_EQ(setns(original.Get(), CLONE_NEWNET), 0);
    std::vector<uint8_t> message(NLMSG_SPACE(sizeof(ifinfomsg)), 0);
    const int namespace_fd = destination.Get();
    AppendScopeAttr(message, IFLA_NET_NS_FD, &namespace_fd, sizeof(namespace_fd));
    auto* header = reinterpret_cast<nlmsghdr*>(message.data());
    header->nlmsg_len = message.size(); header->nlmsg_type = RTM_NEWLINK;
    header->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK; header->nlmsg_seq = ++seq;
    reinterpret_cast<ifinfomsg*>(NLMSG_DATA(header))->ifi_index = a;
    ASSERT_EQ(send(route.Get(), message.data(), message.size(), 0), message.size());
    ASSERT_EQ(RecvAck(route.Get(), seq), 0);
    // Even after entering the target namespace, the old sockets must not
    // resolve their numeric scope against the caller's namespace devices.
    ASSERT_EQ(setns(destination.Get(), CLONE_NEWNET), 0);
    for (int fd : {udp.Get(), tcp.Get(), raw.Get()}) {
        sockaddr_in6 local{}; socklen_t size = sizeof(local);
        ASSERT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(&local), &size), 0);
        EXPECT_EQ(local.sin6_scope_id, a);
        char name[IFNAMSIZ]{}; size = sizeof(name);
        EXPECT_EQ(getsockopt(fd, SOL_SOCKET, SO_BINDTODEVICE, name, &size), -1);
        EXPECT_EQ(errno, ENODEV);
    }
    EXPECT_EQ(setns(original.Get(), CLONE_NEWNET), 0);
}
TEST_F(Ipv6ScopeNetwork, PartialInputScopeTailUsesExistingDevice) {
    FdGuard rx(Bound(SOCK_DGRAM, "fe80::92b", b, 31101));
    for (socklen_t length : {24U, 25U, 26U, 27U}) {
        FdGuard tx(socket(AF_INET6, SOCK_DGRAM, 0));
        ASSERT_GE(tx.Get(), 0);
        ASSERT_EQ(setsockopt(tx.Get(), SOL_SOCKET, SO_BINDTODEVICE, "sc92a", 6), 0);
        auto destination = Address("fe80::92b", c, 31101);
        ASSERT_EQ(sendto(tx.Get(), "S", 1, 0, reinterpret_cast<sockaddr*>(&destination), length), 1)
            << ErrnoString(errno);
        Receive(rx.Get(), b);
        ASSERT_EQ(connect(tx.Get(), reinterpret_cast<sockaddr*>(&destination), length), 0)
            << ErrnoString(errno);
        sockaddr_in6 peer{}; socklen_t size = sizeof(peer);
        ASSERT_EQ(getpeername(tx.Get(), reinterpret_cast<sockaddr*>(&peer), &size), 0);
        EXPECT_EQ(peer.sin6_scope_id, a);
    }
}
TEST_F(Ipv6ScopeNetwork, DatagramAndRawTruncatedReceiveAddressesRespectGuardPages) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* memory = static_cast<unsigned char*>(mmap(nullptr, page * 2,
        PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(memory, MAP_FAILED);
    ASSERT_EQ(mprotect(memory + page, page, PROT_NONE), 0);
    for (int type : {SOCK_DGRAM, SOCK_RAW}) {
        FdGuard rx(Bound(type, "fe80::92b", b, type == SOCK_DGRAM ? 31102 : 0));
        FdGuard tx(socket(AF_INET6, type, type == SOCK_RAW ? 253 : 0));
        ASSERT_GE(tx.Get(), 0);
        for (socklen_t size : {0U, 1U, 23U, 24U, 25U, 27U, 28U}) {
            for (bool message : {false, true}) {
                SCOPED_TRACE("type=" + std::to_string(type) + " size=" + std::to_string(size));
                Send(tx.Get(), a, type == SOCK_DGRAM ? 31102 : 0);
                pollfd ready{rx.Get(), POLLIN, 0}; ASSERT_EQ(poll(&ready, 1, 2000), 1);
                char byte = 0; socklen_t actual = size;
                auto* address = reinterpret_cast<sockaddr*>(memory + page - size);
                if (message) {
                    iovec vector{&byte, 1}; msghdr header{};
                    header.msg_name = address; header.msg_namelen = size;
                    header.msg_iov = &vector; header.msg_iovlen = 1;
                    ASSERT_EQ(recvmsg(rx.Get(), &header, 0), 1) << ErrnoString(errno);
                    actual = header.msg_namelen;
                } else {
                    ASSERT_EQ(recvfrom(rx.Get(), &byte, 1, 0, address, &actual), 1)
                        << ErrnoString(errno);
                }
                EXPECT_EQ(byte, 'S'); EXPECT_EQ(actual, 28U);
                if (size > 24) EXPECT_EQ(memcmp(memory + page - size + 24, &b, size - 24), 0);
            }
        }
    }
    EXPECT_EQ(munmap(memory, page * 2), 0);
}
TEST_F(Ipv6ScopeNetwork, WildcardReceiversUseActualIngressNotSocketBinding) {
    for (int type : {SOCK_DGRAM, SOCK_RAW}) {
        SCOPED_TRACE("type=" + std::to_string(type));
        FdGuard rx(Bound(type, "::", 0, type == SOCK_DGRAM ? 31103 : 0));
        FdGuard tx(socket(AF_INET6, type, type == SOCK_RAW ? 253 : 0));
        ASSERT_GE(tx.Get(), 0);
        Send(tx.Get(), a, type == SOCK_DGRAM ? 31103 : 0); Receive(rx.Get(), b);
        Send(tx.Get(), c, type == SOCK_DGRAM ? 31103 : 0, true); Receive(rx.Get(), d, true);
    }
}
TEST_F(Ipv6ScopeNetwork, RawHdrinclRespectsPerMessageScopeOnBothLinks) {
    FdGuard first(Bound(SOCK_RAW, "fe80::92b", b));
    FdGuard second(Bound(SOCK_RAW, "fe80::92b", d));
    FdGuard tx(socket(AF_INET6, SOCK_RAW, 253)); ASSERT_GE(tx.Get(), 0);
    const int enabled = 1;
    ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_HDRINCL, &enabled, sizeof(enabled)), 0);
    unsigned char packet[41]{};
    packet[0] = 0x60; packet[5] = 1; packet[6] = 253; packet[7] = 64; packet[40] = 'S';
    auto source = Address("fe80::92a", 0), destination = Address("fe80::92b", a);
    memcpy(packet + 8, &source.sin6_addr, 16); memcpy(packet + 24, &destination.sin6_addr, 16);
    ASSERT_EQ(sendto(tx.Get(), packet, sizeof(packet), 0,
        reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), sizeof(packet));
    Receive(first.Get(), b);
    destination.sin6_scope_id = c;
    iovec vector{packet, sizeof(packet)}; msghdr message{};
    message.msg_name = &destination; message.msg_namelen = sizeof(destination);
    message.msg_iov = &vector; message.msg_iovlen = 1;
    ASSERT_EQ(sendmsg(tx.Get(), &message, 0), sizeof(packet));
    Receive(second.Get(), d, true);
}
TEST_F(Ipv6ScopeNetwork, ErrorQueueDestinationAndOffenderUseIcmpIngressScope) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* memory = static_cast<unsigned char*>(mmap(nullptr, page * 2,
        PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(memory, MAP_FAILED);
    ASSERT_EQ(mprotect(memory + page, page, PROT_NONE), 0);
    FdGuard sink(Bound(SOCK_DGRAM, "fe80::92b", b, 31200));
    FdGuard capture(socket(AF_PACKET, SOCK_DGRAM | SOCK_NONBLOCK, htons(ETH_P_IPV6)));
    ASSERT_GE(capture.Get(), 0);
    sockaddr_ll link{}; link.sll_family = AF_PACKET;
    link.sll_protocol = htons(ETH_P_IPV6); link.sll_ifindex = b;
    ASSERT_EQ(bind(capture.Get(), reinterpret_cast<sockaddr*>(&link), sizeof(link)), 0);
    FdGuard feedback(socket(AF_INET6, SOCK_RAW, IPPROTO_ICMPV6));
    ASSERT_GE(feedback.Get(), 0);
    auto reporter = Address("fe80::92b", b);
    ASSERT_EQ(bind(feedback.Get(), reinterpret_cast<sockaddr*>(&reporter), sizeof(reporter)), 0);
    for (socklen_t size : {24U, 25U, 27U, 28U}) {
        FdGuard tx(Bound(SOCK_DGRAM, "fe80::92a", a));
        ASSERT_GE(tx.Get(), 0);
        const int enabled = 1;
        ASSERT_EQ(setsockopt(tx.Get(), IPPROTO_IPV6, IPV6_RECVERR, &enabled, sizeof(enabled)), 0);
        Send(tx.Get(), a, 31200);
        // Quote the actual transmitted UDP packet, not a fabricated tuple.
        unsigned char packet[4096]{}; ssize_t captured = -1;
        for (unsigned attempt = 0; attempt < 128; ++attempt) {
            pollfd input{capture.Get(), POLLIN, 0};
            ASSERT_EQ(poll(&input, 1, 2000), 1);
            captured = recv(capture.Get(), packet, sizeof(packet), MSG_DONTWAIT);
            ASSERT_GE(captured, 40);
            if (packet[6] == IPPROTO_UDP) break;
        }
        ASSERT_EQ(packet[6], IPPROTO_UDP);
        ASSERT_GE(captured, 49);
        std::vector<unsigned char> error(8 + captured, 0);
        error[0] = 2; error[6] = 5;  // ICMPv6 Packet Too Big, MTU 1280.
        memcpy(error.data() + 8, packet, captured);
        auto destination = Address("fe80::92a", b);
        ASSERT_EQ(sendto(feedback.Get(), error.data(), error.size(), 0,
            reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), error.size());
        pollfd ready{tx.Get(), POLLERR, 0}; ASSERT_EQ(poll(&ready, 1, 2000), 1);
        char payload[64]{}; alignas(cmsghdr) unsigned char control[256]{};
        iovec vector{payload, sizeof(payload)}; msghdr message{};
        message.msg_name = memory + page - size; message.msg_namelen = size;
        message.msg_iov = &vector; message.msg_iovlen = 1;
        message.msg_control = control; message.msg_controllen = sizeof(control);
        ASSERT_GE(recvmsg(tx.Get(), &message, MSG_ERRQUEUE | MSG_DONTWAIT), 0) << ErrnoString(errno);
        EXPECT_EQ(message.msg_namelen, 28U);
        if (size > 24) EXPECT_EQ(memcmp(memory + page - size + 24, &a, size - 24), 0);
        bool found = false;
        for (cmsghdr* header = CMSG_FIRSTHDR(&message); header;
             header = CMSG_NXTHDR(&message, header)) {
            if (header->cmsg_level != IPPROTO_IPV6 || header->cmsg_type != IPV6_RECVERR) continue;
            ASSERT_GE(header->cmsg_len, CMSG_LEN(sizeof(sock_extended_err) + sizeof(sockaddr_in6)));
            const auto* error = reinterpret_cast<const sock_extended_err*>(CMSG_DATA(header));
            const auto* offender = reinterpret_cast<const sockaddr_in6*>(error + 1);
            EXPECT_EQ(error->ee_origin, SO_EE_ORIGIN_ICMP6);
            EXPECT_EQ(offender->sin6_scope_id, a);
            found = true;
        }
        EXPECT_TRUE(found);
    }
    EXPECT_EQ(munmap(memory, page * 2), 0);
}

}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
