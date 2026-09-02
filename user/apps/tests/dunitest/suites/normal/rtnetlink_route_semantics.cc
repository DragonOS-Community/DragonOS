#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/if_addr.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sys/socket.h>
#include <sys/time.h>
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
    uint32_t priority = 0;
    uint8_t protocol = RTPROT_STATIC;
    uint8_t kind = RTN_UNICAST;
    std::optional<uint8_t> scope;
    std::optional<uint32_t> preferred_source;
    uint8_t tos = 0;
    uint32_t route_flags = 0;
};

struct DumpedRoute {
    uint32_t dst = 0;
    uint8_t prefix_len = 0;
    uint8_t table = RT_TABLE_UNSPEC;
    uint32_t oif = 0;
    std::optional<uint32_t> gateway;
    uint32_t priority = 0;
    std::optional<uint32_t> preferred_source;
    uint8_t protocol = RTPROT_UNSPEC;
    uint8_t scope = RT_SCOPE_UNIVERSE;
    uint8_t kind = RTN_UNSPEC;
};

uint32_t Ipv4(const char* text);

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

int OpenRouteListener(uint32_t groups) {
    int fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (fd < 0) {
        return -1;
    }
    sockaddr_nl addr = {};
    addr.nl_family = AF_NETLINK;
    addr.nl_groups = groups;
    if (bind(fd, reinterpret_cast<sockaddr*>(&addr), sizeof(addr)) < 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    timeval timeout = {};
    timeout.tv_sec = 2;
    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0) {
        int saved_errno = errno;
        close(fd);
        errno = saved_errno;
        return -1;
    }
    return fd;
}

