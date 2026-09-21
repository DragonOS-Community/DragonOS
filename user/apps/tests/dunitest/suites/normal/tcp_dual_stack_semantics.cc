#include <gtest/gtest.h>

#include <cstring>

#include <arpa/inet.h>
#include <errno.h>
#include <ifaddrs.h>
#include <poll.h>
#include <sys/socket.h>
#include <unistd.h>

namespace {
class Fd {
  public:
    explicit Fd(int fd) : fd_(fd) {}
    ~Fd() { Reset(); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int Get() const { return fd_; }
    void Reset() {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = -1;
    }
  private:
    int fd_;
};

sockaddr_in V4(uint16_t port, bool loopback = false) {
    sockaddr_in a {};
    a.sin_family = AF_INET;
    a.sin_port = htons(port);
    a.sin_addr.s_addr = htonl(loopback ? INADDR_LOOPBACK : INADDR_ANY);
    return a;
}

sockaddr_in6 V6(uint16_t port, bool loopback = false) {
    sockaddr_in6 a {};
    a.sin6_family = AF_INET6;
    a.sin6_port = htons(port);
    a.sin6_addr = loopback ? in6addr_loopback : in6addr_any;
    return a;
}

int Bind(int fd, int family, uint16_t port) {
    if (family == AF_INET) {
        auto a = V4(port);
        return bind(fd, reinterpret_cast<sockaddr*>(&a), sizeof(a));
    }
    auto a = V6(port);
    return bind(fd, reinterpret_cast<sockaddr*>(&a), sizeof(a));
}

uint16_t Port(int fd, int family) {
    sockaddr_storage a {};
    socklen_t len = sizeof(a);
    EXPECT_EQ(getsockname(fd, reinterpret_cast<sockaddr*>(&a), &len), 0);
    EXPECT_EQ(a.ss_family, family);
    if (family == AF_INET) return ntohs(reinterpret_cast<sockaddr_in*>(&a)->sin_port);
    return ntohs(reinterpret_cast<sockaddr_in6*>(&a)->sin6_port);
}

int Only(int fd, int value) {
    return setsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &value, sizeof(value));
}

int GetOnly(int fd) {
    int value = -1;
    socklen_t len = sizeof(value);
    EXPECT_EQ(getsockopt(fd, IPPROTO_IPV6, IPV6_V6ONLY, &value, &len), 0);
    EXPECT_EQ(len, sizeof(value));
    return value;
}

// Nonblocking connect and bounded poll keep a broken demultiplexer from hanging CI.
bool Connect(int fd, int family, uint16_t port) {
    int rc;
    if (family == AF_INET) {
        auto a = V4(port, true);
        rc = connect(fd, reinterpret_cast<sockaddr*>(&a), sizeof(a));
    } else {
        auto a = V6(port, true);
        rc = connect(fd, reinterpret_cast<sockaddr*>(&a), sizeof(a));
    }
    if (rc == 0) return true;
    if (errno != EINPROGRESS) return false;
    pollfd p {fd, POLLOUT, 0};
    if (poll(&p, 1, 3000) != 1) return false;
    int error = -1;
    socklen_t len = sizeof(error);
    return getsockopt(fd, SOL_SOCKET, SO_ERROR, &error, &len) == 0 && error == 0;
}

void CheckName(int fd, bool peer, int family, bool mapped) {
    sockaddr_storage a {};
    socklen_t len = sizeof(a);
    ASSERT_EQ(peer ? getpeername(fd, reinterpret_cast<sockaddr*>(&a), &len)
                   : getsockname(fd, reinterpret_cast<sockaddr*>(&a), &len), 0);
    ASSERT_EQ(a.ss_family, family);
    if (family == AF_INET6) {
        EXPECT_EQ(len, sizeof(sockaddr_in6));
        auto* v6 = reinterpret_cast<sockaddr_in6*>(&a);
        EXPECT_EQ(!!IN6_IS_ADDR_V4MAPPED(&v6->sin6_addr), mapped);
    }
}
}  // namespace

