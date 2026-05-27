#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <unistd.h>

#include <cstring>
#include <optional>
#include <string>
#include <vector>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() { Reset(); }

    int Get() const { return fd_; }

    void Reset(int fd = -1) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

  private:
    int fd_;
};

std::string ErrnoString(int err) {
    return std::to_string(err) + " (" + std::strerror(err) + ")";
}

struct RouteSpec {
    uint32_t dst = 0;
    uint8_t prefix_len = 0;
    uint8_t table = RT_TABLE_UNSPEC;
    uint32_t oif = 0;
    std::optional<uint32_t> gateway;
};

struct DumpedRoute {
    uint32_t dst = 0;
    uint8_t prefix_len = 0;
    uint8_t table = RT_TABLE_UNSPEC;
    uint32_t oif = 0;
    bool has_gateway = false;
};

int OpenRouteSocket() {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) {
        return -1;
    }

    sockaddr_nl addr = {};
    addr.nl_family = AF_NETLINK;
    if (bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    return fd;
}

void AddAttr(nlmsghdr* nlh, size_t max_len, uint16_t type, const void* data, size_t len) {
    size_t attr_len = RTA_LENGTH(len);
    size_t aligned_len = RTA_ALIGN(attr_len);
    ASSERT_LE(static_cast<size_t>(nlh->nlmsg_len) + aligned_len, max_len);

    auto* rta = reinterpret_cast<rtattr*>(reinterpret_cast<char*>(nlh) + NLMSG_ALIGN(nlh->nlmsg_len));
    rta->rta_type = type;
    rta->rta_len = attr_len;
    std::memcpy(RTA_DATA(rta), data, len);
    std::memset(reinterpret_cast<char*>(rta) + attr_len, 0, aligned_len - attr_len);
    nlh->nlmsg_len = NLMSG_ALIGN(nlh->nlmsg_len) + aligned_len;
}

int RecvAck(int fd, uint32_t seq) {
    char buf[4096] = {};
    for (;;) {
        ssize_t len = recv(fd, buf, sizeof(buf), 0);
        if (len < 0) {
            return errno;
        }

        for (auto* nlh = reinterpret_cast<nlmsghdr*>(buf); NLMSG_OK(nlh, len);
             nlh = NLMSG_NEXT(nlh, len)) {
            if (nlh->nlmsg_seq != seq) {
                continue;
            }
            if (nlh->nlmsg_type != NLMSG_ERROR) {
                continue;
            }

            auto* err = reinterpret_cast<nlmsgerr*>(NLMSG_DATA(nlh));
            return err->error == 0 ? 0 : -err->error;
        }
    }
}

int SendRouteRequest(int fd, uint16_t type, uint16_t flags, const RouteSpec& route, uint32_t seq) {
    alignas(nlmsghdr) char buf[512] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));

    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = type;
    nlh->nlmsg_flags = flags;
    nlh->nlmsg_seq = seq;

    rtm->rtm_family = AF_INET;
    rtm->rtm_dst_len = route.prefix_len;
    rtm->rtm_table = route.table;
    rtm->rtm_protocol = RTPROT_STATIC;
    rtm->rtm_scope = route.gateway.has_value() ? RT_SCOPE_UNIVERSE : RT_SCOPE_LINK;
    rtm->rtm_type = RTN_UNICAST;

    if (route.prefix_len != 0) {
        AddAttr(nlh, sizeof(buf), RTA_DST, &route.dst, sizeof(route.dst));
    }
    AddAttr(nlh, sizeof(buf), RTA_OIF, &route.oif, sizeof(route.oif));
    if (route.gateway.has_value()) {
        uint32_t gw = *route.gateway;
        AddAttr(nlh, sizeof(buf), RTA_GATEWAY, &gw, sizeof(gw));
    }

    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