std::optional<uint16_t> ReceiveNotificationType(int fd) {
    char buf[4096] = {};
    ssize_t len = recv(fd, buf, sizeof(buf), 0);
    if (len < 0) return std::nullopt;
    auto* msg = reinterpret_cast<nlmsghdr*>(buf);
    if (!NLMSG_OK(msg, len)) return std::nullopt;
    return msg->nlmsg_type;
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

int SetLinkUp(int fd, uint32_t ifindex, bool up, uint32_t seq) {
    struct {
        nlmsghdr header;
        ifinfomsg link;
    } request = {};
    request.header.nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
    request.header.nlmsg_type = RTM_SETLINK;
    request.header.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    request.header.nlmsg_seq = seq;
    request.link.ifi_family = AF_UNSPEC;
    request.link.ifi_index = static_cast<int>(ifindex);
    request.link.ifi_flags = up ? IFF_UP : 0;
    request.link.ifi_change = IFF_UP;

    if (send(fd, &request, request.header.nlmsg_len, 0) !=
        static_cast<ssize_t>(request.header.nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

int SendRouteRequest(int fd, uint16_t type, uint16_t flags, const RouteSpec& route, uint32_t seq,
                     uint16_t extra_attr_type = 0) {
    alignas(nlmsghdr) char buf[512] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));

    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = type;
    nlh->nlmsg_flags = flags;
    nlh->nlmsg_seq = seq;

    rtm->rtm_family = AF_INET;
    rtm->rtm_dst_len = route.prefix_len;
    rtm->rtm_tos = route.tos;
    rtm->rtm_table = route.table;
    rtm->rtm_protocol = route.protocol;
    rtm->rtm_scope = route.scope.value_or(route.gateway.has_value() ? RT_SCOPE_UNIVERSE
                                                                    : RT_SCOPE_LINK);
    rtm->rtm_type = route.kind;
    rtm->rtm_flags = route.route_flags;

    if (route.prefix_len != 0) {
        AddAttr(nlh, sizeof(buf), RTA_DST, &route.dst, sizeof(route.dst));
    }
    AddAttr(nlh, sizeof(buf), RTA_OIF, &route.oif, sizeof(route.oif));
    if (route.gateway.has_value()) {
        uint32_t gw = *route.gateway;
        AddAttr(nlh, sizeof(buf), RTA_GATEWAY, &gw, sizeof(gw));
    }
    if (route.priority != 0) {
        AddAttr(nlh, sizeof(buf), RTA_PRIORITY, &route.priority, sizeof(route.priority));
    }
    if (route.preferred_source.has_value()) {
        AddAttr(nlh, sizeof(buf), RTA_PREFSRC, &*route.preferred_source,
                sizeof(*route.preferred_source));
    }
    if (extra_attr_type != 0) {
        uint32_t opaque = 0x12345678;
        AddAttr(nlh, sizeof(buf), extra_attr_type, &opaque, sizeof(opaque));
    }

    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

int SendAddrRequest(int fd, uint16_t type, uint16_t flags, uint32_t ifindex, const char* addr,
                    uint8_t prefix_len, uint32_t seq) {
    alignas(nlmsghdr) char buf[512] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(buf);
    auto* ifa = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(nlh));

    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(ifaddrmsg));
    nlh->nlmsg_type = type;
    nlh->nlmsg_flags = flags;
    nlh->nlmsg_seq = seq;

    ifa->ifa_family = AF_INET;
    ifa->ifa_prefixlen = prefix_len;
    ifa->ifa_scope = RT_SCOPE_HOST;
    ifa->ifa_index = ifindex;

    uint32_t ip = Ipv4(addr);
    AddAttr(nlh, sizeof(buf), IFA_LOCAL, &ip, sizeof(ip));
    AddAttr(nlh, sizeof(buf), IFA_ADDRESS, &ip, sizeof(ip));

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
            route.protocol = route_msg->rtm_protocol;
            route.scope = route_msg->rtm_scope;
            route.kind = route_msg->rtm_type;

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
                        if (RTA_PAYLOAD(attr) >= sizeof(uint32_t)) {
                            uint32_t gateway = 0;
                            std::memcpy(&gateway, RTA_DATA(attr), sizeof(gateway));
                            route.gateway = gateway;
                        }
                        break;
                    case RTA_PRIORITY:
                        if (RTA_PAYLOAD(attr) >= sizeof(route.priority)) {
                            std::memcpy(&route.priority, RTA_DATA(attr), sizeof(route.priority));
                        }
                        break;
                    case RTA_PREFSRC:
                        if (RTA_PAYLOAD(attr) >= sizeof(uint32_t)) {
                            uint32_t source = 0;
                            std::memcpy(&source, RTA_DATA(attr), sizeof(source));
                            route.preferred_source = source;
                        }
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

std::optional<DumpedRoute> LookupIpv4Route(int fd, const char* destination, uint32_t seq,
                                           std::optional<uint32_t> oif = std::nullopt) {
    alignas(nlmsghdr) char request_buf[256] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(request_buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));
    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = RTM_GETROUTE;
    nlh->nlmsg_flags = NLM_F_REQUEST;
    nlh->nlmsg_seq = seq;
    rtm->rtm_family = AF_INET;
    rtm->rtm_dst_len = 32;
    uint32_t dst = Ipv4(destination);
    AddAttr(nlh, sizeof(request_buf), RTA_DST, &dst, sizeof(dst));
    if (oif.has_value()) {
        AddAttr(nlh, sizeof(request_buf), RTA_OIF, &*oif, sizeof(*oif));
    }
    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) {
        return std::nullopt;
    }

    char response[4096] = {};
    ssize_t len = recv(fd, response, sizeof(response), 0);
    if (len < 0) {
        return std::nullopt;
    }
    for (auto* msg = reinterpret_cast<nlmsghdr*>(response); NLMSG_OK(msg, len);
         msg = NLMSG_NEXT(msg, len)) {
        if (msg->nlmsg_seq != seq || msg->nlmsg_type != RTM_NEWROUTE) {
            continue;
        }
        auto* route_msg = reinterpret_cast<rtmsg*>(NLMSG_DATA(msg));
        DumpedRoute route = {};
        route.prefix_len = route_msg->rtm_dst_len;
        route.table = route_msg->rtm_table;
        route.protocol = route_msg->rtm_protocol;
        route.scope = route_msg->rtm_scope;
        route.kind = route_msg->rtm_type;
        int attr_len = msg->nlmsg_len - NLMSG_LENGTH(sizeof(rtmsg));
        for (auto* attr = RTM_RTA(route_msg); RTA_OK(attr, attr_len);
             attr = RTA_NEXT(attr, attr_len)) {
            if (attr->rta_type == RTA_OIF && RTA_PAYLOAD(attr) >= sizeof(route.oif)) {
                std::memcpy(&route.oif, RTA_DATA(attr), sizeof(route.oif));
            } else if (attr->rta_type == RTA_GATEWAY &&
                       RTA_PAYLOAD(attr) >= sizeof(uint32_t)) {
                uint32_t gateway = 0;
                std::memcpy(&gateway, RTA_DATA(attr), sizeof(gateway));
                route.gateway = gateway;
            } else if (attr->rta_type == RTA_PREFSRC &&
                       RTA_PAYLOAD(attr) >= sizeof(uint32_t)) {
                uint32_t source = 0;
                std::memcpy(&source, RTA_DATA(attr), sizeof(source));
                route.preferred_source = source;
            }
        }
        return route;
    }
    return std::nullopt;
}

