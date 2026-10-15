#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif
#include <sys/ioctl.h>
#include <poll.h>
#include "rtnetlink_route_test_support.h"

namespace {

// Each case runs in a loopback-only namespace: a physical default route must
// not hide loss of the source-selected output device during PMTU lookup.
class UdpSourceRoute : public testing::TestWithParam<bool> {};

// Exercise the first automatic-bind transaction, not an already-bound sender.
// The outer guest timeout catches a kernel lock stall; socket nonblocking mode
// must not allow a connect/sendto call to sleep in a recursive placement lock.
void ExerciseUnboundDatagram(int family, bool connected) {
    FdGuard receiver(socket(family, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    FdGuard sender(socket(family, SOCK_DGRAM | SOCK_NONBLOCK, 0));
    ASSERT_GE(receiver.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);
    sockaddr_storage address{}, local{};
    socklen_t length;
    if (family == AF_INET) {
        auto* ip = reinterpret_cast<sockaddr_in*>(&address);
        ip->sin_family = AF_INET;
        ip->sin_addr.s_addr = htonl(INADDR_LOOPBACK);
        length = sizeof(*ip);
    } else {
        auto* ip = reinterpret_cast<sockaddr_in6*>(&address);
        ip->sin6_family = AF_INET6;
        ip->sin6_addr = in6addr_loopback;
        length = sizeof(*ip);
    }
    ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&address), length), 0)
        << ErrnoString(errno);
    ASSERT_EQ(getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&address), &length), 0);
    socklen_t local_length = sizeof(local);
    ASSERT_EQ(getsockname(sender.Get(), reinterpret_cast<sockaddr*>(&local), &local_length), 0);
    auto port = [family](const sockaddr_storage& value) {
        return family == AF_INET
            ? reinterpret_cast<const sockaddr_in*>(&value)->sin_port
            : reinterpret_cast<const sockaddr_in6*>(&value)->sin6_port;
    };
    ASSERT_EQ(port(local), 0);
    const char payload[] = "udp-autobind-transaction";
    ssize_t sent;
    if (connected) {
        ASSERT_EQ(connect(sender.Get(), reinterpret_cast<sockaddr*>(&address), length), 0)
            << ErrnoString(errno);
        sent = send(sender.Get(), payload, sizeof(payload), 0);
    } else {
        sent = sendto(sender.Get(), payload, sizeof(payload), 0,
                      reinterpret_cast<sockaddr*>(&address), length);
    }
    ASSERT_EQ(sent, static_cast<ssize_t>(sizeof(payload))) << ErrnoString(errno);
    local_length = sizeof(local);
    ASSERT_EQ(getsockname(sender.Get(), reinterpret_cast<sockaddr*>(&local), &local_length), 0);
    EXPECT_NE(port(local), 0);
    pollfd ready{receiver.Get(), POLLIN, 0};
    ASSERT_EQ(poll(&ready, 1, 5000), 1) << ErrnoString(errno);
    ASSERT_NE(ready.revents & POLLIN, 0);
    char received[sizeof(payload)];
    ASSERT_EQ(recv(receiver.Get(), received, sizeof(received), 0),
              static_cast<ssize_t>(sizeof(payload))) << ErrnoString(errno);
    EXPECT_EQ(memcmp(received, payload, sizeof(payload)), 0);
}

TEST(UdpAutobind, Ipv4UnboundConnect) { ExerciseUnboundDatagram(AF_INET, true); }
TEST(UdpAutobind, Ipv4UnboundSendto) { ExerciseUnboundDatagram(AF_INET, false); }
TEST(UdpAutobind, Ipv6UnboundConnect) { ExerciseUnboundDatagram(AF_INET6, true); }
TEST(UdpAutobind, Ipv6UnboundSendto) { ExerciseUnboundDatagram(AF_INET6, false); }