std::vector<DumpedRoute> DumpRoutes(int fd, uint32_t seq) {
    alignas(nlmsghdr) char req_buf[NLMSG_LENGTH(sizeof(rtmsg))] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(req_buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));

    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = RTM_GETROUTE;
    nlh->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    nlh->nlmsg_seq = seq;
    rtm->rtm_family = AF_INET;

    EXPECT_EQ(send(fd, nlh, nlh->nlmsg_len, 0), static_cast<ssize_t>(nlh->nlmsg_len))
            << "send RTM_GETROUTE failed: " << ErrnoString(errno);

    std::vector<DumpedRoute> routes;
    char buf[8192] = {};
    bool done = false;
    while (!done) {
        ssize_t len = recv(fd, buf, sizeof(buf), 0);
        if (len < 0) {
            ADD_FAILURE() << "recv RTM_GETROUTE failed: " << ErrnoString(errno);
            break;
        }

        for (auto* msg = reinterpret_cast<nlmsghdr*>(buf); NLMSG_OK(msg, len);
             msg = NLMSG_NEXT(msg, len)) {
            if (msg->nlmsg_seq != seq) {
                continue;
            }
            if (msg->nlmsg_type == NLMSG_DONE) {
                done = true;
                break;
            }
            if (msg->nlmsg_type != RTM_NEWROUTE) {
                continue;
            }

            auto* route_msg = reinterpret_cast<rtmsg*>(NLMSG_DATA(msg));
            DumpedRoute route = {};
            route.prefix_len = route_msg->rtm_dst_len;
            route.table = route_msg->rtm_table;

            int attr_len = msg->nlmsg_len - NLMSG_LENGTH(sizeof(rtmsg));
            for (auto* attr = RTM_RTA(route_msg); RTA_OK(attr, attr_len);
                 attr = RTA_NEXT(attr, attr_len)) {
                switch (attr->rta_type) {
                    case RTA_DST:
                        if (RTA_PAYLOAD(attr) >= sizeof(route.dst)) {
                            std::memcpy(&route.dst, RTA_DATA(attr), sizeof(route.dst));
                        }
                        break;
                    case RTA_OIF:
                        if (RTA_PAYLOAD(attr) >= sizeof(route.oif)) {
                            std::memcpy(&route.oif, RTA_DATA(attr), sizeof(route.oif));
                        }
                        break;
                    case RTA_GATEWAY:
                        route.has_gateway = true;
                        break;
                    default:
                        break;
                }
            }
            routes.push_back(route);
        }
    }
    return routes;
}

std::optional<DumpedRoute> FindRoute(int fd, const RouteSpec& spec, uint32_t seq) {
    for (const auto& route : DumpRoutes(fd, seq)) {
        if (route.dst == spec.dst && route.prefix_len == spec.prefix_len &&
            route.oif == spec.oif) {
            return route;
        }
    }
    return std::nullopt;
}

void DeleteRouteIfPresent(int fd, const RouteSpec& spec, uint32_t* seq) {
    (void)SendRouteRequest(fd, RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, spec, ++(*seq));
}

uint32_t Ipv4(const char* text) {
    in_addr addr = {};
    EXPECT_EQ(inet_pton(AF_INET, text, &addr), 1) << text;
    return addr.s_addr;
}

RouteSpec MakeIpv4Route(const char* dst, uint8_t prefix_len, uint32_t oif) {
    RouteSpec route = {};
    route.dst = Ipv4(dst);
    route.prefix_len = prefix_len;
    route.oif = oif;
    return route;
}

}  // namespace

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
    EXPECT_FALSE(dumped->has_gateway);

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
    DeleteRouteIfPresent(fd.Get(), route, &seq);

    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              0);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route, ++seq),
              0);
}

TEST(RtnetlinkRouteSemantics, FailedDataPlaneSyncRollsBackNormalizedTableRoute) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << "socket(AF_NETLINK, NETLINK_ROUTE) failed: " << ErrnoString(errno);
    uint32_t seq = 3000;
    uint32_t lo = if_nametoindex("lo");
    ASSERT_NE(lo, 0u);

    std::vector<RouteSpec> added;
    std::optional<RouteSpec> failed;
    for (int i = 0; i < 16; ++i) {
        std::string dst = std::string("198.18.") + std::to_string(100 + i) + ".0";
        RouteSpec route = MakeIpv4Route(dst.c_str(), 24, lo);
        DeleteRouteIfPresent(fd.Get(), route, &seq);
        int err = SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                                   NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                                   ++seq);
        if (err == 0) {
            added.push_back(route);
            continue;
        }
        if (err == ENOSPC) {
            failed = route;
            break;
        }
        FAIL() << "unexpected RTM_NEWROUTE error: " << ErrnoString(err);
    }

    ASSERT_TRUE(failed.has_value()) << "route table did not reach ENOSPC during rollback test";
    EXPECT_FALSE(FindRoute(fd.Get(), *failed, ++seq).has_value())
            << "failed add left a netlink control-plane route behind";

    for (const auto& route : added) {
        EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, route,
                                   ++seq),
                  0);
    }
}

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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