int SendInvalidIpv4GetAttr(int fd, const char* destination, uint16_t attr_type, uint32_t value,
                           uint32_t seq) {
    alignas(nlmsghdr) char request_buf[256] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(request_buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));
    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = RTM_GETROUTE;
    nlh->nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    nlh->nlmsg_seq = seq;
    rtm->rtm_family = AF_INET;
    rtm->rtm_dst_len = 32;
    uint32_t dst = Ipv4(destination);
    AddAttr(nlh, sizeof(request_buf), RTA_DST, &dst, sizeof(dst));
    AddAttr(nlh, sizeof(request_buf), attr_type, &value, sizeof(value));
    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

std::optional<DumpedRoute> FindRoute(int fd, const RouteSpec& spec, uint32_t seq) {
    for (const auto& route : DumpRoutes(fd, seq)) {
        if (route.dst == spec.dst && route.prefix_len == spec.prefix_len &&
            route.oif == spec.oif && (spec.priority == 0 || route.priority == spec.priority) &&
            (spec.table == RT_TABLE_UNSPEC || route.table == spec.table) &&
            route.protocol == spec.protocol && route.kind == spec.kind &&
            (!spec.scope.has_value() || route.scope == *spec.scope) &&
            (!spec.preferred_source.has_value() ||
             route.preferred_source == spec.preferred_source)) {
            return route;
        }
    }
    return std::nullopt;
}

std::optional<uint16_t> ReceiveIpv4RouteFlags(int fd, uint16_t type, uint32_t destination,
                                              uint8_t prefix_len) {
    char buf[4096] = {};
    for (;;) {
        ssize_t len = recv(fd, buf, sizeof(buf), 0);
        if (len < 0) {
            return std::nullopt;
        }
        for (auto* msg = reinterpret_cast<nlmsghdr*>(buf); NLMSG_OK(msg, len);
             msg = NLMSG_NEXT(msg, len)) {
            if (msg->nlmsg_type != type) {
                continue;
            }
            auto* route_msg = reinterpret_cast<rtmsg*>(NLMSG_DATA(msg));
            if (route_msg->rtm_family != AF_INET || route_msg->rtm_dst_len != prefix_len) {
                continue;
            }
            uint32_t actual = 0;
            int attr_len = msg->nlmsg_len - NLMSG_LENGTH(sizeof(rtmsg));
            for (auto* attr = RTM_RTA(route_msg); RTA_OK(attr, attr_len);
                 attr = RTA_NEXT(attr, attr_len)) {
                if (attr->rta_type == RTA_DST && RTA_PAYLOAD(attr) >= sizeof(actual)) {
                    std::memcpy(&actual, RTA_DATA(attr), sizeof(actual));
                }
            }
            if (actual == destination) {
                return msg->nlmsg_flags;
            }
        }
    }
}

void DeleteRouteIfPresent(int fd, const RouteSpec& spec, uint32_t* seq) {
    (void)SendRouteRequest(fd, RTM_DELROUTE, NLM_F_REQUEST | NLM_F_ACK, spec, ++(*seq));
}

void DeleteAddrIfPresent(int fd, uint32_t ifindex, const char* addr, uint8_t prefix_len,
                         uint32_t* seq) {
    (void)SendAddrRequest(fd, RTM_DELADDR, NLM_F_REQUEST | NLM_F_ACK, ifindex, addr, prefix_len,
                          ++(*seq));
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

int SendIpv6RouteRequest(int fd, uint16_t type, uint16_t flags, const char* destination,
                         uint8_t prefix_len, uint32_t oif, uint8_t scope, uint32_t seq,
                         const char* gateway = nullptr, uint32_t route_flags = 0,
                         const char* preferred_source = nullptr) {
    alignas(nlmsghdr) char buf[512] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));
    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = type;
    nlh->nlmsg_flags = flags;
    nlh->nlmsg_seq = seq;
    rtm->rtm_family = AF_INET6;
    rtm->rtm_dst_len = prefix_len;
    rtm->rtm_protocol = RTPROT_STATIC;
    rtm->rtm_scope = scope;
    rtm->rtm_type = RTN_UNICAST;
    rtm->rtm_flags = route_flags;

    in6_addr dst = {};
    if (inet_pton(AF_INET6, destination, &dst) != 1) {
        return EINVAL;
    }
    AddAttr(nlh, sizeof(buf), RTA_DST, &dst, sizeof(dst));
    AddAttr(nlh, sizeof(buf), RTA_OIF, &oif, sizeof(oif));
    if (gateway != nullptr) {
        in6_addr gw = {};
        if (inet_pton(AF_INET6, gateway, &gw) != 1) {
            return EINVAL;
        }
        AddAttr(nlh, sizeof(buf), RTA_GATEWAY, &gw, sizeof(gw));
    }
    if (preferred_source != nullptr) {
        in6_addr source = {};
        if (inet_pton(AF_INET6, preferred_source, &source) != 1) {
            return EINVAL;
        }
        AddAttr(nlh, sizeof(buf), RTA_PREFSRC, &source, sizeof(source));
    }
    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

