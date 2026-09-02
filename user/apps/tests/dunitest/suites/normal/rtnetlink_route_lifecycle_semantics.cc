#include "rtnetlink_route_test_support.h"

TEST(RtnetlinkRouteSemantics, OnLinkRouteAllowsUdpSendWithoutNoRoute) {
    FdGuard netlink_fd(OpenRouteSocket());
    ASSERT_GE(netlink_fd.Get(), 0) << "socket(AF_NETLINK, NETLINK_ROUTE) failed: "
                                   << ErrnoString(errno);
    uint32_t seq = 4000;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec route = MakeIpv4Route("198.18.201.0", 24, lo);
    DeleteRouteIfPresent(netlink_fd.Get(), route, &seq);
    ASSERT_EQ(SendRouteRequest(netlink_fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);

    FdGuard udp_fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(udp_fd.Get(), 0) << "socket(AF_INET, SOCK_DGRAM) failed: " << ErrnoString(errno);

    sockaddr_in dst = {};
    dst.sin_family = AF_INET;
    dst.sin_port = htons(9);
    dst.sin_addr.s_addr = Ipv4("198.18.201.42");

    const char payload[] = "x";
    errno = 0;
    ssize_t sent = sendto(udp_fd.Get(), payload, sizeof(payload), 0,
                          reinterpret_cast<sockaddr*>(&dst), sizeof(dst));
    EXPECT_GE(sent, 0) << "sendto failed: " << ErrnoString(errno);
    EXPECT_NE(errno, ENETUNREACH);

    EXPECT_EQ(SendRouteRequest(netlink_fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route,
                               ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, ConnectedRouteIsAnOrdinaryDeletableFibObject) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 4500;
    uint32_t veth = if_nametoindex("veth-host1");
    ASSERT_NE(veth, 0u);

    DeleteAddrIfPresent(fd.Get(), veth, "198.19.0.1", 24, &seq);
    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_NEWADDR,
                              NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, veth,
                              "198.19.0.1", 24, ++seq),
              0);

    RouteSpec connected = MakeIpv4Route("198.19.0.0", 24, veth);
    connected.protocol = RTPROT_KERNEL;
    connected.scope = RT_SCOPE_LINK;
    ASSERT_TRUE(FindRoute(fd.Get(), connected, ++seq).has_value());
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, connected,
                               ++seq),
              0);
    EXPECT_FALSE(FindRoute(fd.Get(), connected, ++seq).has_value());

    EXPECT_EQ(SendAddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, veth,
                              "198.19.0.1", 24, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, LoopbackIpv4SubnetUsesLinuxLocalRouteIdentity) {
    FdGuard netlink_fd(OpenRouteSocket());
    ASSERT_GE(netlink_fd.Get(), 0) << "socket(AF_NETLINK, NETLINK_ROUTE) failed: "
                                   << ErrnoString(errno);
    uint32_t seq = 5000;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    DeleteAddrIfPresent(netlink_fd.Get(), lo, "192.0.2.1", 24, &seq);
    ASSERT_EQ(SendAddrRequest(netlink_fd.Get(), RTM_NEWADDR,
                              NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, lo,
                              "192.0.2.1", 24, ++seq),
              0);

    RouteSpec local_prefix = MakeIpv4Route("192.0.2.0", 24, lo);
    local_prefix.table = RT_TABLE_LOCAL;
    local_prefix.protocol = RTPROT_KERNEL;
    local_prefix.scope = RT_SCOPE_HOST;
    local_prefix.kind = RTN_LOCAL;
    ASSERT_TRUE(FindRoute(netlink_fd.Get(), local_prefix, ++seq).has_value());

    RouteSpec local_host = local_prefix;
    local_host.dst = Ipv4("192.0.2.1");
    local_host.prefix_len = 32;
    local_host.preferred_source = Ipv4("192.0.2.1");
    ASSERT_TRUE(FindRoute(netlink_fd.Get(), local_host, ++seq).has_value());

    RouteSpec broadcast = local_prefix;
    broadcast.dst = Ipv4("192.0.2.255");
    broadcast.prefix_len = 32;
    broadcast.scope = RT_SCOPE_LINK;
    broadcast.kind = RTN_BROADCAST;
    broadcast.preferred_source = Ipv4("192.0.2.1");
    ASSERT_TRUE(FindRoute(netlink_fd.Get(), broadcast, ++seq).has_value());

    // The local table performs one LPM across route kinds: the /32 broadcast
    // must beat the shorter loopback-local /24 prefix.
    auto broadcast_lookup = LookupIpv4Route(netlink_fd.Get(), "192.0.2.255", ++seq);
    ASSERT_TRUE(broadcast_lookup.has_value());
    EXPECT_EQ(broadcast_lookup->kind, RTN_BROADCAST);
    EXPECT_EQ(broadcast_lookup->prefix_len, 32);
    EXPECT_EQ(broadcast_lookup->oif, lo);

    RouteSpec wrong_main = local_prefix;
    wrong_main.table = RT_TABLE_MAIN;
    wrong_main.kind = RTN_UNICAST;
    wrong_main.scope = RT_SCOPE_LINK;
    EXPECT_FALSE(FindRoute(netlink_fd.Get(), wrong_main, ++seq).has_value());

    FdGuard local(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(local.Get(), 0);
    sockaddr_in address = {};
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = Ipv4("192.0.2.1");
    EXPECT_EQ(bind(local.Get(), reinterpret_cast<sockaddr*>(&address), sizeof(address)), 0)
            << ErrnoString(errno);

    EXPECT_EQ(SendAddrRequest(netlink_fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, lo,
                              "192.0.2.1", 24, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, TransientBroadcastAndMulticastLookupsFollowLinuxFibSemantics) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 5250;
    uint32_t veth = if_nametoindex("veth-host1");
    ASSERT_NE(veth, 0u);

    auto limited = LookupIpv4Route(fd.Get(), "255.255.255.255", ++seq, veth);
    ASSERT_TRUE(limited.has_value());
    EXPECT_EQ(limited->table, RT_TABLE_MAIN);
    EXPECT_EQ(limited->kind, RTN_BROADCAST);
    EXPECT_EQ(limited->prefix_len, 32);
    EXPECT_EQ(limited->oif, veth);
    EXPECT_FALSE(limited->gateway.has_value());
    bool dumped_limited_broadcast = false;
    for (const auto& route : DumpRoutes(fd.Get(), ++seq)) {
        if (route.dst == Ipv4("255.255.255.255") && route.prefix_len == 32) {
            dumped_limited_broadcast = true;
            break;
        }
    }
    EXPECT_FALSE(dumped_limited_broadcast)
            << "limited broadcast must remain a transient lookup decision";

    RouteSpec multicast = MakeIpv4Route("239.1.0.0", 16, veth);
    multicast.gateway = Ipv4("192.168.1.1");
    multicast.priority = 5250;
    DeleteRouteIfPresent(fd.Get(), multicast, &seq);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                               multicast, ++seq),
              0);

    for (const auto oif : {std::optional<uint32_t>{}, std::optional<uint32_t>{veth}}) {
        auto lookup = LookupIpv4Route(fd.Get(), "239.1.1.1", ++seq, oif);
        if (!lookup.has_value()) {
            ADD_FAILURE() << "multicast lookup failed";
            continue;
        }
        EXPECT_EQ(lookup->table, RT_TABLE_MAIN);
        EXPECT_EQ(lookup->kind, RTN_MULTICAST);
        EXPECT_EQ(lookup->prefix_len, 32);
        EXPECT_EQ(lookup->oif, veth);
        EXPECT_EQ(lookup->gateway, multicast.gateway);
    }

    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, multicast,
                               ++seq),
              0);

    RouteSpec less_specific = MakeIpv4Route("224.0.0.0", 3, veth);
    less_specific.gateway = Ipv4("192.168.1.1");
    less_specific.priority = 5251;
    DeleteRouteIfPresent(fd.Get(), less_specific, &seq);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                               less_specific, ++seq),
              0);

    for (const auto oif : {std::optional<uint32_t>{}, std::optional<uint32_t>{veth}}) {
        auto lookup = LookupIpv4Route(fd.Get(), "239.1.1.1", ++seq, oif);
        ASSERT_TRUE(lookup.has_value());
        EXPECT_EQ(lookup->kind, RTN_MULTICAST);
        EXPECT_EQ(lookup->prefix_len, 32);
        EXPECT_EQ(lookup->oif, veth);
        EXPECT_FALSE(lookup->gateway.has_value())
                << "a route less specific than 224/4 must not retain its gateway";
    }

    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                               less_specific, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, AddressNotificationPrecedesDerivedRouteNotification) {
    FdGuard fd(OpenRouteSocket());
    FdGuard listener(OpenRouteListener(RTMGRP_IPV4_IFADDR | RTMGRP_IPV4_ROUTE));
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    ASSERT_GE(listener.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 5500;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    DeleteAddrIfPresent(fd.Get(), lo, "192.0.3.1", 24, &seq);
    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_NEWADDR,
                              NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, lo,
                              "192.0.3.1", 24, ++seq),
              0);
    EXPECT_EQ(ReceiveNotificationType(listener.Get()), RTM_NEWADDR);
    EXPECT_EQ(ReceiveNotificationType(listener.Get()), RTM_NEWROUTE);
    EXPECT_EQ(ReceiveNotificationType(listener.Get()), RTM_NEWROUTE);
    EXPECT_EQ(ReceiveNotificationType(listener.Get()), RTM_NEWROUTE);

    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, lo,
                              "192.0.3.1", 24, ++seq),
              0);
    EXPECT_EQ(ReceiveNotificationType(listener.Get()), RTM_DELADDR);
    EXPECT_EQ(ReceiveNotificationType(listener.Get()), RTM_DELROUTE);
    EXPECT_EQ(ReceiveNotificationType(listener.Get()), RTM_DELROUTE);
    EXPECT_EQ(ReceiveNotificationType(listener.Get()), RTM_DELROUTE);
}

