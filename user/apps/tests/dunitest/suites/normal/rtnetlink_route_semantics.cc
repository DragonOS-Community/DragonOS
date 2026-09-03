#include "rtnetlink_route_test_support.h"

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
    RouteSpec main = MakeIpv4Route("203.0.0.0", 16, egress);
    RouteSpec fallback = MakeIpv4Route("203.0.113.0", 24, egress);
    fallback.gateway = Ipv4("111.111.11.2");
    fallback.table = RT_TABLE_DEFAULT;
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
    ASSERT_EQ(restore_error, 0);
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
                                   kDestination, 64, veth, RT_SCOPE_LINK, ++seq),
              0);
    ASSERT_TRUE(FindIpv6Route(sender.Get(), kDestination, 64, veth, ++seq).has_value());

    FdGuard listener(OpenRouteListener(RTMGRP_IPV6_ROUTE));
    ASSERT_GE(listener.Get(), 0) << ErrnoString(errno);
    ASSERT_EQ(SetLinkUp(sender.Get(), veth, false, ++seq), 0);

    EXPECT_FALSE(FindIpv6Route(sender.Get(), kDestination, 64, veth, ++seq).has_value());
    EXPECT_FALSE(FindIpv6Route(sender.Get(), kConnected, 64, veth, ++seq).has_value());
    EXPECT_TRUE(SawIpv6RouteNotification(listener.Get(), kDestination, 64, RTM_DELROUTE));

    // This request carries IFA_F_NODAD. Linux installs its host-local route
    // immediately while the veth is down, but defers the connected prefix
    // until NETDEV_UP.
    (void)SendIpv6AddrRequest(sender.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, veth,
                              kDownAddress, 64, ++seq);
    ASSERT_EQ(SendIpv6AddrRequest(sender.Get(), RTM_NEWADDR,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, veth,
                                  kDownAddress, 64, ++seq),
              0);
    EXPECT_FALSE(FindIpv6Route(sender.Get(), kDownConnected, 64, veth, ++seq).has_value());
    EXPECT_TRUE(FindIpv6Route(sender.Get(), kDownAddress, 128, veth, ++seq).has_value());

    ASSERT_EQ(SetLinkUp(sender.Get(), veth, true, ++seq), 0);
    EXPECT_TRUE(FindIpv6Route(sender.Get(), kConnected, 64, veth, ++seq).has_value());
    EXPECT_TRUE(FindIpv6Route(sender.Get(), kDownConnected, 64, veth, ++seq).has_value());
    EXPECT_TRUE(FindIpv6Route(sender.Get(), kDownAddress, 128, veth, ++seq).has_value());
    EXPECT_TRUE(SawIpv6RouteNotification(listener.Get(), kConnected, 64, RTM_NEWROUTE));
    EXPECT_EQ(SendIpv6AddrRequest(sender.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, veth,
                                  kAddress, 64, ++seq),
              0);
    EXPECT_EQ(SendIpv6AddrRequest(sender.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, veth,
                                  kDownAddress, 64, ++seq),
              0);
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