TEST_P(UdpSourceRoute, BoundSourceSurvivesSendAndConnectedMtuLookup) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        auto run = [&]() {
            ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
            FdGuard sender(socket(AF_INET, SOCK_DGRAM, 0));
            ASSERT_GE(sender.Get(), 0);
            ifreq request{};
            strcpy(request.ifr_name, "lo");
            ASSERT_EQ(ioctl(sender.Get(), SIOCGIFFLAGS, &request), 0);
            request.ifr_flags |= IFF_UP;
            ASSERT_EQ(ioctl(sender.Get(), SIOCSIFFLAGS, &request), 0);

            sockaddr_in local{};
            local.sin_family = AF_INET;
            local.sin_addr.s_addr = htonl(INADDR_LOOPBACK);
            ASSERT_EQ(bind(sender.Get(), reinterpret_cast<sockaddr*>(&local), sizeof(local)), 0);
            const int enabled = 1;
            ASSERT_EQ(setsockopt(sender.Get(), SOL_SOCKET, SO_BROADCAST, &enabled, sizeof(enabled)), 0);
            sockaddr_in destination{};
            destination.sin_family = AF_INET;
            destination.sin_port = htons(12345);
            destination.sin_addr.s_addr = Ipv4(GetParam() ? "255.255.255.255" : "239.1.2.3");
            const char payload[] = "source route";
            ASSERT_EQ(sendto(sender.Get(), payload, sizeof(payload), 0,
                             reinterpret_cast<sockaddr*>(&destination), sizeof(destination)),
                      static_cast<ssize_t>(sizeof(payload))) << ErrnoString(errno);
            ASSERT_EQ(connect(sender.Get(), reinterpret_cast<sockaddr*>(&destination), sizeof(destination)), 0)
                << ErrnoString(errno);
            int mtu = 0;
            socklen_t length = sizeof(mtu);
            ASSERT_EQ(getsockopt(sender.Get(), IPPROTO_IP, IP_MTU, &mtu, &length), 0)
                << ErrnoString(errno);
            EXPECT_GT(mtu, 0);
            EXPECT_EQ(length, sizeof(mtu));
        };
        run();
        _exit(testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

INSTANTIATE_TEST_SUITE_P(Destinations, UdpSourceRoute, testing::Bool());

// Match ip6_dst_lookup_tail: automatic source selection can fail before a
// missing route is reported. Keep missing-source and missing-route errors apart.
class UdpIpv6MissingSource : public testing::TestWithParam<bool> {};

void ExerciseMissingIpv6Source(bool connected, bool remove_address,
                               bool bind_source, bool remote_destination = false) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        auto run = [&]() {
            ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
            FdGuard route(OpenRouteSocket());
            ASSERT_GE(route.Get(), 0);
            unsigned loopback = if_nametoindex("lo");
            ASSERT_NE(loopback, 0u);
            ASSERT_EQ(SetLinkUp(route.Get(), loopback, true, 1), 0);
            FdGuard sender(socket(AF_INET6, SOCK_DGRAM, 0));
            ASSERT_GE(sender.Get(), 0);
            sockaddr_in6 destination{};
            destination.sin6_family = AF_INET6;
            destination.sin6_addr = in6addr_loopback;
            if (bind_source) {
                ASSERT_EQ(bind(sender.Get(), reinterpret_cast<sockaddr*>(&destination),
                               sizeof(destination)), 0) << ErrnoString(errno);
            }
            if (remove_address) {
                ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_DELADDR,
                    NLM_F_REQUEST | NLM_F_ACK, loopback, "::1", 128, 2, RT_SCOPE_HOST), 0);
            }
            if (remote_destination) {
                ASSERT_EQ(inet_pton(AF_INET6, "2001:db8::93", &destination.sin6_addr), 1);
            }
            destination.sin6_port = htons(31093);
            auto output = [&]() -> ssize_t {
                if (connected) {
                    return connect(sender.Get(), reinterpret_cast<sockaddr*>(&destination),
                                   sizeof(destination));
                }
                return sendto(sender.Get(), "S", 1, 0,
                              reinterpret_cast<sockaddr*>(&destination), sizeof(destination));
            };
            ASSERT_EQ(output(), -1);
            ASSERT_EQ(errno, remove_address && !bind_source ? EADDRNOTAVAIL : ENETUNREACH)
                << ErrnoString(errno);
            sockaddr_in6 peer{};
            socklen_t length = sizeof(peer);
            EXPECT_EQ(getpeername(sender.Get(), reinterpret_cast<sockaddr*>(&peer), &length), -1);
            EXPECT_EQ(errno, ENOTCONN);
            if (remove_address && !bind_source && !remote_destination) {
                ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR,
                    NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                    loopback, "::1", 128, 3, RT_SCOPE_HOST), 0);
                FdGuard receiver(socket(AF_INET6, SOCK_DGRAM, 0));
                ASSERT_GE(receiver.Get(), 0);
                ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&destination),
                               sizeof(destination)), 0) << ErrnoString(errno);
                ASSERT_EQ(output(), connected ? 0 : 1) << ErrnoString(errno);
                sockaddr_in6 local{};
                length = sizeof(local);
                ASSERT_EQ(getsockname(sender.Get(), reinterpret_cast<sockaddr*>(&local), &length), 0);
                if (connected) {
                    EXPECT_EQ(memcmp(&local.sin6_addr, &in6addr_loopback, sizeof(in6_addr)), 0);
                }
                EXPECT_NE(local.sin6_port, 0);
            }
        };
        run();
        _exit(testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST_P(UdpIpv6MissingSource, DeletedAddressFailsSourceSelectionThenRecovers) {
    ExerciseMissingIpv6Source(GetParam(), true, false);
}
TEST_P(UdpIpv6MissingSource, ExplicitSourceWithoutRouteFailsRouting) {
    ExerciseMissingIpv6Source(GetParam(), false, true, true);
}
TEST_P(UdpIpv6MissingSource, RemoteDestinationWithoutAnySourceFailsSourceSelection) {
    ExerciseMissingIpv6Source(GetParam(), true, false, true);
}
TEST_P(UdpIpv6MissingSource, RemoteDestinationWithSourceWithoutRouteFailsRouting) {
    ExerciseMissingIpv6Source(GetParam(), false, false, true);
}
INSTANTIATE_TEST_SUITE_P(ConnectAndSendto, UdpIpv6MissingSource, testing::Bool());
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