TEST(RtnetlinkRouteSemantics, RejectsNonCanonicalPrefixAndUnsupportedTos) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 6000;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec noncanonical = MakeIpv4Route("198.18.210.1", 24, lo);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                               noncanonical, ++seq),
              EINVAL);

    RouteSpec ecn = MakeIpv4Route("198.18.211.0", 24, lo);
    ecn.tos = 1;
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, ecn,
                               ++seq),
              EINVAL);

    RouteSpec dscp = MakeIpv4Route("198.18.212.0", 24, lo);
    dscp.tos = 0x10;
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, dscp,
                               ++seq),
              EOPNOTSUPP);
}

TEST(RtnetlinkRouteSemantics, ZeroGatewayIsNormalizedAndStrictGetRejectsRouteAttributes) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 6500;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec route = MakeIpv4Route("198.18.213.0", 24, lo);
    route.gateway = 0;
    DeleteRouteIfPresent(fd.Get(), route, &seq);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    auto dumped = FindRoute(fd.Get(), route, ++seq);
    ASSERT_TRUE(dumped.has_value());
    EXPECT_FALSE(dumped->gateway.has_value());
    auto lookup = LookupIpv4Route(fd.Get(), "198.18.213.1", ++seq, lo);
    ASSERT_TRUE(lookup.has_value());
    EXPECT_EQ(lookup->oif, lo);
    ASSERT_TRUE(lookup->preferred_source.has_value());
    EXPECT_EQ(*lookup->preferred_source, Ipv4("127.0.0.1"));

    EXPECT_EQ(SendInvalidIpv4GetAttr(fd.Get(), "198.18.213.1", RTA_PRIORITY, 100, ++seq),
              EINVAL);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, CrossInterfacePreferredSourceDrivesOutputAndAddressRemoval) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 6750;
    const uint32_t lo = if_nametoindex("lo");
    const uint32_t egress = if_nametoindex("veth1");
    ASSERT_NE(lo, 0u);
    ASSERT_NE(egress, 0u);
    constexpr const char* kSource = "198.18.215.1";
    constexpr const char* kDestination = "198.18.214.7";

    RouteSpec route = MakeIpv4Route("198.18.214.0", 24, egress);
    route.gateway = Ipv4("111.111.11.2");
    route.priority = 6750;
    route.preferred_source = Ipv4(kSource);
    RouteSpec fallback = route;
    fallback.gateway.reset();
    fallback.priority = 6751;
    fallback.preferred_source.reset();
    DeleteRouteIfPresent(fd.Get(), route, &seq);
    DeleteRouteIfPresent(fd.Get(), fallback, &seq);
    DeleteAddrIfPresent(fd.Get(), lo, kSource, 32, &seq);
    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_NEWADDR,
                              NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, lo, kSource,
                              32, ++seq),
              0);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, fallback,
                               ++seq),
              0);
    auto dumped = FindRoute(fd.Get(), route, ++seq);
    ASSERT_TRUE(dumped.has_value());
    EXPECT_EQ(dumped->preferred_source, route.preferred_source);
    auto lookup = LookupIpv4Route(fd.Get(), kDestination, ++seq, egress);
    ASSERT_TRUE(lookup.has_value());
    EXPECT_EQ(lookup->preferred_source, route.preferred_source);

    FdGuard observer(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    ASSERT_GE(observer.Get(), 0) << ErrnoString(errno);
    sockaddr_ll packet_bind = {};
    packet_bind.sll_family = AF_PACKET;
    packet_bind.sll_protocol = htons(ETH_P_ALL);
    packet_bind.sll_ifindex = egress;
    ASSERT_EQ(bind(observer.Get(), reinterpret_cast<sockaddr*>(&packet_bind), sizeof(packet_bind)),
              0)
            << ErrnoString(errno);
    timeval timeout = {.tv_sec = 2, .tv_usec = 0};
    ASSERT_EQ(setsockopt(observer.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)), 0)
            << ErrnoString(errno);

    FdGuard socket_fd(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(socket_fd.Get(), 0) << ErrnoString(errno);
    constexpr char device[] = "veth1";
    ASSERT_EQ(setsockopt(socket_fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)), 0)
            << ErrnoString(errno);
    sockaddr_in remote = {};
    remote.sin_family = AF_INET;
    remote.sin_port = htons(9);
    remote.sin_addr.s_addr = Ipv4(kDestination);
    ASSERT_EQ(connect(socket_fd.Get(), reinterpret_cast<sockaddr*>(&remote), sizeof(remote)), 0)
            << ErrnoString(errno);
    constexpr char payload[] = "cross-interface-prefsrc";
    ASSERT_EQ(send(socket_fd.Get(), payload, sizeof(payload), 0),
              static_cast<ssize_t>(sizeof(payload)))
            << ErrnoString(errno);

    bool saw_selected_source = false;
    char frame[2048] = {};
    while (!saw_selected_source) {
        const ssize_t length = recv(observer.Get(), frame, sizeof(frame), 0);
        if (length < 0) break;
        if (length < ETH_HLEN + 20) continue;
        uint16_t ether_type = 0;
        std::memcpy(&ether_type, frame + 12, sizeof(ether_type));
        if (ntohs(ether_type) != ETH_P_IP || (static_cast<uint8_t>(frame[ETH_HLEN]) >> 4) != 4) {
            continue;
        }
        uint32_t source = 0;
        uint32_t destination = 0;
        std::memcpy(&source, frame + ETH_HLEN + 12, sizeof(source));
        std::memcpy(&destination, frame + ETH_HLEN + 16, sizeof(destination));
        saw_selected_source = source == Ipv4(kSource) && destination == remote.sin_addr.s_addr;
    }
    EXPECT_TRUE(saw_selected_source);

    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, lo, kSource, 32,
                              ++seq),
              0);
    EXPECT_FALSE(FindRoute(fd.Get(), route, ++seq).has_value());
    EXPECT_TRUE(FindRoute(fd.Get(), fallback, ++seq).has_value());

    // The cross-OIF withdrawal must also republish the egress smoltcp
    // projection. The surviving direct route therefore resolves the remote
    // destination itself, not the removed route's gateway.
    FdGuard post_delete_socket(socket(AF_INET, SOCK_DGRAM, 0));
    ASSERT_GE(post_delete_socket.Get(), 0) << ErrnoString(errno);
    ASSERT_EQ(setsockopt(post_delete_socket.Get(), SOL_SOCKET, SO_BINDTODEVICE, device,
                         sizeof(device)),
              0)
            << ErrnoString(errno);
    ASSERT_EQ(sendto(post_delete_socket.Get(), payload, sizeof(payload), 0,
                     reinterpret_cast<sockaddr*>(&remote), sizeof(remote)),
              static_cast<ssize_t>(sizeof(payload)))
            << ErrnoString(errno);
    bool saw_direct_arp = false;
    while (!saw_direct_arp) {
        const ssize_t length = recv(observer.Get(), frame, sizeof(frame), 0);
        if (length < 0) break;
        if (length < ETH_HLEN + 28) continue;
        uint16_t ether_type = 0;
        std::memcpy(&ether_type, frame + 12, sizeof(ether_type));
        if (ntohs(ether_type) != ETH_P_ARP) continue;
        uint32_t target = 0;
        std::memcpy(&target, frame + ETH_HLEN + 24, sizeof(target));
        saw_direct_arp = target == Ipv4(kDestination);
    }
    EXPECT_TRUE(saw_direct_arp);

    route.preferred_source = Ipv4("198.18.215.254");
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              EINVAL);
    EXPECT_FALSE(FindRoute(fd.Get(), route, ++seq).has_value());
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, fallback,
                               ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, Ipv4PreferredSourceSurvivesUntilLastNamespaceAddressCopy) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 6875;
    const uint32_t first = if_nametoindex("lo");
    const uint32_t second = if_nametoindex("veth-host1");
    const uint32_t egress = if_nametoindex("veth1");
    ASSERT_NE(first, 0u);
    ASSERT_NE(second, 0u);
    ASSERT_NE(egress, 0u);
    constexpr const char* kSource = "198.18.217.1";

    RouteSpec main_route = MakeIpv4Route("198.18.218.0", 24, egress);
    main_route.gateway = Ipv4("111.111.11.2");
    main_route.priority = 6875;
    main_route.preferred_source = Ipv4(kSource);
    RouteSpec default_table_route = main_route;
    default_table_route.dst = Ipv4("198.18.219.0");
    default_table_route.table = RT_TABLE_DEFAULT;

    DeleteRouteIfPresent(fd.Get(), main_route, &seq);
    DeleteRouteIfPresent(fd.Get(), default_table_route, &seq);
    DeleteAddrIfPresent(fd.Get(), first, kSource, 32, &seq);
    DeleteAddrIfPresent(fd.Get(), second, kSource, 32, &seq);
    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_NEWADDR,
                              NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, first,
                              kSource, 32, ++seq),
              0);
    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_NEWADDR,
                              NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, second,
                              kSource, 32, ++seq),
              0);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                               main_route, ++seq),
              0);
    ASSERT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                               default_table_route, ++seq),
              0);

    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, first, kSource,
                              32, ++seq),
              0);
    EXPECT_TRUE(FindRoute(fd.Get(), main_route, ++seq).has_value());
    EXPECT_TRUE(FindRoute(fd.Get(), default_table_route, ++seq).has_value());

    ASSERT_EQ(SendAddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, second, kSource,
                              32, ++seq),
              0);
    EXPECT_FALSE(FindRoute(fd.Get(), main_route, ++seq).has_value());
    // Linux fib_sync_down_addr() only invalidates the L3 domain's main table.
    auto retained = FindRoute(fd.Get(), default_table_route, ++seq);
    ASSERT_TRUE(retained.has_value());
    EXPECT_EQ(retained->preferred_source, default_table_route.preferred_source);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                               default_table_route, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, UnknownRouteFlagsAreLenientForDumpButRejectedForMutation) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 6900;
    constexpr uint32_t kUnknownRouteFlag = 0x80000000u;

    EXPECT_FALSE(DumpRoutes(fd.Get(), ++seq, kUnknownRouteFlag).empty());

    const uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);
    RouteSpec route = MakeIpv4Route("198.18.216.0", 24, lo);
    route.route_flags = kUnknownRouteFlag;
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              EOPNOTSUPP);
}