TEST(TcpDualStack, V6OnlyOptionAndBoundImmutability) {
    Fd s(socket(AF_INET6, SOCK_STREAM, 0));
    ASSERT_GE(s.Get(), 0);
    EXPECT_EQ(GetOnly(s.Get()), 0);
    ASSERT_EQ(Only(s.Get(), 7), 0);
    EXPECT_EQ(GetOnly(s.Get()), 1);
    int zero = 0;
    EXPECT_EQ(setsockopt(s.Get(), IPPROTO_IPV6, IPV6_V6ONLY, &zero, 1), -1);
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(GetOnly(s.Get()), 1);
    ASSERT_EQ(Bind(s.Get(), AF_INET6, 0), 0);
    EXPECT_EQ(Only(s.Get(), 0), -1);
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(GetOnly(s.Get()), 1);
}

TEST(TcpDualStack, IndependentBindingsInBothOrdersAndCloseIsolation) {
    for (bool v6_first : {false, true}) {
        SCOPED_TRACE(v6_first);
        Fd v4(socket(AF_INET, SOCK_STREAM, 0));
        Fd v6(socket(AF_INET6, SOCK_STREAM, 0));
        ASSERT_GE(v4.Get(), 0);
        ASSERT_GE(v6.Get(), 0);
        ASSERT_EQ(Only(v6.Get(), 1), 0);
        int first = v6_first ? v6.Get() : v4.Get();
        int second = v6_first ? v4.Get() : v6.Get();
        ASSERT_EQ(Bind(first, v6_first ? AF_INET6 : AF_INET, 0), 0);
        auto port = Port(first, v6_first ? AF_INET6 : AF_INET);
        ASSERT_NE(port, 0);
        ASSERT_EQ(Bind(second, v6_first ? AF_INET : AF_INET6, port), 0) << errno;
        EXPECT_EQ(Port(second, v6_first ? AF_INET : AF_INET6), port);
        // Exercise both release directions, not merely IPv4 followed by IPv6.
        if (v6_first) {
            v6.Reset();
        } else {
            v4.Reset();
        }
        int remaining_family = v6_first ? AF_INET : AF_INET6;
        Fd duplicate(socket(remaining_family, SOCK_STREAM, 0));
        ASSERT_GE(duplicate.Get(), 0);
        if (remaining_family == AF_INET6) {
            ASSERT_EQ(Only(duplicate.Get(), 1), 0);
        }
        EXPECT_EQ(Bind(duplicate.Get(), remaining_family, port), -1);
        EXPECT_EQ(errno, EADDRINUSE);
        v4.Reset();
        v6.Reset();
        EXPECT_EQ(Bind(duplicate.Get(), remaining_family, port), 0);
    }
}

TEST(TcpDualStack, DefaultDualStackConflictsInBothOrders) {
    for (bool v6_first : {false, true}) {
        Fd v4(socket(AF_INET, SOCK_STREAM, 0));
        Fd v6(socket(AF_INET6, SOCK_STREAM, 0));
        ASSERT_GE(v4.Get(), 0);
        ASSERT_GE(v6.Get(), 0);
        ASSERT_EQ(Only(v6.Get(), 0), 0);
        int first = v6_first ? v6.Get() : v4.Get();
        int second = v6_first ? v4.Get() : v6.Get();
        ASSERT_EQ(Bind(first, v6_first ? AF_INET6 : AF_INET, 0), 0);
        auto port = Port(first, v6_first ? AF_INET6 : AF_INET);
        EXPECT_EQ(Bind(second, v6_first ? AF_INET : AF_INET6, port), -1);
        EXPECT_EQ(errno, EADDRINUSE);
        // Failure must not poison the socket or leak a reservation.
        EXPECT_EQ(Bind(second, v6_first ? AF_INET : AF_INET6, 0), 0);
    }
}

