#include "rtnetlink_route_test_support.h"

#include <linux/errqueue.h>
#include <poll.h>

TEST(RtnetlinkRouteSemantics, Ipv4BuiltinRulesFallBackFromMainToDefaultTable) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << ErrnoString(errno);
    if (child == 0) {
        const int result = RunIpv4BuiltinRuleChain();
        if (result != 0) dprintf(STDERR_FILENO, "builtin rule child failed: %d\n", result);
        _exit(result == 0 ? 0 : 1);
    }

    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child) << ErrnoString(errno);
    FdGuard cleanup(OpenRouteSocket());
    ASSERT_GE(cleanup.Get(), 0);
    uint32_t cleanup_seq = 100;
    const uint32_t egress = if_nametoindex("veth1");
    RouteSpec through_default = MakeIpv4Route("198.19.0.0", 24, egress);
    through_default.gateway = Ipv4("198.18.240.1");
    RouteSpec gateway_link = MakeIpv4Route("198.18.240.0", 24, egress);
    gateway_link.table = RT_TABLE_DEFAULT;
    RouteSpec main = MakeIpv4Route("203.0.0.0", 16, egress);
    RouteSpec fallback = MakeIpv4Route("203.0.113.0", 24, egress);
    fallback.gateway = Ipv4("111.111.11.2");
    fallback.table = RT_TABLE_DEFAULT;
    DeleteRouteIfPresent(cleanup.Get(), through_default, &cleanup_seq);
    DeleteRouteIfPresent(cleanup.Get(), gateway_link, &cleanup_seq);
    DeleteRouteIfPresent(cleanup.Get(), main, &cleanup_seq);
    DeleteRouteIfPresent(cleanup.Get(), fallback, &cleanup_seq);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(RtnetlinkRouteSemantics, OnLinkRouteWithUnspecTableDumpsAsMainWithoutGateway) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << "socket(AF_NETLINK, NETLINK_ROUTE) failed: " << ErrnoString(errno);
    uint32_t seq = 1000;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec route = MakeIpv4Route("198.18.77.0", 24, lo);
    DeleteRouteIfPresent(fd.Get(), route, &seq);

    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    auto dumped = FindRoute(fd.Get(), route, ++seq);
    ASSERT_TRUE(dumped.has_value());
    EXPECT_EQ(dumped->table, RT_TABLE_MAIN);
    EXPECT_FALSE(dumped->gateway.has_value());

    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, DefaultDevRouteWithoutGatewaySucceeds) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << "socket(AF_NETLINK, NETLINK_ROUTE) failed: " << ErrnoString(errno);
    uint32_t seq = 2000;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec route = {};
    route.oif = lo;
    // Keep this test independent of a default route installed during boot.
    route.priority = 4242;
    DeleteRouteIfPresent(fd.Get(), route, &seq);

    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, NonStrictRequestsIgnoreUnknownAttributes) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2250;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec route = MakeIpv4Route("198.18.87.0", 24, lo);
    DeleteRouteIfPresent(fd.Get(), route, &seq);
    constexpr uint16_t kUnknownRouteAttribute = RTA_MAX + 1;

    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq, kUnknownRouteAttribute),
              0);
    EXPECT_TRUE(FindRoute(fd.Get(), route, ++seq).has_value());
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++seq,
                               kUnknownRouteAttribute),
              0);
}