int SendIpv4ZeroPrefixWithoutDst(int fd, uint16_t type, uint16_t flags, uint8_t prefix_len,
                                 uint32_t oif, uint32_t priority, uint32_t seq) {
    alignas(nlmsghdr) char buf[256] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));
    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = type;
    nlh->nlmsg_flags = flags;
    nlh->nlmsg_seq = seq;
    rtm->rtm_family = AF_INET;
    rtm->rtm_dst_len = prefix_len;
    rtm->rtm_protocol = RTPROT_STATIC;
    rtm->rtm_scope = RT_SCOPE_LINK;
    rtm->rtm_type = RTN_UNICAST;
    AddAttr(nlh, sizeof(buf), RTA_OIF, &oif, sizeof(oif));
    AddAttr(nlh, sizeof(buf), RTA_PRIORITY, &priority, sizeof(priority));
    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

int SendCompactIpv6PrefixRequest(int fd, uint16_t type, uint16_t flags, const char* destination,
                                 uint8_t prefix_len, uint32_t oif, uint32_t seq) {
    alignas(nlmsghdr) char buf[256] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));
    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = type;
    nlh->nlmsg_flags = flags;
    nlh->nlmsg_seq = seq;
    rtm->rtm_family = AF_INET6;
    rtm->rtm_dst_len = prefix_len;
    rtm->rtm_protocol = RTPROT_STATIC;
    rtm->rtm_scope = RT_SCOPE_LINK;
    rtm->rtm_type = RTN_UNICAST;
    in6_addr dst = {};
    if (inet_pton(AF_INET6, destination, &dst) != 1) {
        return EINVAL;
    }
    size_t compact_len = (prefix_len + 7) / 8;
    AddAttr(nlh, sizeof(buf), RTA_DST, &dst, compact_len);
    AddAttr(nlh, sizeof(buf), RTA_OIF, &oif, sizeof(oif));
    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) {
        return errno;
    }
    return RecvAck(fd, seq);
}

struct Ipv6RouteMetadata {
    uint32_t priority = 0;
    uint8_t scope = RT_SCOPE_NOWHERE;
    bool has_preferred_source = false;
};

std::optional<Ipv6RouteMetadata> FindIpv6Route(int fd, const char* destination,
                                               uint8_t prefix_len, uint32_t oif, uint32_t seq) {
    alignas(nlmsghdr) char req_buf[NLMSG_LENGTH(sizeof(rtmsg))] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(req_buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));
    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = RTM_GETROUTE;
    nlh->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    nlh->nlmsg_seq = seq;
    rtm->rtm_family = AF_INET6;
    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) {
        return std::nullopt;
    }

    in6_addr expected = {};
    if (inet_pton(AF_INET6, destination, &expected) != 1) {
        return std::nullopt;
    }
    std::optional<Ipv6RouteMetadata> found;
    char buf[8192] = {};
    bool done = false;
    while (!done) {
        ssize_t len = recv(fd, buf, sizeof(buf), 0);
        if (len < 0) {
            return std::nullopt;
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
            if (route_msg->rtm_family != AF_INET6 || route_msg->rtm_dst_len != prefix_len) {
                continue;
            }
            in6_addr actual = {};
            uint32_t actual_oif = 0;
            uint32_t priority = 0;
            bool has_preferred_source = false;
            int attr_len = msg->nlmsg_len - NLMSG_LENGTH(sizeof(rtmsg));
            for (auto* attr = RTM_RTA(route_msg); RTA_OK(attr, attr_len);
                 attr = RTA_NEXT(attr, attr_len)) {
                if (attr->rta_type == RTA_DST && RTA_PAYLOAD(attr) >= sizeof(actual)) {
                    std::memcpy(&actual, RTA_DATA(attr), sizeof(actual));
                } else if (attr->rta_type == RTA_OIF && RTA_PAYLOAD(attr) >= sizeof(actual_oif)) {
                    std::memcpy(&actual_oif, RTA_DATA(attr), sizeof(actual_oif));
                } else if (attr->rta_type == RTA_PRIORITY &&
                           RTA_PAYLOAD(attr) >= sizeof(priority)) {
                    std::memcpy(&priority, RTA_DATA(attr), sizeof(priority));
                } else if (attr->rta_type == RTA_PREFSRC) {
                    has_preferred_source = true;
                }
            }
            if (actual_oif == oif && std::memcmp(&actual, &expected, sizeof(actual)) == 0) {
                found = Ipv6RouteMetadata{priority, route_msg->rtm_scope,
                                          has_preferred_source};
            }
        }
    }
    return found;
}