TEST(TcpDualStack, SeparateListenersDispatchAndRefill) {
    Fd v4(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
    Fd v6(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(v4.Get(), 0);
    ASSERT_GE(v6.Get(), 0);
    ASSERT_EQ(Only(v6.Get(), 1), 0);
    ASSERT_EQ(Bind(v4.Get(), AF_INET, 0), 0);
    auto port = Port(v4.Get(), AF_INET);
    ASSERT_EQ(Bind(v6.Get(), AF_INET6, port), 0);
    ASSERT_EQ(listen(v4.Get(), 2), 0);
    ASSERT_EQ(listen(v6.Get(), 2), 0);
    EXPECT_EQ(Port(v6.Get(), AF_INET6), port);
    // Exceed both requested backlog and DragonOS's current eight-slot cap.
    for (int i = 0; i < 12; ++i) {
        for (int family : {AF_INET, AF_INET6}) {
            SCOPED_TRACE(i);
            SCOPED_TRACE(family);
            Fd client(socket(family, SOCK_STREAM | SOCK_NONBLOCK, 0));
            ASSERT_TRUE(Connect(client.Get(), family, port)) << errno;
            pollfd p[2] {{v4.Get(), POLLIN, 0}, {v6.Get(), POLLIN, 0}};
            ASSERT_EQ(poll(p, 2, 3000), 1);
            int index = family == AF_INET ? 0 : 1;
            EXPECT_EQ(p[1 - index].revents & POLLIN, 0);
            ASSERT_NE(p[index].revents & POLLIN, 0);
            Fd child(accept(p[index].fd, nullptr, nullptr));
            ASSERT_GE(child.Get(), 0) << errno;
            CheckName(child.Get(), false, family, false);
            CheckName(child.Get(), true, family, false);
            if (family == AF_INET6) {
                EXPECT_EQ(GetOnly(child.Get()), 1);
            }
            const char marker = static_cast<char>(i + 1);
            ASSERT_EQ(send(client.Get(), &marker, 1, MSG_NOSIGNAL), 1);
            pollfd readable {child.Get(), POLLIN, 0};
            ASSERT_EQ(poll(&readable, 1, 3000), 1);
            char received = 0;
            ASSERT_EQ(recv(child.Get(), &received, 1, MSG_DONTWAIT), 1);
            EXPECT_EQ(received, marker);
        }
    }
}

TEST(TcpDualStack, DualStackAcceptedIpv4UsesMappedNames) {
    Fd listener(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(listener.Get(), 0);
    ASSERT_EQ(Only(listener.Get(), 0), 0);
    ASSERT_EQ(Bind(listener.Get(), AF_INET6, 0), 0);
    auto port = Port(listener.Get(), AF_INET6);
    ASSERT_EQ(listen(listener.Get(), 2), 0);
    Fd client(socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_TRUE(Connect(client.Get(), AF_INET, port)) << errno;
    pollfd p {listener.Get(), POLLIN, 0};
    ASSERT_EQ(poll(&p, 1, 3000), 1);
    sockaddr_in6 peer {};
    socklen_t len = sizeof(peer);
    Fd child(accept(listener.Get(), reinterpret_cast<sockaddr*>(&peer), &len));
    ASSERT_GE(child.Get(), 0) << errno;
    EXPECT_EQ(len, sizeof(peer));
    EXPECT_EQ(peer.sin6_family, AF_INET6);
    EXPECT_TRUE(IN6_IS_ADDR_V4MAPPED(&peer.sin6_addr));
    CheckName(child.Get(), false, AF_INET6, true);
    CheckName(child.Get(), true, AF_INET6, true);
    EXPECT_EQ(GetOnly(child.Get()), 0);
}

TEST(TcpDualStack, NativeBindSetsV6OnlyOnlyAfterSuccess) {
    Fd owner(socket(AF_INET6, SOCK_STREAM, 0));
    ASSERT_GE(owner.Get(), 0);
    ASSERT_EQ(Only(owner.Get(), 0), 0);
    auto addr = V6(0, true);
    ASSERT_EQ(bind(owner.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0);
    EXPECT_EQ(GetOnly(owner.Get()), 1);
    auto port = Port(owner.Get(), AF_INET6);
    ASSERT_NE(port, 0);

    Fd retry(socket(AF_INET6, SOCK_STREAM, 0));
    ASSERT_GE(retry.Get(), 0);
    ASSERT_EQ(Only(retry.Get(), 0), 0);
    addr.sin6_port = htons(port);
    ASSERT_EQ(bind(retry.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), -1);
    EXPECT_EQ(errno, EADDRINUSE);
    EXPECT_EQ(GetOnly(retry.Get()), 0);
    owner.Reset();
    ASSERT_EQ(bind(retry.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0);
    EXPECT_EQ(GetOnly(retry.Get()), 1);
}

TEST(TcpDualStack, Ipv4RejectsIpv6OnlyOption) {
    Fd v4(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(v4.Get(), 0);
    int value = 1;
    socklen_t len = sizeof(value);
    EXPECT_EQ(setsockopt(v4.Get(), IPPROTO_IPV6, IPV6_V6ONLY, &value, len), -1);
    EXPECT_EQ(errno, ENOPROTOOPT);
    EXPECT_EQ(getsockopt(v4.Get(), IPPROTO_IPV6, IPV6_V6ONLY, &value, &len), -1);
    EXPECT_EQ(errno, EOPNOTSUPP);
}

TEST(TcpDualStack, MappedBindSharesIpv4DomainAndPreservesSockaddr) {
    Fd mapped(socket(AF_INET6, SOCK_STREAM, 0));
    ASSERT_GE(mapped.Get(), 0);
    ASSERT_EQ(Only(mapped.Get(), 0), 0);
    auto addr = V6(0);
    ASSERT_EQ(inet_pton(AF_INET6, "::ffff:127.0.0.1", &addr.sin6_addr), 1);
    ASSERT_EQ(bind(mapped.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), 0);
    CheckName(mapped.Get(), false, AF_INET6, true);
    EXPECT_EQ(GetOnly(mapped.Get()), 0);
    auto port = Port(mapped.Get(), AF_INET6);
    ASSERT_NE(port, 0);
    Fd v4(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(v4.Get(), 0);
    auto addr4 = V4(port, true);
    EXPECT_EQ(bind(v4.Get(), reinterpret_cast<sockaddr*>(&addr4), sizeof(addr4)), -1);
    EXPECT_EQ(errno, EADDRINUSE);

    Fd only(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(only.Get(), 0);
    ASSERT_EQ(Only(only.Get(), 1), 0);
    // Port zero avoids conflating V6ONLY rejection with a port collision.
    ASSERT_EQ(bind(only.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), -1);
    EXPECT_EQ(errno, EINVAL);
    EXPECT_EQ(GetOnly(only.Get()), 1);
    addr.sin6_port = htons(port);
    EXPECT_EQ(connect(only.Get(), reinterpret_cast<sockaddr*>(&addr), sizeof(addr)), -1);
    EXPECT_EQ(errno, ENETUNREACH);
    ASSERT_EQ(Bind(only.Get(), AF_INET6, 0), 0);
}

TEST(TcpDualStack, ImplicitListenPreservesIpv6OnlyAndFamily) {
    Fd listener(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(listener.Get(), 0);
    ASSERT_EQ(Only(listener.Get(), 1), 0);
    ASSERT_EQ(listen(listener.Get(), 2), 0);
    auto port = Port(listener.Get(), AF_INET6);
    ASSERT_NE(port, 0);
    EXPECT_EQ(GetOnly(listener.Get()), 1);
    Fd v4(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(v4.Get(), 0);
    ASSERT_EQ(Bind(v4.Get(), AF_INET, port), 0);
    Fd client(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_TRUE(Connect(client.Get(), AF_INET6, port)) << errno;
    pollfd p {listener.Get(), POLLIN, 0};
    ASSERT_EQ(poll(&p, 1, 3000), 1);
    Fd child(accept(listener.Get(), nullptr, nullptr));
    ASSERT_GE(child.Get(), 0);
    CheckName(child.Get(), false, AF_INET6, false);
    EXPECT_EQ(GetOnly(child.Get()), 1);
}

TEST(TcpDualStack, DistinctAssignedIpv4AddressesSharePortButWildcardConflicts) {
    // Use real interface addresses, not an assumption that all of 127/8 is local.
    ifaddrs* interfaces = nullptr;
    ASSERT_EQ(getifaddrs(&interfaces), 0);
    sockaddr_in addresses[2] {};
    int count = 0;
    for (auto* iface = interfaces; iface != nullptr && count < 2; iface = iface->ifa_next) {
        if (iface->ifa_addr == nullptr || iface->ifa_addr->sa_family != AF_INET) continue;
        auto addr = *reinterpret_cast<sockaddr_in*>(iface->ifa_addr);
        if (addr.sin_addr.s_addr == INADDR_ANY ||
            (count != 0 && addr.sin_addr.s_addr == addresses[0].sin_addr.s_addr)) continue;
        addr.sin_port = 0;
        addresses[count++] = addr;
    }
    freeifaddrs(interfaces);
    if (count != 2) {
        GTEST_SKIP() << "Requires two assigned IPv4 addresses";
    }
    Fd first(socket(AF_INET, SOCK_STREAM, 0));
    Fd second(socket(AF_INET, SOCK_STREAM, 0));
    Fd wildcard(socket(AF_INET, SOCK_STREAM, 0));
    ASSERT_GE(first.Get(), 0);
    ASSERT_GE(second.Get(), 0);
    ASSERT_GE(wildcard.Get(), 0);
    ASSERT_EQ(bind(first.Get(), reinterpret_cast<sockaddr*>(&addresses[0]), sizeof(sockaddr_in)), 0);
    auto port = Port(first.Get(), AF_INET);
    ASSERT_NE(port, 0);
    addresses[1].sin_port = htons(port);
    ASSERT_EQ(bind(second.Get(), reinterpret_cast<sockaddr*>(&addresses[1]), sizeof(sockaddr_in)), 0);
    EXPECT_EQ(Bind(wildcard.Get(), AF_INET, port), -1);
    EXPECT_EQ(errno, EADDRINUSE);
    first.Reset();
    EXPECT_EQ(Bind(wildcard.Get(), AF_INET, port), -1);
    EXPECT_EQ(errno, EADDRINUSE);
    second.Reset();
    EXPECT_EQ(Bind(wildcard.Get(), AF_INET, port), 0);
}

TEST(TcpDualStack, NativeIpv4SockaddrIsNotMappedIpv6Input) {
    // Supply enough bytes for sockaddr_in6 so Linux reaches its family check,
    // rather than rejecting a short sockaddr_in with EINVAL first.
    sockaddr_storage address {};
    auto v4 = V4(12345, true);
    memcpy(&address, &v4, sizeof(v4));
    Fd binder(socket(AF_INET6, SOCK_STREAM, 0));
    Fd connector(socket(AF_INET6, SOCK_STREAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(binder.Get(), 0);
    ASSERT_GE(connector.Get(), 0);
    EXPECT_EQ(bind(binder.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(sockaddr_in6)), -1);
    EXPECT_EQ(errno, EAFNOSUPPORT);
    EXPECT_EQ(connect(connector.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(sockaddr_in6)), -1);
    EXPECT_EQ(errno, EAFNOSUPPORT);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