TEST(RtnetlinkRouteSemantics, Ipv6NormalizesDefaultMetricAndScopeForIdentity) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 7000;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);
    constexpr const char* kDestination = "2001:db8:ffff:9::";

    (void)SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, kDestination, 64,
                               lo, RT_SCOPE_NOWHERE, ++seq);
    ASSERT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kDestination, 64, lo, RT_SCOPE_LINK, ++seq),
              0);
    auto metadata = FindIpv6Route(fd.Get(), kDestination, 64, lo, ++seq);
    ASSERT_TRUE(metadata.has_value());
    EXPECT_EQ(metadata->priority, 1024u);
    EXPECT_EQ(metadata->scope, RT_SCOPE_UNIVERSE);

    // IPv6 scope is not a delete selector. A different request scope still
    // removes the single-path route.
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, kDestination,
                                   64, lo, RT_SCOPE_HOST, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, Ipv6GatewayValidationPreservesAttributeSemantics) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 7250;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);
    uint32_t veth = if_nametoindex("veth-host1");
    ASSERT_NE(veth, 0u);
    constexpr const char* kDestination = "2001:db8:ffff:10::";
    constexpr const char* kLinkLocalDestination = "2001:db8:ffff:11::";
    constexpr const char* kCrossDeviceLocalDestination = "2001:db8:ffff:12::";
    constexpr const char* kUnreachableGatewayDestination = "2001:db8:ffff:13::";

    (void)SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, kDestination, 64,
                               lo, RT_SCOPE_NOWHERE, ++seq);
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kDestination, 64, lo, RT_SCOPE_UNIVERSE, ++seq, "::"),
              EINVAL);
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kDestination, 64, lo, RT_SCOPE_UNIVERSE, ++seq, "::1"),
              EINVAL);

    ASSERT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kDestination, 64, lo, RT_SCOPE_LINK, ++seq),
              0);
    // Linux keeps IPv6 gateway attribute presence: an explicit :: selects a
    // direct route rather than becoming an unconstrained wildcard.
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, kDestination,
                                   64, lo, RT_SCOPE_NOWHERE, ++seq, "::"),
              0);

    // Link-local gateways require an explicit zone (OIF) and do not require a
    // recursive connected-prefix route on that interface.
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kLinkLocalDestination, 64, 0, RT_SCOPE_UNIVERSE, ++seq,
                                   "fe80::1"),
              EINVAL);
    ASSERT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kLinkLocalDestination, 64, veth, RT_SCOPE_UNIVERSE, ++seq,
                                   "fe80::1"),
              0);
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                   kLinkLocalDestination, 64, veth, RT_SCOPE_NOWHERE, ++seq,
                                   "fe80::1"),
              0);

    // A non-link-local gateway that is local anywhere in this netns remains
    // invalid even when ONLINK names a different output interface.
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kCrossDeviceLocalDestination, 64, veth, RT_SCOPE_UNIVERSE,
                                   ++seq, "::1", RTNH_F_ONLINK),
              EINVAL);
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kUnreachableGatewayDestination, 64, veth, RT_SCOPE_UNIVERSE,
                                   ++seq, "2001:db8:dead::1"),
              EHOSTUNREACH);
}