TEST(RtnetlinkRouteSemantics, MetricAliasesUseLowestPriorityAndWildcardDeleteFirstMatch) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2500;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec metric_200 = MakeIpv4Route("198.18.88.0", 24, lo);
    metric_200.priority = 200;
    RouteSpec metric_100 = metric_200;
    metric_100.priority = 100;
    DeleteRouteIfPresent(fd.Get(), metric_100, &seq);
    DeleteRouteIfPresent(fd.Get(), metric_200, &seq);

    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                               metric_200, ++seq),
              0);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                               metric_100, ++seq),
              0);
    EXPECT_TRUE(FindRoute(fd.Get(), metric_100, ++seq).has_value());
    EXPECT_TRUE(FindRoute(fd.Get(), metric_200, ++seq).has_value());

    RouteSpec wrong_metric = metric_100;
    wrong_metric.priority = 300;
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, wrong_metric,
                               ++seq),
              ESRCH);

    RouteSpec wildcard = metric_100;
    wildcard.priority = 0;
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, wildcard,
                               ++seq),
              0);
    EXPECT_FALSE(FindRoute(fd.Get(), metric_100, ++seq).has_value());
    EXPECT_TRUE(FindRoute(fd.Get(), metric_200, ++seq).has_value());
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, metric_200,
                               ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, ReplaceAndDeleteMatchGatewayPrecisely) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2700;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec route = MakeIpv4Route("198.18.89.0", 24, lo);
    route.priority = 100;
    route.gateway = Ipv4("127.0.0.2");
    DeleteRouteIfPresent(fd.Get(), route, &seq);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              EEXIST);

    RouteSpec replacement = route;
    replacement.gateway = Ipv4("127.0.0.3");
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE, replacement, ++seq),
              0);
    auto dumped = FindRoute(fd.Get(), replacement, ++seq);
    ASSERT_TRUE(dumped.has_value());
    EXPECT_EQ(dumped->gateway, replacement.gateway);
    auto lookup = LookupIpv4Route(fd.Get(), "198.18.89.42", ++seq);
    ASSERT_TRUE(lookup.has_value());
    EXPECT_EQ(lookup->prefix_len, 32);
    EXPECT_EQ(lookup->oif, lo);
    EXPECT_EQ(lookup->gateway, replacement.gateway);

    RouteSpec wrong_gateway = replacement;
    wrong_gateway.gateway = Ipv4("127.0.0.4");
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, wrong_gateway,
                               ++seq),
              ESRCH);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, replacement,
                               ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, CreateFlagIsRequiredForNewIpv4Alias) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2850;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec first = MakeIpv4Route("198.18.219.0", 24, lo);
    first.priority = 2850;
    first.gateway = Ipv4("127.0.0.2");
    RouteSpec alias = first;
    alias.gateway = Ipv4("127.0.0.3");
    DeleteRouteIfPresent(fd.Get(), first, &seq);
    DeleteRouteIfPresent(fd.Get(), alias, &seq);

    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, first,
                               ++seq),
              0);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE, NLM_F_REQUEST | NLM_F_ACK, alias, ++seq),
              ENOENT);
    EXPECT_TRUE(FindRoute(fd.Get(), first, ++seq).has_value());
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, first, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, ZeroOifDoesNotConstrainGatewayResolution) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2900;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec route = MakeIpv4Route("198.18.220.0", 24, 0);
    route.priority = 2900;
    route.gateway = Ipv4("127.0.0.2");
    RouteSpec committed = route;
    committed.oif = lo;
    DeleteRouteIfPresent(fd.Get(), committed, &seq);

    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    auto dumped = FindRoute(fd.Get(), committed, ++seq);
    ASSERT_TRUE(dumped.has_value());
    EXPECT_EQ(dumped->oif, lo);
    EXPECT_EQ(dumped->gateway, route.gateway);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, committed,
                               ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, MissingAndCompactDestinationPayloadsKeepPrefixIdentity) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2950;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    constexpr uint32_t kPriority = 2950;
    (void)SendIpv4ZeroPrefixWithoutDst(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, 24, lo,
                                       kPriority, ++seq);
    ASSERT_EQ(SendIpv4ZeroPrefixWithoutDst(
                      fd.Get(), RTM_NEWROUTE,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, 24, lo, kPriority,
                      ++seq),
              0);
    RouteSpec zero_prefix = MakeIpv4Route("0.0.0.0", 24, lo);
    zero_prefix.priority = kPriority;
    EXPECT_TRUE(FindRoute(fd.Get(), zero_prefix, ++seq).has_value());
    EXPECT_EQ(SendIpv4ZeroPrefixWithoutDst(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, 24,
                                           lo, kPriority, ++seq),
              0);

    constexpr const char* kIpv6Prefix = "2001:db8:abcd:1234::";
    (void)SendCompactIpv6PrefixRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                       kIpv6Prefix, 64, lo, ++seq);
    ASSERT_EQ(SendCompactIpv6PrefixRequest(
                      fd.Get(), RTM_NEWROUTE,
                      NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, kIpv6Prefix, 64, lo,
                      ++seq),
              0);
    EXPECT_TRUE(FindIpv6Route(fd.Get(), kIpv6Prefix, 64, lo, ++seq).has_value());
    EXPECT_EQ(SendCompactIpv6PrefixRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                           kIpv6Prefix, 64, lo, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, Ipv4GatewayHonorsPrefixEndpointsAndRouteScope) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2975;
    uint32_t veth = if_nametoindex("veth-host1");
    ASSERT_NE(veth, 0u);

    RouteSpec route = MakeIpv4Route("198.18.221.0", 24, veth);
    route.priority = 2975;
    route.gateway = Ipv4("192.168.1.0");
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++seq),
              0);
    route.route_flags = RTNH_F_ONLINK;
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    route.route_flags = 0;
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++seq),
              0);

    route.gateway = Ipv4("192.168.1.255");
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              EINVAL);

    route.gateway = Ipv4("192.168.1.254");
    route.route_flags = RTNH_F_ONLINK;
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              EINVAL);
    route.route_flags = 0;

    route.gateway = Ipv4("192.168.1.1");
    route.scope = RT_SCOPE_LINK;
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              ENETUNREACH);
    route.scope = RT_SCOPE_UNIVERSE;
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, LinkDownPurgesRoutesAndRejectsNewNexthops) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2985;
    uint32_t veth = if_nametoindex("veth-host1");
    ASSERT_NE(veth, 0u);

    RouteSpec route = MakeIpv4Route("198.18.222.0", 24, veth);
    route.priority = 2985;
    DeleteRouteIfPresent(fd.Get(), route, &seq);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);

    FdGuard receiver(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(receiver.Get(), 0) << ErrnoString(errno);
    timeval timeout = {};
    timeout.tv_sec = 2;
    ASSERT_EQ(setsockopt(receiver.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
    sockaddr_in local_endpoint = {};
    local_endpoint.sin_family = AF_INET;
    local_endpoint.sin_addr.s_addr = Ipv4("192.168.1.254");
    ASSERT_EQ(bind(receiver.Get(), reinterpret_cast<sockaddr*>(&local_endpoint),
                   sizeof(local_endpoint)),
              0)
        << ErrnoString(errno);
    socklen_t endpoint_len = sizeof(local_endpoint);
    ASSERT_EQ(getsockname(receiver.Get(), reinterpret_cast<sockaddr*>(&local_endpoint),
                          &endpoint_len),
              0);

    ASSERT_EQ(SetLinkUp(fd.Get(), veth, false, ++seq), 0);

    int add_error = SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                                     NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                     route, ++seq);
    bool dumped_while_down = FindRoute(fd.Get(), route, ++seq).has_value();
    auto lookup_while_down = LookupIpv4Route(fd.Get(), "198.18.222.1", ++seq);
    auto local_while_down = LookupIpv4Route(fd.Get(), "192.168.1.254", ++seq);
    FdGuard sender(socket(AF_INET, SOCK_DGRAM, 0));
    ssize_t sent = sender.Get() < 0
                       ? -1
                       : sendto(sender.Get(), "x", 1, 0,
                                reinterpret_cast<sockaddr*>(&local_endpoint),
                                sizeof(local_endpoint));
    int send_error = sent < 0 ? errno : 0;
    char payload = 0;
    ssize_t received = sent < 0 ? -1 : recv(receiver.Get(), &payload, sizeof(payload), 0);
    int receive_error = received < 0 ? errno : 0;
    // Raw IPv4 output does not actively poll the local target. This proves
    // that a DOWN interface still runs its LocalOnly NAPI work independently.
    FdGuard raw_sender(socket(AF_INET, SOCK_RAW, IPPROTO_UDP));
    const uint8_t raw_datagram[9] = {
        0x9c, 0x40, static_cast<uint8_t>(ntohs(local_endpoint.sin_port) >> 8),
        static_cast<uint8_t>(ntohs(local_endpoint.sin_port)), 0, 9, 0, 0, 'r'};
    ssize_t raw_sent = raw_sender.Get() < 0
                           ? -1
                           : sendto(raw_sender.Get(), raw_datagram, sizeof(raw_datagram), 0,
                                    reinterpret_cast<sockaddr*>(&local_endpoint),
                                    sizeof(local_endpoint));
    int raw_send_error = raw_sent < 0 ? errno : 0;
    char raw_payload = 0;
    ssize_t raw_received = raw_sent < 0 ? -1 : recv(receiver.Get(), &raw_payload, 1, 0);
    int raw_receive_error = raw_received < 0 ? errno : 0;
    int restore_error = SetLinkUp(fd.Get(), veth, true, ++seq);

    EXPECT_EQ(add_error, ENETDOWN);
    EXPECT_FALSE(dumped_while_down);
    EXPECT_TRUE(!lookup_while_down.has_value() || lookup_while_down->oif != veth ||
                lookup_while_down->prefix_len != route.prefix_len);
    ASSERT_TRUE(local_while_down.has_value());
    EXPECT_EQ(local_while_down->table, RT_TABLE_LOCAL);
    EXPECT_EQ(local_while_down->kind, RTN_LOCAL);
    EXPECT_EQ(local_while_down->oif, veth);
    EXPECT_EQ(sent, 1) << ErrnoString(send_error);
    EXPECT_EQ(received, 1) << ErrnoString(receive_error);
    EXPECT_EQ(payload, 'x');
    EXPECT_EQ(raw_sent, static_cast<ssize_t>(sizeof(raw_datagram)))
        << ErrnoString(raw_send_error);
    EXPECT_EQ(raw_received, 1) << ErrnoString(raw_receive_error);
    EXPECT_EQ(raw_payload, 'r');
    ASSERT_EQ(restore_error, 0);
}

TEST(RtnetlinkRouteSemantics, Ipv6RouteMutationsSelectTheExactSourcePrefix) {
    const pid_t child = fork();
    ASSERT_GE(child, 0) << ErrnoString(errno);
    if (child == 0) {
        [] {
            ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
            FdGuard fd(OpenRouteSocket());
            ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
            const uint32_t lo = if_nametoindex("lo");
            ASSERT_NE(lo, 0u);
            uint32_t seq = 2900;
            ASSERT_EQ(SetLinkUp(fd.Get(), lo, true, ++seq), 0);
            constexpr const char* destination = "2001:db8:ffff:229::";
            constexpr Ipv6SourcePrefix ordinary{"::", 0};
            constexpr Ipv6SourcePrefix first{"fd00:29:1::", 64};
            constexpr Ipv6SourcePrefix second{"fd00:29:2::", 64};
            auto mutate = [&](uint16_t type, uint16_t flags,
                              std::optional<Ipv6SourcePrefix> source, const char* preferred = nullptr) {
                return SendIpv6RouteRequest(fd.Get(), type, NLM_F_REQUEST | NLM_F_ACK | flags,
                                            destination, 64, lo, RT_SCOPE_UNIVERSE, ++seq,
                                            nullptr, 0, preferred, source);
            };
            auto find = [&](Ipv6SourcePrefix source) {
                return FindIpv6Route(fd.Get(), destination, 64, lo, ++seq, RTN_UNICAST, source);
            };
            constexpr uint16_t add = NLM_F_CREATE | NLM_F_EXCL;
            ASSERT_EQ(mutate(RTM_NEWROUTE, add, std::nullopt), 0);
            EXPECT_EQ(mutate(RTM_NEWROUTE, add, Ipv6SourcePrefix{"fd00:29::1", 0}), EEXIST);
            ASSERT_EQ(mutate(RTM_NEWROUTE, add, Ipv6SourcePrefix{"fd00:29:1::1", 64}), 0);
            ASSERT_EQ(mutate(RTM_NEWROUTE, add, second), 0);
            ASSERT_EQ(mutate(RTM_NEWROUTE, NLM_F_REPLACE, first, "::1"), 0);
            const auto default_route = find(ordinary);
            const auto replaced = find(first);
            const auto untouched = find(second);
            ASSERT_TRUE(default_route.has_value());
            ASSERT_TRUE(replaced.has_value());
            ASSERT_TRUE(untouched.has_value());
            EXPECT_FALSE(default_route->has_preferred_source);
            EXPECT_TRUE(replaced->has_preferred_source);
            EXPECT_FALSE(untouched->has_preferred_source);

            ASSERT_EQ(mutate(RTM_DELROUTE, 0, std::nullopt), 0);
            EXPECT_FALSE(find(ordinary).has_value());
            EXPECT_TRUE(find(first).has_value());
            EXPECT_TRUE(find(second).has_value());
            ASSERT_EQ(mutate(RTM_DELROUTE, 0, Ipv6SourcePrefix{"fd00:29:1::2", 64}), 0);
            EXPECT_FALSE(find(first).has_value());
            EXPECT_TRUE(find(second).has_value());
        }();
        _exit(HasFailure() ? 1 : 0);
    }

    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child) << ErrnoString(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(RtnetlinkRouteSemantics, LinkDownWithdrawsIpv6RoutesAndNotifiesListeners) {
    FdGuard sender(OpenRouteSocket());
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 2990;
    uint32_t veth = if_nametoindex("veth-host1");
    ASSERT_NE(veth, 0u);
    constexpr const char* kDestination = "2001:db8:ffff:222::";
    constexpr const char* kAddress = "2001:db8:ffff:223::1";
    constexpr const char* kConnected = "2001:db8:ffff:223::";
    constexpr const char* kDownAddress = "2001:db8:ffff:224::1";
    constexpr const char* kDownConnected = "2001:db8:ffff:224::";

    ASSERT_EQ(SetLinkUp(sender.Get(), veth, true, ++seq), 0);
    (void)SendIpv6AddrRequest(sender.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, veth,
                              kAddress, 64, ++seq);
    ASSERT_EQ(SendIpv6AddrRequest(sender.Get(), RTM_NEWADDR,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, veth,
                                  kAddress, 64, ++seq),
              0);
    ASSERT_TRUE(FindIpv6Route(sender.Get(), kConnected, 64, veth, ++seq).has_value());
    (void)SendIpv6RouteRequest(sender.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                               kDestination, 64, veth, RT_SCOPE_NOWHERE, ++seq);
    ASSERT_EQ(SendIpv6RouteRequest(sender.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kDestination, 64, veth, RT_SCOPE_LINK, ++seq,
                                   nullptr, 0, kAddress),
              0);
    const auto configured = FindIpv6Route(sender.Get(), kDestination, 64, veth, ++seq);
    ASSERT_TRUE(configured.has_value());
    EXPECT_TRUE(configured->has_preferred_source);

    FdGuard listener(OpenRouteListener(RTMGRP_IPV6_ROUTE));
    ASSERT_GE(listener.Get(), 0) << ErrnoString(errno);
    ASSERT_EQ(SetLinkUp(sender.Get(), veth, false, ++seq), 0);

    EXPECT_FALSE(FindIpv6Route(sender.Get(), kDestination, 64, veth, ++seq).has_value());
    EXPECT_FALSE(FindIpv6Route(sender.Get(), kConnected, 64, veth, ++seq).has_value());
    EXPECT_FALSE(FindIpv6Route(sender.Get(), kAddress, 128, veth, ++seq).has_value());
    EXPECT_TRUE(SawIpv6RouteNotification(listener.Get(), kDestination, 64, RTM_DELROUTE));

    // IFA_F_NODAD allows the host-local route to appear while the device
    // is down. The connected prefix still waits for NETDEV_UP.
    (void)SendIpv6AddrRequest(sender.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, veth,
                              kDownAddress, 64, ++seq);
    ASSERT_EQ(SendIpv6AddrRequest(sender.Get(), RTM_NEWADDR,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, veth,
                                  kDownAddress, 64, ++seq),
              0);
    EXPECT_FALSE(FindIpv6Route(sender.Get(), kDownConnected, 64, veth, ++seq).has_value());
    auto host = FindIpv6Route(sender.Get(), kDownAddress, 128, veth, ++seq);
    for (int attempt = 0; !host && attempt < 50; ++attempt) {
        usleep(10 * 1000);
        host = FindIpv6Route(sender.Get(), kDownAddress, 128, veth, ++seq);
    }
    EXPECT_TRUE(host.has_value());

    ASSERT_EQ(SetLinkUp(sender.Get(), veth, true, ++seq), 0);
    EXPECT_FALSE(FindIpv6Route(sender.Get(), kConnected, 64, veth, ++seq).has_value());
    EXPECT_FALSE(FindIpv6Route(sender.Get(), kAddress, 128, veth, ++seq).has_value());
    EXPECT_TRUE(FindIpv6Route(sender.Get(), kDownConnected, 64, veth, ++seq).has_value());
    EXPECT_TRUE(FindIpv6Route(sender.Get(), kDownAddress, 128, veth, ++seq).has_value());
    EXPECT_TRUE(SawIpv6RouteNotification(listener.Get(), kDownConnected, 64, RTM_NEWROUTE));
    EXPECT_EQ(SendIpv6AddrRequest(sender.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, veth,
                                  kDownAddress, 64, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, Ipv6FibMatchReturnsTheConfiguredRoute) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        [] {
            ASSERT_EQ(unshare(CLONE_NEWNET), 0);
            FdGuard fd(OpenRouteSocket());
            ASSERT_GE(fd.Get(), 0);
            timeval timeout{2, 0};
            ASSERT_EQ(setsockopt(fd.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0);
            const uint32_t lo = if_nametoindex("lo");
            ASSERT_NE(lo, 0u);
            uint32_t seq = 1;
            ASSERT_EQ(SetLinkUp(fd.Get(), lo, true, ++seq), 0);
            constexpr auto add = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
            ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_NEWADDR, add, lo, "fd74:29::1", 64, ++seq), 0);
            bool published = false;
            for (int attempt = 0; attempt < 100; ++attempt) {
                published = FindIpv6Route(fd.Get(), "fd74:29::", 64, lo, ++seq).has_value();
                if (published) break;
                usleep(10000);
            }
            ASSERT_TRUE(published);
            ASSERT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE, add, "fd74:30::", 64, 0,
                                          RT_SCOPE_UNIVERSE, ++seq, nullptr, 0, nullptr,
                                          std::nullopt, RTN_PROHIBIT),
                      0);

            alignas(nlmsghdr) char response[1024] = {};
            auto* reply = reinterpret_cast<nlmsghdr*>(response);
            auto query = [&](const char* address, uint32_t flags, uint32_t input = 0,
                             const char* source = nullptr, uint32_t output = 0) {
                alignas(nlmsghdr) char request[128] = {};
                auto* nlh = reinterpret_cast<nlmsghdr*>(request);
                nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
                nlh->nlmsg_type = RTM_GETROUTE;
                nlh->nlmsg_flags = NLM_F_REQUEST;
                nlh->nlmsg_seq = ++seq;
                auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));
                rtm->rtm_family = AF_INET6;
                rtm->rtm_dst_len = 128;
                rtm->rtm_flags = flags;
                in6_addr destination{};
                ASSERT_EQ(inet_pton(AF_INET6, address, &destination), 1);
                AddAttr(nlh, sizeof(request), RTA_DST, &destination, sizeof(destination));
                if (input != 0) AddAttr(nlh, sizeof(request), RTA_IIF, &input, sizeof(input));
                if (output != 0) AddAttr(nlh, sizeof(request), RTA_OIF, &output, sizeof(output));
                if (source != nullptr) {
                    in6_addr address{};
                    ASSERT_EQ(inet_pton(AF_INET6, source, &address), 1);
                    rtm->rtm_src_len = 128;
                    AddAttr(nlh, sizeof(request), RTA_SRC, &address, sizeof(address));
                }
                ASSERT_EQ(send(fd.Get(), nlh, nlh->nlmsg_len, 0),
                          static_cast<ssize_t>(nlh->nlmsg_len));
                const ssize_t len = recv(fd.Get(), response, sizeof(response), 0);
                ASSERT_TRUE(NLMSG_OK(reply, len)) << ErrnoString(errno);
                ASSERT_EQ(reply->nlmsg_seq, seq);
            };
            auto* route = reinterpret_cast<rtmsg*>(NLMSG_DATA(reply));
            auto attribute = [&](uint16_t type) -> const rtattr* {
                int len = RTM_PAYLOAD(reply);
                for (auto* attr = RTM_RTA(route); RTA_OK(attr, len); attr = RTA_NEXT(attr, len)) {
                    if (attr->rta_type == type) return attr;
                }
                return nullptr;
            };
            auto expect_attr = [&](uint16_t type, const auto& value) {
                const auto* attr = attribute(type);
                ASSERT_NE(attr, nullptr) << type;
                ASSERT_EQ(RTA_PAYLOAD(attr), sizeof(value)) << type;
                EXPECT_EQ(std::memcmp(RTA_DATA(attr), &value, sizeof(value)), 0) << type;
            };

            ASSERT_NO_FATAL_FAILURE(query("fd74:29::9", RTM_F_FIB_MATCH | RTM_F_LOOKUP_TABLE));
            ASSERT_EQ(reply->nlmsg_type, RTM_NEWROUTE);
            EXPECT_EQ(route->rtm_dst_len, 64);
            EXPECT_EQ(route->rtm_type, RTN_UNICAST);
            EXPECT_EQ(route->rtm_protocol, RTPROT_KERNEL);
            EXPECT_EQ(route->rtm_flags & RTM_F_CLONED, 0u);
            in6_addr prefix{};
            ASSERT_EQ(inet_pton(AF_INET6, "fd74:29::", &prefix), 1);
            expect_attr(RTA_DST, prefix);
            expect_attr(RTA_OIF, lo);
            expect_attr(RTA_TABLE, uint32_t{RT_TABLE_MAIN});
            EXPECT_EQ(attribute(RTA_PREFSRC), nullptr);

            ASSERT_NO_FATAL_FAILURE(query("::1", RTM_F_FIB_MATCH));
            ASSERT_EQ(reply->nlmsg_type, RTM_NEWROUTE);
            EXPECT_EQ(route->rtm_dst_len, 128);
            expect_attr(RTA_PRIORITY, uint32_t{0});
            expect_attr(RTA_PREF, uint8_t{0});
            expect_attr(RTA_CACHEINFO, rta_cacheinfo{});

            ASSERT_NO_FATAL_FAILURE(query("fd74:29::9", 0));
            ASSERT_EQ(reply->nlmsg_type, RTM_NEWROUTE);
            EXPECT_EQ(route->rtm_dst_len, 128);
            EXPECT_EQ(attribute(RTA_IIF), nullptr);

            ASSERT_NO_FATAL_FAILURE(query("fd74:29::9", 0, lo));
            ASSERT_EQ(reply->nlmsg_type, RTM_NEWROUTE);
            EXPECT_EQ(route->rtm_dst_len, 128);
            EXPECT_EQ(route->rtm_src_len, 128);
            expect_attr(RTA_SRC, in6addr_any);
            expect_attr(RTA_IIF, lo);
            expect_attr(RTA_OIF, lo);
            EXPECT_EQ(attribute(RTA_PREFSRC), nullptr);

            in6_addr multicast{};
            ASSERT_EQ(inet_pton(AF_INET6, "ff02::1", &multicast), 1);
            ASSERT_NO_FATAL_FAILURE(query("fd74:29::9", 0, lo, "ff02::1"));
            ASSERT_EQ(reply->nlmsg_type, RTM_NEWROUTE);
            expect_attr(RTA_SRC, multicast);
            expect_attr(RTA_IIF, lo);
            EXPECT_NE(attribute(RTA_CACHEINFO), nullptr);

            constexpr char link_name[] = "iif80";
            const struct {
                rtattr header;
                char kind[RTA_ALIGN(sizeof("veth"))];
            } link_info{{RTA_LENGTH(sizeof("veth")), IFLA_INFO_KIND}, "veth"};
            alignas(nlmsghdr) char link_request[128] = {};
            auto* link = reinterpret_cast<nlmsghdr*>(link_request);
            link->nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
            link->nlmsg_type = RTM_NEWLINK;
            link->nlmsg_flags = add;
            link->nlmsg_seq = ++seq;
            AddAttr(link, sizeof(link_request), IFLA_IFNAME, link_name, sizeof(link_name));
            AddAttr(link, sizeof(link_request), IFLA_LINKINFO, &link_info, sizeof(link_info));
            ASSERT_EQ(send(fd.Get(), link, link->nlmsg_len, 0),
                      static_cast<ssize_t>(link->nlmsg_len));
            ASSERT_EQ(RecvAck(fd.Get(), seq), 0);
            const uint32_t veth = if_nametoindex(link_name);
            ASSERT_NE(veth, 0u);
            ASSERT_EQ(SetLinkUp(fd.Get(), veth, true, ++seq), 0);

            const uint32_t peer = if_nametoindex("veth0");
            ASSERT_NE(peer, 0u);
            ASSERT_NE(peer, veth);
            ASSERT_EQ(SetLinkUp(fd.Get(), peer, true, ++seq), 0);
            for (const uint32_t input : {veth, peer}) {
                bool multicast_ready = false;
                for (int attempt = 0; attempt < 100 && !multicast_ready; ++attempt) {
                    multicast_ready = FindIpv6Route(fd.Get(), "ff00::", 8, input, ++seq,
                                                    RTN_MULTICAST).has_value();
                    if (!multicast_ready) usleep(10000);
                }
                ASSERT_TRUE(multicast_ready) << input;
            }
            for (const uint32_t flags : {0u, uint32_t{RTM_F_FIB_MATCH}}) {
                for (const uint32_t input : {veth, peer}) {
                    ASSERT_NO_FATAL_FAILURE(query("ff02::1", flags, input, "fe80::2"));
                    if (reply->nlmsg_type == NLMSG_ERROR) {
                        // Linux with IPv6 multicast routing enabled can resolve the
                        // interface but cannot serialize a reply without an MFC entry.
                        ASSERT_GE(reply->nlmsg_len, NLMSG_LENGTH(sizeof(nlmsgerr)));
                        EXPECT_EQ(reinterpret_cast<nlmsgerr*>(NLMSG_DATA(reply))->error, -EMSGSIZE)
                            << "input=" << input << " flags=" << flags;
                    } else {
                        ASSERT_EQ(reply->nlmsg_type, RTM_NEWROUTE);
                        expect_attr(RTA_IIF, input);
                        expect_attr(RTA_OIF, input);
                    }
                }
                ASSERT_NO_FATAL_FAILURE(query("ff02::1", flags, lo, "fe80::2"));
                ASSERT_EQ(reply->nlmsg_type, NLMSG_ERROR);
                ASSERT_GE(reply->nlmsg_len, NLMSG_LENGTH(sizeof(nlmsgerr)));
                EXPECT_EQ(reinterpret_cast<nlmsgerr*>(NLMSG_DATA(reply))->error, -ENETUNREACH);
            }

            ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_NEWADDR, add, veth, "fd85:1::1", 64,
                                          ++seq), 0);
            ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_NEWADDR, add, peer, "fd85:1::2", 64,
                                          ++seq), 0);
            for (const uint32_t flags : {0u, uint32_t{RTM_F_FIB_MATCH}}) {
                for (const uint32_t input : {veth, peer}) {
                    ASSERT_NO_FATAL_FAILURE(query("fd85:1::9", flags, input, "fd85:1::3"));
                    ASSERT_EQ(reply->nlmsg_type, RTM_NEWROUTE);
                    expect_attr(RTA_IIF, input);
                    expect_attr(RTA_OIF, input);
                }
            }

            const uint32_t unknown = UINT32_MAX;
            ASSERT_NO_FATAL_FAILURE(
                query("fd74:29::9", RTM_F_FIB_MATCH, lo, "fd74:40::2", unknown));
            ASSERT_EQ(reply->nlmsg_type, RTM_NEWROUTE);
            EXPECT_EQ(route->rtm_dst_len, 64);
            EXPECT_EQ(route->rtm_src_len, 0);
            expect_attr(RTA_DST, prefix);
            expect_attr(RTA_IIF, lo);
            expect_attr(RTA_OIF, lo);
            EXPECT_EQ(attribute(RTA_SRC), nullptr);

            ASSERT_NO_FATAL_FAILURE(query("fd74:29::9", 0, unknown, "fd74:40::2"));
            ASSERT_EQ(reply->nlmsg_type, NLMSG_ERROR);
            ASSERT_GE(reply->nlmsg_len, NLMSG_LENGTH(sizeof(nlmsgerr)));
            EXPECT_EQ(reinterpret_cast<nlmsgerr*>(NLMSG_DATA(reply))->error, -ENODEV);

            ASSERT_NO_FATAL_FAILURE(query("fd74:30::9", RTM_F_FIB_MATCH));
            ASSERT_EQ(reply->nlmsg_type, NLMSG_ERROR);
            ASSERT_GE(reply->nlmsg_len, NLMSG_LENGTH(sizeof(nlmsgerr)));
            EXPECT_EQ(reinterpret_cast<nlmsgerr*>(NLMSG_DATA(reply))->error, -EACCES);

            ASSERT_NO_FATAL_FAILURE(query("fd74:30::9", RTM_F_FIB_MATCH, lo, "fd74:40::2"));
            ASSERT_EQ(reply->nlmsg_type, NLMSG_ERROR);
            ASSERT_GE(reply->nlmsg_len, NLMSG_LENGTH(sizeof(nlmsgerr)));
            EXPECT_EQ(reinterpret_cast<nlmsgerr*>(NLMSG_DATA(reply))->error, -EACCES);

            ASSERT_NO_FATAL_FAILURE(query("fd74:31::9", RTM_F_FIB_MATCH));
            ASSERT_EQ(reply->nlmsg_type, NLMSG_ERROR);
            ASSERT_GE(reply->nlmsg_len, NLMSG_LENGTH(sizeof(nlmsgerr)));
            EXPECT_EQ(reinterpret_cast<nlmsgerr*>(NLMSG_DATA(reply))->error, -ENETUNREACH);
        }();
        _exit(HasFailure() ? 1 : 0);
    }

    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(RtnetlinkRouteSemantics, Ipv6RejectTerminatesLookupAndThrowSkipsTheTable) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        auto run = []() {
            ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
            FdGuard route(OpenRouteSocket());
            ASSERT_GE(route.Get(), 0);
            const unsigned lo = if_nametoindex("lo");
            ASSERT_NE(lo, 0u);
            uint32_t seq = 1;
            constexpr auto request = NLM_F_REQUEST | NLM_F_ACK;
            constexpr auto add = request | NLM_F_CREATE | NLM_F_EXCL;
            constexpr auto prefix = "2001:db8:29:aa::";
            auto mutate = [&](uint16_t operation, uint16_t flags, uint8_t kind,
                              uint8_t table = RT_TABLE_MAIN) {
                return SendIpv6RouteRequest(route.Get(), operation, flags, prefix, 64, 0,
                                            RT_SCOPE_UNIVERSE, ++seq, nullptr, 0, nullptr,
                                            std::nullopt, kind, table);
            };
            auto connect_error = []() {
                FdGuard sender(socket(AF_INET6, SOCK_DGRAM, 0));
                if (sender.Get() < 0) return errno;
                sockaddr_in6 peer{};
                peer.sin6_family = AF_INET6;
                peer.sin6_port = htons(23029);
                if (inet_pton(AF_INET6, "2001:db8:29:aa::9", &peer.sin6_addr) != 1) return EINVAL;
                return connect(sender.Get(), reinterpret_cast<sockaddr*>(&peer), sizeof(peer)) == 0
                    ? 0 : errno;
            };
            ASSERT_EQ(SetLinkUp(route.Get(), lo, false, ++seq), 0);
            ASSERT_EQ(mutate(RTM_NEWROUTE, add, RTN_UNREACHABLE), 0);
            EXPECT_TRUE(FindIpv6Route(route.Get(), prefix, 64, lo, ++seq, RTN_UNREACHABLE));
            ASSERT_EQ(SetLinkUp(route.Get(), lo, true, ++seq), 0);
            ASSERT_EQ(SendIpv6AddrRequest(route.Get(), RTM_NEWADDR, add, lo, "fd29:aa::1", 64, ++seq), 0);
            ASSERT_EQ(SendIpv6RouteRequest(route.Get(), RTM_NEWROUTE, add, "2001:db8:29::", 48,
                                          lo, RT_SCOPE_UNIVERSE, ++seq), 0);
            EXPECT_EQ(connect_error(), EHOSTUNREACH);
            EXPECT_EQ(mutate(RTM_DELROUTE, request, RTN_UNREACHABLE), 0);

            for (const auto [kind, error] :
                 {std::pair{RTN_PROHIBIT, EACCES}, std::pair{RTN_BLACKHOLE, EINVAL}}) {
                ASSERT_EQ(mutate(RTM_NEWROUTE, add, kind), 0);
                EXPECT_TRUE(FindIpv6Route(route.Get(), prefix, 64, lo, ++seq, kind));
                EXPECT_EQ(connect_error(), error);
                EXPECT_EQ(mutate(RTM_DELROUTE, request, kind), 0);
            }
            ASSERT_EQ(mutate(RTM_NEWROUTE, add, RTN_THROW, RT_TABLE_LOCAL), 0);
            EXPECT_TRUE(FindIpv6Route(route.Get(), prefix, 64, lo, ++seq, RTN_THROW));
            EXPECT_EQ(connect_error(), 0);
            ASSERT_EQ(mutate(RTM_NEWROUTE, add, RTN_THROW), 0);
            EXPECT_EQ(connect_error(), ENETUNREACH);
        };
        run();
        _exit(testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(RtnetlinkRouteSemantics, Ipv6ExplicitLocalRouteDeliversWithoutOwningAnAddress) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        [] {
            ASSERT_EQ(unshare(CLONE_NEWNET), 0) << ErrnoString(errno);
            FdGuard route(OpenRouteSocket());
            ASSERT_GE(route.Get(), 0);
            const uint32_t lo = if_nametoindex("lo");
            ASSERT_NE(lo, 0u);
            uint32_t seq = 1;
            ASSERT_EQ(SetLinkUp(route.Get(), lo, true, ++seq), 0);
            constexpr auto request = NLM_F_REQUEST | NLM_F_ACK;
            constexpr auto add = request | NLM_F_CREATE | NLM_F_EXCL;
            constexpr char link_name[] = "local76";
            const struct {
                rtattr header;
                char kind[RTA_ALIGN(sizeof("veth"))];
            } link_info{{RTA_LENGTH(sizeof("veth")), IFLA_INFO_KIND}, "veth"};
            alignas(nlmsghdr) char link_request[128] = {};
            auto* link = reinterpret_cast<nlmsghdr*>(link_request);
            link->nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
            link->nlmsg_type = RTM_NEWLINK;
            link->nlmsg_flags = add;
            link->nlmsg_seq = ++seq;
            AddAttr(link, sizeof(link_request), IFLA_IFNAME, link_name, sizeof(link_name));
            AddAttr(link, sizeof(link_request), IFLA_LINKINFO, &link_info, sizeof(link_info));
            ASSERT_EQ(send(route.Get(), link, link->nlmsg_len, 0),
                      static_cast<ssize_t>(link->nlmsg_len));
            ASSERT_EQ(RecvAck(route.Get(), seq), 0);
            const uint32_t veth = if_nametoindex(link_name);
            ASSERT_NE(veth, 0u);
            ASSERT_EQ(SetLinkUp(route.Get(), veth, true, ++seq), 0);

            auto mutate = [&](uint16_t operation, uint16_t flags) {
                return SendIpv6RouteRequest(route.Get(), operation, flags, "fd76:80::", 80, veth,
                                            RT_SCOPE_UNIVERSE, ++seq, nullptr, 0, nullptr,
                                            std::nullopt, RTN_LOCAL, RT_TABLE_LOCAL);
            };
            ASSERT_EQ(mutate(RTM_NEWROUTE, add), 0);
            ASSERT_TRUE(FindIpv6Route(route.Get(), "fd76:80::", 80, veth, ++seq, RTN_LOCAL));

            sockaddr_in6 destination{};
            destination.sin6_family = AF_INET6;
            destination.sin6_port = htons(23076);
            ASSERT_EQ(inet_pton(AF_INET6, "fd76:80::9", &destination.sin6_addr), 1);
            FdGuard listener(socket(AF_INET6, SOCK_DGRAM, 0));
            ASSERT_GE(listener.Get(), 0);
            ASSERT_EQ(bind(listener.Get(), reinterpret_cast<sockaddr*>(&destination),
                           sizeof(destination)), -1);
            EXPECT_EQ(errno, EADDRNOTAVAIL);
            sockaddr_in6 wildcard = destination;
            wildcard.sin6_addr = in6addr_any;
            ASSERT_EQ(bind(listener.Get(), reinterpret_cast<sockaddr*>(&wildcard), sizeof(wildcard)), 0);

            FdGuard sender(socket(AF_INET6, SOCK_DGRAM, 0));
            ASSERT_GE(sender.Get(), 0);
            sockaddr_in6 source{};
            source.sin6_family = AF_INET6;
            source.sin6_addr = in6addr_loopback;
            ASSERT_EQ(bind(sender.Get(), reinterpret_cast<sockaddr*>(&source), sizeof(source)), 0);
            auto send_byte = [&] {
                return sendto(sender.Get(), "a", 1, 0, reinterpret_cast<sockaddr*>(&destination),
                              sizeof(destination));
            };
            ASSERT_EQ(send_byte(), 1);
            pollfd readable{listener.Get(), POLLIN, 0};
            ASSERT_EQ(poll(&readable, 1, 2000), 1);
            char byte = 0;
            sockaddr_in6 peer{};
            socklen_t peer_len = sizeof(peer);
            ASSERT_EQ(recvfrom(listener.Get(), &byte, 1, MSG_DONTWAIT,
                               reinterpret_cast<sockaddr*>(&peer), &peer_len), 1);
            EXPECT_EQ(byte, 'a');
            EXPECT_TRUE(IN6_IS_ADDR_LOOPBACK(&peer.sin6_addr));

            ASSERT_EQ(mutate(RTM_DELROUTE, request), 0);
            EXPECT_FALSE(FindIpv6Route(route.Get(), "fd76:80::", 80, veth, ++seq, RTN_LOCAL));
            ASSERT_EQ(send_byte(), -1);
            EXPECT_EQ(errno, ENETUNREACH);
        }();
        _exit(HasFailure() ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(RtnetlinkRouteSemantics, Ipv6LoopbackAnycastRejectsDeliveryWithoutOwningAnAddress) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        auto run = []() {
            ASSERT_EQ(unshare(CLONE_NEWNET), 0);
            FdGuard route(OpenRouteSocket());
            ASSERT_GE(route.Get(), 0);
            const unsigned lo = if_nametoindex("lo");
            ASSERT_NE(lo, 0u);
            uint32_t seq = 1;
            constexpr auto request = NLM_F_REQUEST | NLM_F_ACK;
            constexpr auto add = request | NLM_F_CREATE | NLM_F_EXCL;
            constexpr auto destination = "fd29:82::9";
            ASSERT_EQ(SetLinkUp(route.Get(), lo, true, ++seq), 0);
            auto mutate = [&](uint16_t operation, uint16_t flags) {
                return SendIpv6RouteRequest(route.Get(), operation, flags, destination, 128, lo,
                                            RT_SCOPE_UNIVERSE, ++seq, nullptr, 0, nullptr,
                                            std::nullopt, RTN_ANYCAST, RT_TABLE_LOCAL);
            };
            ASSERT_EQ(mutate(RTM_NEWROUTE, add), 0);
            ASSERT_TRUE(FindIpv6Route(route.Get(), destination, 128, lo, ++seq, RTN_ANYCAST));

            sockaddr_in6 peer{};
            peer.sin6_family = AF_INET6;
            peer.sin6_port = htons(23029);
            ASSERT_EQ(inet_pton(AF_INET6, destination, &peer.sin6_addr), 1);
            FdGuard listener(socket(AF_INET6, SOCK_DGRAM, 0));
            ASSERT_GE(listener.Get(), 0);
            ASSERT_EQ(bind(listener.Get(), reinterpret_cast<sockaddr*>(&peer), sizeof(peer)), -1);
            ASSERT_EQ(errno, EADDRNOTAVAIL);
            sockaddr_in6 wildcard = peer;
            wildcard.sin6_addr = in6addr_any;
            ASSERT_EQ(bind(listener.Get(), reinterpret_cast<sockaddr*>(&wildcard), sizeof(wildcard)), 0);

            FdGuard oversized(socket(AF_INET6, SOCK_DGRAM, 0));
            ASSERT_GE(oversized.Get(), 0);
            int mtu = 1280;
            int discover = IPV6_PMTUDISC_DO;
            ASSERT_EQ(setsockopt(oversized.Get(), IPPROTO_IPV6, IPV6_MTU, &mtu, sizeof(mtu)), 0);
            ASSERT_EQ(setsockopt(oversized.Get(), IPPROTO_IPV6, IPV6_MTU_DISCOVER,
                                 &discover, sizeof(discover)), 0);
            char datagram[2000] = {};
            ASSERT_EQ(sendto(oversized.Get(), datagram, sizeof(datagram), 0,
                             reinterpret_cast<sockaddr*>(&peer), sizeof(peer)), -1);
            EXPECT_EQ(errno, EMSGSIZE);

            FdGuard sender(socket(AF_INET6, SOCK_DGRAM, 0));
            ASSERT_GE(sender.Get(), 0);
            int enabled = 1;
            ASSERT_EQ(setsockopt(sender.Get(), IPPROTO_IPV6, IPV6_RECVERR, &enabled, sizeof(enabled)), 0);
            ASSERT_EQ(sendto(sender.Get(), "a", 1, 0, reinterpret_cast<sockaddr*>(&peer), sizeof(peer)), 1);
            pollfd pending{sender.Get(), POLLERR, 0};
            ASSERT_EQ(poll(&pending, 1, 2000), 1);
            ASSERT_TRUE(pending.revents & POLLERR);
            alignas(cmsghdr) char control[128] = {};
            char payload = 0;
            iovec iov{&payload, sizeof(payload)};
            msghdr msg{};
            msg.msg_iov = &iov;
            msg.msg_iovlen = 1;
            msg.msg_control = control;
            msg.msg_controllen = sizeof(control);
            ASSERT_EQ(recvmsg(sender.Get(), &msg, MSG_ERRQUEUE), 1);
            auto* cmsg = CMSG_FIRSTHDR(&msg);
            ASSERT_NE(cmsg, nullptr);
            ASSERT_GE(cmsg->cmsg_len, CMSG_LEN(sizeof(sock_extended_err)));
            EXPECT_EQ(cmsg->cmsg_level, IPPROTO_IPV6);
            EXPECT_EQ(cmsg->cmsg_type, IPV6_RECVERR);
            sock_extended_err error{};
            std::memcpy(&error, CMSG_DATA(cmsg), sizeof(error));
            EXPECT_EQ(error.ee_origin, SO_EE_ORIGIN_ICMP6);
            EXPECT_EQ(error.ee_type, 1);
            EXPECT_EQ(error.ee_code, 0);
            EXPECT_EQ(error.ee_errno, ENETUNREACH);
            EXPECT_EQ(recv(listener.Get(), &payload, 1, MSG_DONTWAIT), -1);
            EXPECT_EQ(errno, EAGAIN);
            ASSERT_EQ(mutate(RTM_DELROUTE, request), 0);
            EXPECT_FALSE(FindIpv6Route(route.Get(), destination, 128, lo, ++seq, RTN_ANYCAST));
        };
        run();
        _exit(testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(RtnetlinkRouteSemantics, RouteProjectionScalesBeyondFormerFixedCapacity) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << "socket(AF_NETLINK, NETLINK_ROUTE) failed: " << ErrnoString(errno);
    uint32_t seq = 3000;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    std::vector<RouteSpec> added;
    for (int i = 0; i < 32; ++i) {
        std::string dst = std::string("198.18.") + std::to_string(100 + i) + ".0";
        RouteSpec route = MakeIpv4Route(dst.c_str(), 24, lo);
        DeleteRouteIfPresent(fd.Get(), route, &seq);
        int err = SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                                   ++seq);
        ASSERT_EQ(err, 0) << "route " << i << " hit an artificial projection limit: "
                          << ErrnoString(err);
        added.push_back(route);
    }

    ASSERT_EQ(added.size(), 32u);
    EXPECT_TRUE(FindRoute(fd.Get(), added.back(), ++seq).has_value());

    for (const auto& route : added) {
        EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route,
                                   ++seq),
                  0);
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