int SendIpv6AddrRequest(int fd, uint16_t type, uint16_t flags, uint32_t ifindex,
                        const char* address, uint8_t prefix_len, uint32_t seq) {
    alignas(nlmsghdr) char buf[512] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(buf);
    auto* ifa = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(nlh));
    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(ifaddrmsg));
    nlh->nlmsg_type = type;
    nlh->nlmsg_flags = flags;
    nlh->nlmsg_seq = seq;
    ifa->ifa_family = AF_INET6;
    ifa->ifa_prefixlen = prefix_len;
    ifa->ifa_scope = RT_SCOPE_UNIVERSE;
    ifa->ifa_flags = IFA_F_NODAD;
    ifa->ifa_index = ifindex;

    in6_addr ip = {};
    if (inet_pton(AF_INET6, address, &ip) != 1) return EINVAL;
    AddAttr(nlh, sizeof(buf), IFA_LOCAL, &ip, sizeof(ip));
    AddAttr(nlh, sizeof(buf), IFA_ADDRESS, &ip, sizeof(ip));
    if (send(fd, nlh, nlh->nlmsg_len, 0) != static_cast<ssize_t>(nlh->nlmsg_len)) return errno;
    return RecvAck(fd, seq);
}

bool SawIpv6RouteNotification(int fd, const char* destination, uint8_t prefix_len) {
    in6_addr expected = {};
    EXPECT_EQ(inet_pton(AF_INET6, destination, &expected), 1);
    int status_flags = fcntl(fd, F_GETFL, 0);
    EXPECT_GE(status_flags, 0);
    EXPECT_EQ(fcntl(fd, F_SETFL, status_flags | O_NONBLOCK), 0);
    char buf[4096] = {};
    for (;;) {
        ssize_t len = recv(fd, buf, sizeof(buf), 0);
        if (len < 0) return false;
        for (auto* msg = reinterpret_cast<nlmsghdr*>(buf); NLMSG_OK(msg, len);
             msg = NLMSG_NEXT(msg, len)) {
            if (msg->nlmsg_type != RTM_NEWROUTE && msg->nlmsg_type != RTM_DELROUTE) continue;
            auto* route_msg = reinterpret_cast<rtmsg*>(NLMSG_DATA(msg));
            if (route_msg->rtm_family != AF_INET6 || route_msg->rtm_dst_len != prefix_len)
                continue;
            int attr_len = msg->nlmsg_len - NLMSG_LENGTH(sizeof(rtmsg));
            for (auto* attr = RTM_RTA(route_msg); RTA_OK(attr, attr_len);
                 attr = RTA_NEXT(attr, attr_len)) {
                if (attr->rta_type == RTA_DST && RTA_PAYLOAD(attr) >= sizeof(expected) &&
                    std::memcmp(RTA_DATA(attr), &expected, sizeof(expected)) == 0)
                    return true;
            }
        }
    }
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

TEST(RtnetlinkRouteSemantics, CrossInterfacePreferredSourceIsRejectedAtomically) {
    FdGuard fd(OpenRouteSocket());
    ASSERT_GE(fd.Get(), 0) << ErrnoString(errno);
    uint32_t seq = 6750;
    uint32_t veth = if_nametoindex("veth-host1");
    ASSERT_NE(veth, 0u);

    RouteSpec route = MakeIpv4Route("198.18.214.0", 24, veth);
    route.preferred_source = Ipv4("127.0.0.1");
    DeleteRouteIfPresent(fd.Get(), route, &seq);
    EXPECT_EQ(SendRouteRequest(fd.Get(), RTM_NEWROUTE,
                               NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, route,
                               ++seq),
              EOPNOTSUPP);
    EXPECT_FALSE(FindRoute(fd.Get(), route, ++seq).has_value());
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