TEST(RtnetlinkRouteSemantics, Ipv6PreferredSourceRemovalSilentlyUpdatesSurvivingRoute) {
    FdGuard sender(OpenRouteSocket());
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 7400;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);
    constexpr const char* kAddress = "2001:db8:74::1";
    constexpr const char* kDestination = "2001:db8:75::";

    (void)SendIpv6RouteRequest(sender.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                               kDestination, 64, lo, RT_SCOPE_NOWHERE, ++seq);
    (void)SendIpv6AddrRequest(sender.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, lo, kAddress,
                              64, ++seq);
    ASSERT_EQ(SendIpv6AddrRequest(sender.Get(), RTM_NEWADDR,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, lo,
                                  kAddress, 64, ++seq),
              0);
    ASSERT_EQ(SendIpv6RouteRequest(sender.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kDestination, 64, lo, RT_SCOPE_UNIVERSE, ++seq, nullptr, 0,
                                   kAddress),
              0);
    auto before = FindIpv6Route(sender.Get(), kDestination, 64, lo, ++seq);
    ASSERT_TRUE(before.has_value());
    EXPECT_TRUE(before->has_preferred_source);

    FdGuard listener(OpenRouteListener(RTMGRP_IPV6_ROUTE));
    ASSERT_GE(listener.Get(), 0) << ErrnoString(errno);
    ASSERT_EQ(SendIpv6AddrRequest(sender.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, lo,
                                  kAddress, 64, ++seq),
              0);
    EXPECT_FALSE(SawIpv6RouteNotification(listener.Get(), kDestination, 64));

    auto after = FindIpv6Route(sender.Get(), kDestination, 64, lo, ++seq);
    ASSERT_TRUE(after.has_value());
    EXPECT_FALSE(after->has_preferred_source);
    EXPECT_EQ(SendIpv6RouteRequest(sender.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                   kDestination, 64, lo, RT_SCOPE_NOWHERE, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, Ipv6PreferredSourceUsesCandidateL3Ownership) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 7475;
    const uint32_t first = if_nametoindex("lo");
    const uint32_t egress = if_nametoindex("veth-host1");
    ASSERT_NE(first, 0u);
    ASSERT_NE(egress, 0u);

    constexpr const char* kGlobal = "2001:db8:76::1";
    constexpr const char* kGlobalDestination = "2001:db8:77::";
    (void)SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                               kGlobalDestination, 64, egress, RT_SCOPE_NOWHERE, ++seq);
    (void)SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, first, kGlobal,
                              64, ++seq);
    (void)SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, egress, kGlobal,
                              64, ++seq);
    ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_NEWADDR,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, first,
                                  kGlobal, 64, ++seq),
              0);
    ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_NEWADDR,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, egress,
                                  kGlobal, 64, ++seq),
              0);
    ASSERT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kGlobalDestination, 64, egress, RT_SCOPE_UNIVERSE, ++seq,
                                   nullptr, 0, kGlobal),
              0);
    ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, first,
                                  kGlobal, 64, ++seq),
              0);
    auto global_after_one = FindIpv6Route(fd.Get(), kGlobalDestination, 64, egress, ++seq);
    ASSERT_TRUE(global_after_one.has_value());
    EXPECT_TRUE(global_after_one->has_preferred_source);
    ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, egress,
                                  kGlobal, 64, ++seq),
              0);
    auto global_after_last = FindIpv6Route(fd.Get(), kGlobalDestination, 64, egress, ++seq);
    ASSERT_TRUE(global_after_last.has_value());
    EXPECT_FALSE(global_after_last->has_preferred_source);
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                   kGlobalDestination, 64, egress, RT_SCOPE_NOWHERE, ++seq),
              0);

    constexpr const char* kLinkLocal = "fe80::7475";
    constexpr const char* kLinkDestination = "2001:db8:78::";
    (void)SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                               kLinkDestination, 64, egress, RT_SCOPE_NOWHERE, ++seq);
    (void)SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, first, kLinkLocal,
                              64, ++seq);
    (void)SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, egress,
                              kLinkLocal, 64, ++seq);
    ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_NEWADDR,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, first,
                                  kLinkLocal, 64, ++seq),
              0);
    ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_NEWADDR,
                                  NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, egress,
                                  kLinkLocal, 64, ++seq),
              0);
    ASSERT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL,
                                   kLinkDestination, 64, egress, RT_SCOPE_UNIVERSE, ++seq, nullptr,
                                   0, kLinkLocal),
              0);
    ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, first,
                                  kLinkLocal, 64, ++seq),
              0);
    auto link_after_other = FindIpv6Route(fd.Get(), kLinkDestination, 64, egress, ++seq);
    ASSERT_TRUE(link_after_other.has_value());
    EXPECT_TRUE(link_after_other->has_preferred_source);
    ASSERT_EQ(SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, egress,
                                  kLinkLocal, 64, ++seq),
              0);
    auto link_after_egress = FindIpv6Route(fd.Get(), kLinkDestination, 64, egress, ++seq);
    ASSERT_TRUE(link_after_egress.has_value());
    EXPECT_FALSE(link_after_egress->has_preferred_source);
    EXPECT_EQ(SendIpv6RouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK,
                                   kLinkDestination, 64, egress, RT_SCOPE_NOWHERE, ++seq),
              0);
    (void)SendIpv6AddrRequest(fd.Get(), RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, first, kLinkLocal,
                              64, ++seq);
}

