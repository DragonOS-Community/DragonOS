#pragma once

#include <gtest/gtest.h>

#include <arpa/inet.h>
#include <errno.h>
#include <fcntl.h>
#include <linux/if_addr.h>
#include <linux/if_ether.h>
#include <linux/if_packet.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <netinet/in.h>
#include <sched.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/wait.h>
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

std::vector<DumpedRoute> DumpRoutes(int fd, uint32_t seq, uint32_t route_flags = 0) {
    alignas(nlmsghdr) char req_buf[NLMSG_LENGTH(sizeof(rtmsg))] = {};
    auto* nlh = reinterpret_cast<nlmsghdr*>(req_buf);
    auto* rtm = reinterpret_cast<rtmsg*>(NLMSG_DATA(nlh));

    nlh->nlmsg_len = NLMSG_LENGTH(sizeof(rtmsg));
    nlh->nlmsg_type = RTM_GETROUTE;
    nlh->nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    nlh->nlmsg_seq = seq;
    rtm->rtm_family = AF_INET;
    rtm->rtm_flags = route_flags;

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

bool SawIpv6RouteNotification(int fd, const char* destination, uint8_t prefix_len,
                              uint16_t expected_type = 0) {
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
            if (expected_type != 0 && msg->nlmsg_type != expected_type) continue;
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

int RunIpv4BuiltinRuleChain() {
    FdGuard fd(OpenRouteSocket());
    if (fd.Get() < 0) return 1000 + errno;
    const uint32_t egress = if_nametoindex("veth1");
    if (egress == 0) return 2000 + errno;
    uint32_t seq = 1;

    RouteSpec fallback = MakeIpv4Route("203.0.113.0", 24, egress);
    fallback.gateway = Ipv4("111.111.11.2");
    fallback.table = RT_TABLE_DEFAULT;
    if (const int error = SendRouteRequest(
                fd.Get(), RTM_NEWROUTE,
                NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, fallback, seq++);
        error != 0) {
        return 3000 + error;
    }

    // RPDB priority is table-wide: even a less-specific main-table route must
    // win before the IPv4 default table is consulted.
    RouteSpec main = MakeIpv4Route("203.0.0.0", 16, egress);
    if (const int error = SendRouteRequest(
                fd.Get(), RTM_NEWROUTE,
                NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL, main, seq++);
        error != 0) {
        return 4000 + error;
    }
    // Constrain both lookups to the interface owned by this test. The shared
    // dunitest guest may already have an unrelated main-table default route;
    // Linux correctly stops at that route for an unconstrained lookup.
    auto lookup = LookupIpv4Route(fd.Get(), "203.0.113.7", seq++, egress);
    if (!lookup.has_value() || lookup->table != RT_TABLE_MAIN) return 5000;
    if (const int error = SendRouteRequest(fd.Get(), RTM_DELROUTE,
                                           NLM_F_REQUEST | NLM_F_ACK, main, seq++);
        error != 0) {
        return 6000 + error;
    }

    lookup = LookupIpv4Route(fd.Get(), "203.0.113.7", seq++, egress);
    if (!lookup.has_value() || lookup->table != RT_TABLE_DEFAULT) return 7000;

    FdGuard observer(socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL)));
    if (observer.Get() < 0) return 9000 + errno;
    sockaddr_ll packet_bind = {};
    packet_bind.sll_family = AF_PACKET;
    packet_bind.sll_protocol = htons(ETH_P_ALL);
    packet_bind.sll_ifindex = egress;
    if (bind(observer.Get(), reinterpret_cast<sockaddr*>(&packet_bind), sizeof(packet_bind)) != 0) {
        return 10000 + errno;
    }
    timeval timeout = {.tv_sec = 2, .tv_usec = 0};
    if (setsockopt(observer.Get(), SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) != 0) {
        return 11000 + errno;
    }

    FdGuard socket_fd(socket(AF_INET, SOCK_DGRAM, 0));
    if (socket_fd.Get() < 0) return 12000 + errno;
    constexpr char device[] = "veth1";
    if (setsockopt(socket_fd.Get(), SOL_SOCKET, SO_BINDTODEVICE, device, sizeof(device)) != 0) {
        return 13000 + errno;
    }
    sockaddr_in remote = {};
    remote.sin_family = AF_INET;
    remote.sin_port = htons(9);
    remote.sin_addr.s_addr = Ipv4("203.0.113.7");
    if (connect(socket_fd.Get(), reinterpret_cast<sockaddr*>(&remote), sizeof(remote)) != 0) {
        return 14000 + errno;
    }
    constexpr char payload[] = "default-table-dataplane";
    if (send(socket_fd.Get(), payload, sizeof(payload), 0) != static_cast<ssize_t>(sizeof(payload))) {
        return 15000 + errno;
    }

    char frame[2048] = {};
    for (;;) {
        const ssize_t length = recv(observer.Get(), frame, sizeof(frame), 0);
        if (length < 0) return 16000 + errno;
        size_t ip_offset = 0;
        uint16_t ether_type = 0;
        if (length >= static_cast<ssize_t>(ETH_HLEN)) {
            std::memcpy(&ether_type, frame + 12, sizeof(ether_type));
        }
        if (ntohs(ether_type) == ETH_P_IP) {
            ip_offset = ETH_HLEN;
        }
        if (length < static_cast<ssize_t>(ip_offset + 20) ||
            (static_cast<uint8_t>(frame[ip_offset]) >> 4) != 4) {
            continue;
        }
        uint32_t destination = 0;
        std::memcpy(&destination, frame + ip_offset + 16, sizeof(destination));
        if (destination == remote.sin_addr.s_addr) return 0;
    }
}

}  // namespace