TEST(RtnetlinkRouteSemantics, RouteNotificationsExposeCreateReplaceAndAppendMutationFlags) {
    FdGuard sender(OpenRouteSocket());
    ASSERT_GE(sender.Get(), 0) << ErrnoString(errno);
    FdGuard listener(OpenRouteListener(RTMGRP_IPV4_ROUTE));
    ASSERT_GE(listener.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 7500;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    RouteSpec route = MakeIpv4Route("198.18.214.0", 24, lo);
    route.priority = 7500;
    DeleteRouteIfPresent(sender.Get(), route, &seq);
    ASSERT_EQ(SendRouteRequest(sender.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    auto create_flags = ReceiveIpv4RouteFlags(listener.Get(), RTM_NEWROUTE, route.dst,
                                               route.prefix_len);
    ASSERT_TRUE(create_flags.has_value());
    EXPECT_NE(*create_flags & NLM_F_CREATE, 0);

    RouteSpec replacement = route;
    replacement.gateway = Ipv4("127.0.0.2");
    ASSERT_EQ(SendRouteRequest(sender.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_REPLACE, replacement, ++seq),
              0);
    auto replace_flags = ReceiveIpv4RouteFlags(listener.Get(), RTM_NEWROUTE, route.dst,
                                                route.prefix_len);
    ASSERT_TRUE(replace_flags.has_value());
    EXPECT_NE(*replace_flags & NLM_F_REPLACE, 0);

    RouteSpec appended = route;
    appended.gateway = Ipv4("127.0.0.3");
    ASSERT_EQ(SendRouteRequest(sender.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_APPEND, appended,
                               ++seq),
              0);
    auto append_flags = ReceiveIpv4RouteFlags(listener.Get(), RTM_NEWROUTE, route.dst,
                                               route.prefix_len);
    ASSERT_TRUE(append_flags.has_value());
    EXPECT_NE(*append_flags & NLM_F_CREATE, 0);
    EXPECT_NE(*append_flags & NLM_F_APPEND, 0);

    EXPECT_EQ(SendRouteRequest(sender.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, appended,
                               ++seq),
              0);
    EXPECT_EQ(SendRouteRequest(sender.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, replacement,
                               ++seq),
              0);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
