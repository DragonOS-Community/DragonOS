#include <gtest/gtest.h>

#include <net/if.h>
#include <linux/if_link.h>
#include <linux/if.h>
#include <linux/if_addr.h>
#include <linux/if_packet.h>
#include <linux/if_ether.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <linux/veth.h>
#include <arpa/inet.h>
#include <fcntl.h>
#include <sched.h>
#include <signal.h>
#include <poll.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <string>
#include <optional>
#include <vector>

namespace {

struct RouteFd {
    int fd = -1;
    explicit RouteFd(uint32_t groups = 0) {
        fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
        if (fd < 0) return;
        sockaddr_nl address {};
        address.nl_family = AF_NETLINK;
        address.nl_groups = groups;
        if (bind(fd, reinterpret_cast<sockaddr*>(&address), sizeof(address)) < 0) {
            close(fd);
            fd = -1;
            return;
        }
        timeval timeout {2, 0};
        setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    }
    ~RouteFd() { if (fd >= 0) close(fd); }
};

struct LinkRequest {
    std::array<uint8_t, 1024> bytes {};
    nlmsghdr* header() { return reinterpret_cast<nlmsghdr*>(bytes.data()); }
    ifinfomsg* body() { return reinterpret_cast<ifinfomsg*>(NLMSG_DATA(header())); }
    LinkRequest(uint16_t type, uint16_t flags, uint32_t seq) {
        header()->nlmsg_len = NLMSG_LENGTH(sizeof(ifinfomsg));
        header()->nlmsg_type = type;
        header()->nlmsg_flags = flags | NLM_F_REQUEST | NLM_F_ACK;
        header()->nlmsg_seq = seq;
        body()->ifi_family = AF_UNSPEC;
    }
    bool add(uint16_t type, const void* data, size_t len) {
        const size_t offset = NLMSG_ALIGN(header()->nlmsg_len);
        const size_t next = offset + RTA_ALIGN(RTA_LENGTH(len));
        if (next > bytes.size()) return false;
        auto* attr = reinterpret_cast<rtattr*>(bytes.data() + offset);
        attr->rta_type = type;
        attr->rta_len = RTA_LENGTH(len);
        if (len) std::memcpy(RTA_DATA(attr), data, len);
        header()->nlmsg_len = next;
        return true;
    }
    bool name(const std::string& value) {
        return add(IFLA_IFNAME, value.c_str(), value.size() + 1);
    }
};

void AppendAttr(std::vector<uint8_t>& bytes, uint16_t type, const void* data, size_t len) {
    const size_t offset = bytes.size();
    bytes.resize(offset + RTA_ALIGN(RTA_LENGTH(len)), 0);
    auto* attr = reinterpret_cast<rtattr*>(bytes.data() + offset);
    attr->rta_type = type;
    attr->rta_len = RTA_LENGTH(len);
    if (len) std::memcpy(RTA_DATA(attr), data, len);
}

int SendAck(int fd, LinkRequest& request) {
    if (send(fd, request.bytes.data(), request.header()->nlmsg_len, 0) < 0) return errno;
    std::array<uint8_t, 4096> reply {};
    for (int tries = 0; tries < 32; ++tries) {
        const ssize_t count = recv(fd, reply.data(), reply.size(), 0);
        if (count < 0) return errno;
        int remaining = static_cast<int>(count);
        for (auto* message = reinterpret_cast<nlmsghdr*>(reply.data());
             NLMSG_OK(message, remaining); message = NLMSG_NEXT(message, remaining)) {
            if (message->nlmsg_seq != request.header()->nlmsg_seq ||
                message->nlmsg_type != NLMSG_ERROR) continue;
            const auto* error = reinterpret_cast<const nlmsgerr*>(NLMSG_DATA(message));
            return error->error == 0 ? 0 : -error->error;
        }
    }
    return ETIMEDOUT;
}

struct LinkState {
    uint32_t index = 0;
    uint32_t link = 0;
    int32_t link_netnsid = -1;
    uint32_t master = 0;
    uint32_t mtu = 0;
    uint32_t tx_queue_len = 0;
    std::string kind;
};

std::optional<LinkState> QueryLinkRequest(int fd, LinkRequest& request, uint32_t seq) {
    if (send(fd, request.bytes.data(), request.header()->nlmsg_len, 0) < 0) return std::nullopt;
    std::array<uint8_t, 4096> reply {};
    for (int tries = 0; tries < 32; ++tries) {
        const ssize_t count = recv(fd, reply.data(), reply.size(), 0);
        if (count < 0) return std::nullopt;
        int remaining = static_cast<int>(count);
        for (auto* message = reinterpret_cast<nlmsghdr*>(reply.data());
             NLMSG_OK(message, remaining); message = NLMSG_NEXT(message, remaining)) {
            if (message->nlmsg_seq != seq || message->nlmsg_type != RTM_NEWLINK) continue;
            const auto* body = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(message));
            LinkState state;
            state.index = body->ifi_index;
            int bytes = IFLA_PAYLOAD(message);
            for (auto* attr = IFLA_RTA(body); RTA_OK(attr, bytes); attr = RTA_NEXT(attr, bytes)) {
                if (attr->rta_type == IFLA_LINK && RTA_PAYLOAD(attr) == sizeof(uint32_t)) {
                    std::memcpy(&state.link, RTA_DATA(attr), sizeof(uint32_t));
                } else if (attr->rta_type == IFLA_LINK_NETNSID &&
                           RTA_PAYLOAD(attr) == sizeof(int32_t)) {
                    std::memcpy(&state.link_netnsid, RTA_DATA(attr), sizeof(int32_t));
                } else if (attr->rta_type == IFLA_MASTER &&
                           RTA_PAYLOAD(attr) == sizeof(uint32_t)) {
                    std::memcpy(&state.master, RTA_DATA(attr), sizeof(uint32_t));
                } else if (attr->rta_type == IFLA_MTU &&
                           RTA_PAYLOAD(attr) == sizeof(uint32_t)) {
                    std::memcpy(&state.mtu, RTA_DATA(attr), sizeof(uint32_t));
                } else if (attr->rta_type == IFLA_TXQLEN &&
                           RTA_PAYLOAD(attr) == sizeof(uint32_t)) {
                    std::memcpy(&state.tx_queue_len, RTA_DATA(attr), sizeof(uint32_t));
                } else if ((attr->rta_type & NLA_TYPE_MASK) == IFLA_LINKINFO) {
                    int info_bytes = RTA_PAYLOAD(attr);
                    for (auto* info = reinterpret_cast<rtattr*>(RTA_DATA(attr));
                         RTA_OK(info, info_bytes); info = RTA_NEXT(info, info_bytes)) {
                        if ((info->rta_type & NLA_TYPE_MASK) == IFLA_INFO_KIND) {
                            const char* value = static_cast<const char*>(RTA_DATA(info));
                            state.kind.assign(value, strnlen(value, RTA_PAYLOAD(info)));
                        }
                    }
                }
            }
            return state;
        }
    }
    return std::nullopt;
}

std::optional<LinkState> QueryLink(int fd, const std::string& name, uint32_t seq) {
    LinkRequest request(RTM_GETLINK, 0, seq);
    request.header()->nlmsg_flags = NLM_F_REQUEST;
    if (!request.name(name)) return std::nullopt;
    return QueryLinkRequest(fd, request, seq);
}

std::optional<LinkState> QueryLinkByIndex(int fd, uint32_t index, uint32_t seq) {
    LinkRequest request(RTM_GETLINK, 0, seq);
    request.header()->nlmsg_flags = NLM_F_REQUEST;
    request.body()->ifi_index = index;
    return QueryLinkRequest(fd, request, seq);
}

std::string UniqueName(const char* suffix) {
    return "dv" + std::to_string(getpid() % 100000) + suffix;
}

void AddKind(LinkRequest& request, const char* kind,
             const std::vector<uint8_t>& data = {}) {
    std::vector<uint8_t> info;
    AppendAttr(info, IFLA_INFO_KIND, kind, std::strlen(kind) + 1);
    if (!data.empty()) AppendAttr(info, IFLA_INFO_DATA, data.data(), data.size());
    ASSERT_TRUE(request.add(IFLA_LINKINFO, info.data(), info.size()));
}

int DeleteByName(int fd, const std::string& name, uint32_t seq) {
    LinkRequest request(RTM_DELLINK, 0, seq);
    if (!request.name(name)) return EMSGSIZE;
    return SendAck(fd, request);
}

int SetLinkMtu(int fd, uint32_t index, uint32_t mtu, uint32_t seq) {
    LinkRequest request(RTM_SETLINK, 0, seq);
    request.body()->ifi_index = index;
    if (!request.add(IFLA_MTU, &mtu, sizeof(mtu))) return EMSGSIZE;
    return SendAck(fd, request);
}

int SetLinkMaster(int fd, uint32_t index, uint32_t master, uint32_t seq) {
    LinkRequest request(RTM_SETLINK, 0, seq);
    request.body()->ifi_index = index;
    if (!request.add(IFLA_MASTER, &master, sizeof(master))) return EMSGSIZE;
    return SendAck(fd, request);
}

int SetLinkUp(int fd, uint32_t index, bool up, uint32_t seq) {
    LinkRequest request(RTM_SETLINK, 0, seq);
    request.body()->ifi_index = index;
    request.body()->ifi_change = IFF_UP;
    request.body()->ifi_flags = up ? IFF_UP : 0;
    return SendAck(fd, request);
}

bool SawBridgeCarrierEvent(int fd, uint32_t index, bool carrier) {
    for (int attempt = 0; attempt < 24; ++attempt) {
        pollfd ready {fd, POLLIN, 0};
        if (poll(&ready, 1, 1000) <= 0) return false;
        std::array<uint8_t, 4096> bytes {};
        const ssize_t count = recv(fd, bytes.data(), bytes.size(), 0);
        if (count <= 0) return false;
        int remaining = static_cast<int>(count);
        for (auto* message = reinterpret_cast<nlmsghdr*>(bytes.data());
             NLMSG_OK(message, remaining); message = NLMSG_NEXT(message, remaining)) {
            if (message->nlmsg_type != RTM_NEWLINK ||
                message->nlmsg_len < NLMSG_LENGTH(sizeof(ifinfomsg))) continue;
            const auto* body = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(message));
            if (body->ifi_index == static_cast<int>(index) &&
                static_cast<bool>(body->ifi_flags & IFF_LOWER_UP) == carrier) return true;
        }
    }
    return false;
}

struct CleanupLinks {
    int fd;
    std::string bridge;
    std::string veth;
    std::string extra_veth;
    ~CleanupLinks() {
        if (!veth.empty()) DeleteByName(fd, veth, 4198);
        if (!extra_veth.empty()) DeleteByName(fd, extra_veth, 4197);
        if (!bridge.empty()) DeleteByName(fd, bridge, 4199);
    }
};

TEST(RtnetlinkBridgeVethSemantics, NestedPeerNameMasterAndPairedDeletion) {
    RouteFd route;
    ASSERT_GE(route.fd, 0);
    const std::string bridge = UniqueName("br");
    const std::string first = UniqueName("a");
    const std::string peer = UniqueName("b");
    CleanupLinks cleanup {route.fd, bridge, first};

    LinkRequest create_bridge(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4101);
    ASSERT_TRUE(create_bridge.name(bridge));
    AddKind(create_bridge, "bridge");
    // Docker 29 constructs LinkAttrs directly (without NewLinkAttrs), so
    // vishvananda/netlink sends the zero-valued TxQLen explicitly.
    const uint32_t zero_qlen = 0;
    ASSERT_TRUE(create_bridge.add(IFLA_TXQLEN, &zero_qlen, sizeof(zero_qlen)));
    const std::array<uint8_t, 6> bridge_mac {0x02, 0x42, 0x70, 0x36, 0x01,
                                             static_cast<uint8_t>(getpid() & 0xff)};
    ASSERT_TRUE(create_bridge.add(IFLA_ADDRESS, bridge_mac.data(), bridge_mac.size()));
    const int bridge_result = SendAck(route.fd, create_bridge);
    if (bridge_result == EPERM) GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
    ASSERT_EQ(bridge_result, 0);

    std::vector<uint8_t> peer_payload(sizeof(ifinfomsg), 0);
    AppendAttr(peer_payload, IFLA_IFNAME, peer.c_str(), peer.size() + 1);
    const uint32_t peer_mtu = 1400;
    AppendAttr(peer_payload, IFLA_MTU, &peer_mtu, sizeof(peer_mtu));
    AppendAttr(peer_payload, IFLA_TXQLEN, &zero_qlen, sizeof(zero_qlen));
    std::vector<uint8_t> veth_data;
    AppendAttr(veth_data, VETH_INFO_PEER, peer_payload.data(), peer_payload.size());
    LinkRequest create_veth(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4102);
    ASSERT_TRUE(create_veth.name(first));
    AddKind(create_veth, "veth", veth_data);
    const uint32_t first_mtu = 1450;
    ASSERT_TRUE(create_veth.add(IFLA_MTU, &first_mtu, sizeof(first_mtu)));
    ASSERT_EQ(SendAck(route.fd, create_veth), 0);

    const unsigned int first_index = if_nametoindex(first.c_str());
    const unsigned int peer_index = if_nametoindex(peer.c_str());
    ASSERT_NE(first_index, 0u);
    ASSERT_NE(peer_index, 0u) << "VETH_INFO_PEER must retain the supplied name";
    const unsigned int bridge_index = if_nametoindex(bridge.c_str());
    ASSERT_NE(bridge_index, 0u);
    const auto bridge_state = QueryLink(route.fd, bridge, 4106);
    const auto first_state = QueryLink(route.fd, first, 4107);
    const auto peer_state = QueryLink(route.fd, peer, 4109);
    ASSERT_TRUE(bridge_state.has_value());
    ASSERT_TRUE(first_state.has_value());
    ASSERT_TRUE(peer_state.has_value());
    EXPECT_EQ(bridge_state->kind, "bridge");
    EXPECT_EQ(bridge_state->tx_queue_len, 0u);
    EXPECT_EQ(first_state->kind, "veth");
    EXPECT_EQ(first_state->link, peer_index);
    EXPECT_EQ(first_state->mtu, first_mtu);
    EXPECT_EQ(peer_state->mtu, peer_mtu);
    EXPECT_EQ(peer_state->tx_queue_len, 0u);

    LinkRequest set_qlen(RTM_SETLINK, 0, 4110);
    set_qlen.body()->ifi_index = bridge_index;
    const uint32_t new_qlen = 77;
    ASSERT_TRUE(set_qlen.add(IFLA_TXQLEN, &new_qlen, sizeof(new_qlen)));
    EXPECT_EQ(SendAck(route.fd, set_qlen), 0);
    const auto updated_bridge = QueryLink(route.fd, bridge, 4111);
    ASSERT_TRUE(updated_bridge.has_value());
    EXPECT_EQ(updated_bridge->tx_queue_len, new_qlen);

    LinkRequest enslave(RTM_SETLINK, 0, 4103);
    enslave.body()->ifi_index = first_index;
    ASSERT_TRUE(enslave.add(IFLA_MASTER, &bridge_index, sizeof(bridge_index)));
    EXPECT_EQ(SendAck(route.fd, enslave), 0);
    const auto enslaved_state = QueryLink(route.fd, first, 4108);
    ASSERT_TRUE(enslaved_state.has_value());
    EXPECT_EQ(enslaved_state->master, bridge_index);
    const auto auto_bridge_mtu = QueryLink(route.fd, bridge, 4112);
    ASSERT_TRUE(auto_bridge_mtu.has_value());
    EXPECT_EQ(auto_bridge_mtu->mtu, first_mtu);

    LinkRequest shrink_port(RTM_SETLINK, 0, 4113);
    shrink_port.body()->ifi_index = first_index;
    const uint32_t smaller_mtu = 1350;
    ASSERT_TRUE(shrink_port.add(IFLA_MTU, &smaller_mtu, sizeof(smaller_mtu)));
    EXPECT_EQ(SendAck(route.fd, shrink_port), 0);
    const auto shrunk_bridge = QueryLink(route.fd, bridge, 4114);
    ASSERT_TRUE(shrunk_bridge.has_value());
    EXPECT_EQ(shrunk_bridge->mtu, smaller_mtu);

    LinkRequest detach(RTM_SETLINK, 0, 4115);
    detach.body()->ifi_index = first_index;
    const uint32_t no_master = 0;
    ASSERT_TRUE(detach.add(IFLA_MASTER, &no_master, sizeof(no_master)));
    EXPECT_EQ(SendAck(route.fd, detach), 0);
    const auto empty_bridge = QueryLink(route.fd, bridge, 4116);
    ASSERT_TRUE(empty_bridge.has_value());
    EXPECT_EQ(empty_bridge->mtu, 1500u);

    LinkRequest explicit_mtu(RTM_SETLINK, 0, 4117);
    explicit_mtu.body()->ifi_index = bridge_index;
    const uint32_t user_mtu = 1200;
    ASSERT_TRUE(explicit_mtu.add(IFLA_MTU, &user_mtu, sizeof(user_mtu)));
    EXPECT_EQ(SendAck(route.fd, explicit_mtu), 0);
    LinkRequest reattach(RTM_SETLINK, 0, 4118);
    reattach.body()->ifi_index = first_index;
    ASSERT_TRUE(reattach.add(IFLA_MASTER, &bridge_index, sizeof(bridge_index)));
    EXPECT_EQ(SendAck(route.fd, reattach), 0);
    const auto fixed_bridge = QueryLink(route.fd, bridge, 4119);
    ASSERT_TRUE(fixed_bridge.has_value());
    EXPECT_EQ(fixed_bridge->mtu, user_mtu);

    EXPECT_EQ(DeleteByName(route.fd, first, 4104), 0);
    EXPECT_EQ(if_nametoindex(first.c_str()), 0u);
    EXPECT_EQ(if_nametoindex(peer.c_str()), 0u);
    EXPECT_EQ(DeleteByName(route.fd, bridge, 4105), 0);
}

TEST(RtnetlinkBridgeVethSemantics, BridgeAutoMtuAndExplicitOverride) {
    RouteFd route;
    ASSERT_GE(route.fd, 0);
    const std::string bridge = UniqueName("mb");
    const std::string first = UniqueName("m1");
    const std::string second = UniqueName("m2");
    CleanupLinks cleanup {route.fd, bridge, first, second};

    LinkRequest create_bridge(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4501);
    ASSERT_TRUE(create_bridge.name(bridge));
    AddKind(create_bridge, "bridge");
    const int bridge_result = SendAck(route.fd, create_bridge);
    if (bridge_result == EPERM) GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
    ASSERT_EQ(bridge_result, 0);
    const uint32_t bridge_index = if_nametoindex(bridge.c_str());
    ASSERT_NE(bridge_index, 0u);

    const auto create_veth = [&](const std::string& name, uint32_t mtu, uint32_t seq) {
        LinkRequest request(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, seq);
        if (!request.name(name)) return EMSGSIZE;
        AddKind(request, "veth");
        if (!request.add(IFLA_MTU, &mtu, sizeof(mtu))) return EMSGSIZE;
        return SendAck(route.fd, request);
    };
    ASSERT_EQ(create_veth(first, 1400, 4502), 0);
    ASSERT_EQ(create_veth(second, 1300, 4503), 0);
    const uint32_t first_index = if_nametoindex(first.c_str());
    const uint32_t second_index = if_nametoindex(second.c_str());
    ASSERT_NE(first_index, 0u);
    ASSERT_NE(second_index, 0u);

    const auto bridge_mtu = [&](uint32_t seq) {
        const auto state = QueryLink(route.fd, bridge, seq);
        EXPECT_TRUE(state.has_value());
        return state ? state->mtu : 0u;
    };
    ASSERT_EQ(bridge_mtu(4510), 1500u);
    ASSERT_EQ(SetLinkMaster(route.fd, first_index, bridge_index, 4511), 0);
    EXPECT_EQ(bridge_mtu(4512), 1400u);
    ASSERT_EQ(SetLinkMaster(route.fd, second_index, bridge_index, 4513), 0);
    EXPECT_EQ(bridge_mtu(4514), 1300u);
    ASSERT_EQ(SetLinkMaster(route.fd, second_index, 0, 4515), 0);
    EXPECT_EQ(bridge_mtu(4516), 1400u);

    // Linux dev_set_mtu_ext returns early for an unchanged value, so this
    // request must not disable future automatic adjustment.
    ASSERT_EQ(SetLinkMtu(route.fd, bridge_index, 1400, 4517), 0);
    ASSERT_EQ(SetLinkMtu(route.fd, first_index, 1250, 4518), 0);
    EXPECT_EQ(bridge_mtu(4519), 1250u);

    ASSERT_EQ(SetLinkMtu(route.fd, bridge_index, 1400, 4520), 0);
    ASSERT_EQ(SetLinkMtu(route.fd, first_index, 1200, 4521), 0);
    EXPECT_EQ(bridge_mtu(4522), 1400u);
    ASSERT_EQ(SetLinkMaster(route.fd, first_index, 0, 4523), 0);
    EXPECT_EQ(bridge_mtu(4524), 1400u);

    // Linux ETH_MAX_MTU applies to software Ethernet devices as well as
    // physical NICs. Validate both the RTNL bound and smoltcp's capability.
    ASSERT_EQ(SetLinkMtu(route.fd, first_index, 9000, 4525), 0);
    const auto jumbo_port = QueryLink(route.fd, first, 4526);
    ASSERT_TRUE(jumbo_port.has_value());
    EXPECT_EQ(jumbo_port->mtu, 9000u);
    ASSERT_EQ(SetLinkMtu(route.fd, bridge_index, 9000, 4527), 0);
    ASSERT_EQ(bridge_mtu(4528), 9000u);
    EXPECT_EQ(SetLinkMtu(route.fd, bridge_index, 65536, 4529), EINVAL);
}

TEST(RtnetlinkBridgeVethSemantics, BridgeCarrierChangesNotifyLinkSubscribers) {
    RouteFd route;
    RouteFd monitor(RTMGRP_LINK);
    ASSERT_GE(route.fd, 0);
    ASSERT_GE(monitor.fd, 0);
    const std::string bridge = UniqueName("cb");
    const std::string host = UniqueName("ch");
    const std::string peer = UniqueName("cp");
    CleanupLinks cleanup {route.fd, bridge, host};

    LinkRequest create_bridge(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4530);
    ASSERT_TRUE(create_bridge.name(bridge));
    AddKind(create_bridge, "bridge");
    const int result = SendAck(route.fd, create_bridge);
    if (result == EPERM) GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
    ASSERT_EQ(result, 0);

    std::vector<uint8_t> peer_payload(sizeof(ifinfomsg), 0);
    AppendAttr(peer_payload, IFLA_IFNAME, peer.c_str(), peer.size() + 1);
    std::vector<uint8_t> veth_data;
    AppendAttr(veth_data, VETH_INFO_PEER, peer_payload.data(), peer_payload.size());
    LinkRequest create_veth(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4531);
    ASSERT_TRUE(create_veth.name(host));
    AddKind(create_veth, "veth", veth_data);
    ASSERT_EQ(SendAck(route.fd, create_veth), 0);

    const uint32_t bridge_index = if_nametoindex(bridge.c_str());
    const uint32_t host_index = if_nametoindex(host.c_str());
    const uint32_t peer_index = if_nametoindex(peer.c_str());
    ASSERT_NE(bridge_index, 0u);
    ASSERT_NE(host_index, 0u);
    ASSERT_NE(peer_index, 0u);
    ASSERT_EQ(SetLinkUp(route.fd, bridge_index, true, 4532), 0);
    ASSERT_EQ(SetLinkUp(route.fd, host_index, true, 4533), 0);
    ASSERT_EQ(SetLinkUp(route.fd, peer_index, true, 4534), 0);
    ASSERT_EQ(SetLinkMaster(route.fd, host_index, bridge_index, 4535), 0);
    EXPECT_TRUE(SawBridgeCarrierEvent(monitor.fd, bridge_index, true));

    ASSERT_EQ(SetLinkUp(route.fd, peer_index, false, 4536), 0);
    EXPECT_TRUE(SawBridgeCarrierEvent(monitor.fd, bridge_index, false));
}

TEST(RtnetlinkBridgeVethSemantics, CreateAttachedUpVethNotifiesBridgeCarrier) {
    RouteFd route;
    RouteFd monitor(RTMGRP_LINK);
    ASSERT_GE(route.fd, 0);
    ASSERT_GE(monitor.fd, 0);
    const std::string bridge = UniqueName("nb");
    const std::string host = UniqueName("nh");
    const std::string peer = UniqueName("np");
    CleanupLinks cleanup {route.fd, bridge, host};

    LinkRequest create_bridge(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4540);
    ASSERT_TRUE(create_bridge.name(bridge));
    AddKind(create_bridge, "bridge");
    const int result = SendAck(route.fd, create_bridge);
    if (result == EPERM) GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
    ASSERT_EQ(result, 0);
    const uint32_t bridge_index = if_nametoindex(bridge.c_str());
    ASSERT_NE(bridge_index, 0u);
    ASSERT_EQ(SetLinkUp(route.fd, bridge_index, true, 4541), 0);

    ifinfomsg peer_info {};
    peer_info.ifi_family = AF_UNSPEC;
    peer_info.ifi_flags = IFF_UP;
    peer_info.ifi_change = IFF_UP;
    std::vector<uint8_t> peer_payload(sizeof(peer_info));
    std::memcpy(peer_payload.data(), &peer_info, sizeof(peer_info));
    AppendAttr(peer_payload, IFLA_IFNAME, peer.c_str(), peer.size() + 1);
    std::vector<uint8_t> veth_data;
    AppendAttr(veth_data, VETH_INFO_PEER, peer_payload.data(), peer_payload.size());
    LinkRequest create_veth(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4542);
    ASSERT_TRUE(create_veth.name(host));
    create_veth.body()->ifi_flags = IFF_UP;
    create_veth.body()->ifi_change = IFF_UP;
    ASSERT_TRUE(create_veth.add(IFLA_MASTER, &bridge_index, sizeof(bridge_index)));
    AddKind(create_veth, "veth", veth_data);
    ASSERT_EQ(SendAck(route.fd, create_veth), 0);
    EXPECT_TRUE(SawBridgeCarrierEvent(monitor.fd, bridge_index, true));
}

TEST(RtnetlinkBridgeVethSemantics, BridgeAllowsVlanHeaderBeyondMtu) {
    RouteFd route;
    ASSERT_GE(route.fd, 0);
    const std::string bridge = UniqueName("vb");
    CleanupLinks cleanup {route.fd, bridge, ""};
    LinkRequest create_bridge(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4543);
    ASSERT_TRUE(create_bridge.name(bridge));
    AddKind(create_bridge, "bridge");
    const int result = SendAck(route.fd, create_bridge);
    if (result == EPERM) GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
    ASSERT_EQ(result, 0);
    ASSERT_EQ(SetLinkUp(route.fd, if_nametoindex(bridge.c_str()), true, 4544), 0);
    const int packet = socket(AF_PACKET, SOCK_RAW, htons(ETH_P_ALL));
    if (packet < 0 && errno == EPERM) GTEST_SKIP() << "CAP_NET_RAW unavailable";
    ASSERT_GE(packet, 0);
    sockaddr_ll dest {};
    dest.sll_family = AF_PACKET;
    dest.sll_ifindex = if_nametoindex(bridge.c_str());
    dest.sll_halen = ETH_ALEN;
    std::vector<uint8_t> frame(1518, 0);
    std::memset(frame.data(), 0xff, ETH_ALEN);
    frame[12] = 0x81;
    frame[13] = 0x00;
    const ssize_t sent = sendto(packet, frame.data(), frame.size(), 0,
                                reinterpret_cast<sockaddr*>(&dest), sizeof(dest));
    EXPECT_EQ(sent, static_cast<ssize_t>(frame.size())) << errno;
    frame.push_back(0);
    EXPECT_EQ(sendto(packet, frame.data(), frame.size(), 0,
                     reinterpret_cast<sockaddr*>(&dest), sizeof(dest)), -1);
    EXPECT_EQ(errno, EMSGSIZE);
    frame.resize(1515);
    frame[12] = 0x08;
    frame[13] = 0x00;
    EXPECT_EQ(sendto(packet, frame.data(), frame.size(), 0,
                     reinterpret_cast<sockaddr*>(&dest), sizeof(dest)), -1);
    EXPECT_EQ(errno, EMSGSIZE);
    close(packet);
}

TEST(RtnetlinkBridgeVethSemantics, PeerNamespaceDependsOnPeerAttribute) {
    RouteFd route;
    ASSERT_GE(route.fd, 0);
    const std::string inherited_name = UniqueName("in");
    const std::string split_name = UniqueName("sp");
    const std::string split_peer = UniqueName("pp");
    const std::string explicit_outer = UniqueName("eo");
    const std::string explicit_peer = UniqueName("ep");

    int ready_pipe[2];
    int command_pipe[2];
    int result_pipe[2];
    ASSERT_EQ(pipe(ready_pipe), 0);
    ASSERT_EQ(pipe(command_pipe), 0);
    ASSERT_EQ(pipe(result_pipe), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(route.fd);
        close(ready_pipe[0]);
        close(command_pipe[1]);
        close(result_pipe[0]);
        const char ready = unshare(CLONE_NEWNET) == 0 ? 'R' : 'S';
        if (write(ready_pipe[1], &ready, 1) != 1 || ready != 'R') _exit(77);
        RouteFd child_route;
        if (child_route.fd < 0) _exit(78);

        char command;
        while (read(command_pipe[0], &command, 1) == 1) {
            bool correct = false;
            if (command == 'I') {
                const auto outer = QueryLink(child_route.fd, inherited_name, 4403);
                char diagnostic = outer.has_value() ? 'K' : 'O';
                if (diagnostic == 'K' && outer->kind != "veth") diagnostic = 'T';
                if (diagnostic == 'K' && outer->link == 0) diagnostic = 'L';
                if (diagnostic == 'K' && outer->link_netnsid != -1) diagnostic = 'D';
                if (diagnostic == 'K') {
                    const auto peer = QueryLinkByIndex(child_route.fd, outer->link, 4404);
                    if (!peer.has_value()) diagnostic = 'P';
                    else if (peer->kind != "veth" || peer->link != outer->index) diagnostic = 'R';
                }
                correct = diagnostic == 'K';
                if (!correct) {
                    if (write(result_pipe[1], &diagnostic, 1) != 1) _exit(80);
                    continue;
                }
            } else if (command == 'S') {
                const auto outer = QueryLink(child_route.fd, split_name, 4408);
                correct = outer.has_value() && outer->link_netnsid >= 0 &&
                          if_nametoindex(split_peer.c_str()) == 0;
            } else if (command == 'E') {
                correct = if_nametoindex(explicit_peer.c_str()) != 0 &&
                          if_nametoindex(explicit_outer.c_str()) == 0;
            } else {
                _exit(79);
            }
            const char result = correct ? 'Y' : 'N';
            if (write(result_pipe[1], &result, 1) != 1) _exit(80);
        }
        _exit(0);
    }

    close(ready_pipe[1]);
    close(command_pipe[0]);
    close(result_pipe[1]);
    char ready = 0;
    const ssize_t ready_count = read(ready_pipe[0], &ready, 1);
    close(ready_pipe[0]);
    if (ready_count != 1 || ready != 'R') {
        close(command_pipe[1]);
        close(result_pipe[0]);
        int status = 0;
        waitpid(child, &status, 0);
        if (ready_count == 1 && ready == 'S') {
            GTEST_SKIP() << "network namespace creation unavailable";
        }
        FAIL() << "namespace child failed before reporting readiness";
    }

    const uint32_t target_pid = static_cast<uint32_t>(child);
    LinkRequest inherited(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4401);
    ASSERT_TRUE(inherited.name(inherited_name));
    AddKind(inherited, "veth");
    ASSERT_TRUE(inherited.add(IFLA_NET_NS_PID, &target_pid, sizeof(target_pid)));
    const int inherited_result = SendAck(route.fd, inherited);
    if (inherited_result == EPERM) {
        close(command_pipe[1]);
        close(result_pipe[0]);
        int status = 0;
        waitpid(child, &status, 0);
        GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
    }
    EXPECT_EQ(inherited_result, 0);
    EXPECT_EQ(if_nametoindex(inherited_name.c_str()), 0u);
    const char inherited_command = 'I';
    EXPECT_EQ(write(command_pipe[1], &inherited_command, 1), 1);
    char inherited_placement = 0;
    EXPECT_EQ(read(result_pipe[0], &inherited_placement, 1), 1);
    EXPECT_EQ(inherited_placement, 'Y')
        << "without VETH_INFO_PEER, both endpoints must inherit outer netns";

    std::vector<uint8_t> peer_payload(sizeof(ifinfomsg), 0);
    AppendAttr(peer_payload, IFLA_IFNAME, split_peer.c_str(), split_peer.size() + 1);
    std::vector<uint8_t> veth_data;
    AppendAttr(veth_data, VETH_INFO_PEER, peer_payload.data(), peer_payload.size());
    LinkRequest split(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4402);
    ASSERT_TRUE(split.name(split_name));
    AddKind(split, "veth", veth_data);
    ASSERT_TRUE(split.add(IFLA_NET_NS_PID, &target_pid, sizeof(target_pid)));
    const int split_result = SendAck(route.fd, split);
    EXPECT_EQ(split_result, 0);
    EXPECT_NE(if_nametoindex(split_peer.c_str()), 0u)
        << "VETH_INFO_PEER without a netns attribute defaults to source netns";
    const auto split_host = QueryLink(route.fd, split_peer, 4409);
    ASSERT_TRUE(split_host.has_value());
    EXPECT_GE(split_host->link_netnsid, 0);
    const char split_command = 'S';
    EXPECT_EQ(write(command_pipe[1], &split_command, 1), 1);
    char split_placement = 0;
    EXPECT_EQ(read(result_pipe[0], &split_placement, 1), 1);
    EXPECT_EQ(split_placement, 'Y');

    if (split_result == 0) {
        EXPECT_EQ(DeleteByName(route.fd, split_peer, 4405), 0);
    }

    std::vector<uint8_t> explicit_peer_payload(sizeof(ifinfomsg), 0);
    AppendAttr(explicit_peer_payload, IFLA_IFNAME, explicit_peer.c_str(),
               explicit_peer.size() + 1);
    AppendAttr(explicit_peer_payload, IFLA_NET_NS_PID, &target_pid, sizeof(target_pid));
    std::vector<uint8_t> explicit_veth_data;
    AppendAttr(explicit_veth_data, VETH_INFO_PEER, explicit_peer_payload.data(),
               explicit_peer_payload.size());
    LinkRequest explicit_request(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4406);
    ASSERT_TRUE(explicit_request.name(explicit_outer));
    AddKind(explicit_request, "veth", explicit_veth_data);
    const int explicit_result = SendAck(route.fd, explicit_request);
    EXPECT_EQ(explicit_result, 0);
    EXPECT_NE(if_nametoindex(explicit_outer.c_str()), 0u);
    const auto explicit_host = QueryLink(route.fd, explicit_outer, 4410);
    ASSERT_TRUE(explicit_host.has_value());
    EXPECT_GE(explicit_host->link_netnsid, 0);
    EXPECT_EQ(explicit_host->link_netnsid, split_host->link_netnsid);
    const char explicit_command = 'E';
    EXPECT_EQ(write(command_pipe[1], &explicit_command, 1), 1);
    char explicit_placement = 0;
    EXPECT_EQ(read(result_pipe[0], &explicit_placement, 1), 1);
    EXPECT_EQ(explicit_placement, 'Y')
        << "VETH_INFO_PEER netns attribute overrides the source netns default";
    if (explicit_result == 0) {
        EXPECT_EQ(DeleteByName(route.fd, explicit_outer, 4407), 0);
    }

    close(command_pipe[1]);
    close(result_pipe[0]);
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

TEST(RtnetlinkBridgeVethSemantics, NamespaceExitNotifiesHostPeerDeletion) {
    RouteFd route;
    RouteFd monitor(RTMGRP_LINK);
    ASSERT_GE(route.fd, 0);
    ASSERT_GE(monitor.fd, 0);
    const std::string host_name = UniqueName("nh");
    const std::string peer_name = UniqueName("np");

    int ready_pipe[2], exit_pipe[2];
    ASSERT_EQ(pipe(ready_pipe), 0);
    ASSERT_EQ(pipe(exit_pipe), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready_pipe[0]);
        close(exit_pipe[1]);
        const char ready = unshare(CLONE_NEWNET) == 0 ? 'R' : 'S';
        if (write(ready_pipe[1], &ready, 1) != 1 || ready != 'R') _exit(77);
        char command;
        if (read(exit_pipe[0], &command, 1) != 1) _exit(78);
        _exit(0);
    }
    close(ready_pipe[1]);
    close(exit_pipe[0]);
    char ready = 0;
    const ssize_t ready_count = read(ready_pipe[0], &ready, 1);
    close(ready_pipe[0]);
    if (ready_count != 1 || ready != 'R') {
        close(exit_pipe[1]);
        int status = 0;
        waitpid(child, &status, 0);
        if (ready_count == 1 && ready == 'S') GTEST_SKIP() << "netns unavailable";
        FAIL() << "namespace child did not become ready";
    }

    std::vector<uint8_t> peer_payload(sizeof(ifinfomsg), 0);
    const uint32_t target_pid = static_cast<uint32_t>(child);
    AppendAttr(peer_payload, IFLA_IFNAME, peer_name.c_str(), peer_name.size() + 1);
    AppendAttr(peer_payload, IFLA_NET_NS_PID, &target_pid, sizeof(target_pid));
    std::vector<uint8_t> data;
    AppendAttr(data, VETH_INFO_PEER, peer_payload.data(), peer_payload.size());
    LinkRequest create(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4450);
    ASSERT_TRUE(create.name(host_name));
    AddKind(create, "veth", data);
    const int create_result = SendAck(route.fd, create);
    if (create_result != 0) {
        const char exit_command = 'X';
        EXPECT_EQ(write(exit_pipe[1], &exit_command, 1), 1);
        close(exit_pipe[1]);
        int status = 0;
        waitpid(child, &status, 0);
        if (create_result == EPERM) GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
        FAIL() << "cross-netns veth create failed: " << create_result;
    }
    const auto host = QueryLink(route.fd, host_name, 4451);
    ASSERT_TRUE(host.has_value());
    const char exit_command = 'X';
    ASSERT_EQ(write(exit_pipe[1], &exit_command, 1), 1);
    close(exit_pipe[1]);
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));

    bool got_delete = false;
    for (int attempt = 0; attempt < 8 && !got_delete; ++attempt) {
        std::array<uint8_t, 4096> reply {};
        const ssize_t count = recv(monitor.fd, reply.data(), reply.size(), 0);
        if (count < 0) continue;
        int remaining = static_cast<int>(count);
        for (auto* message = reinterpret_cast<nlmsghdr*>(reply.data());
             NLMSG_OK(message, remaining); message = NLMSG_NEXT(message, remaining)) {
            if (message->nlmsg_type != RTM_DELLINK ||
                message->nlmsg_len < NLMSG_LENGTH(sizeof(ifinfomsg))) continue;
            const auto* body = reinterpret_cast<const ifinfomsg*>(NLMSG_DATA(message));
            got_delete |= body->ifi_index == static_cast<int>(host->index);
        }
    }
    EXPECT_TRUE(got_delete) << "netns exit must multicast DELLINK for the live host peer";
    EXPECT_EQ(if_nametoindex(host_name.c_str()), 0u);
}

TEST(RtnetlinkBridgeVethSemantics, RejectsMalformedNestedAttributesAndInvalidNamespaceFd) {
    RouteFd route;
    ASSERT_GE(route.fd, 0);
    const std::string bad_name = UniqueName("bad");

    std::array<uint8_t, 4> invalid_nested {2, 0, 1, 0};
    LinkRequest malformed(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4201);
    ASSERT_TRUE(malformed.name(bad_name));
    ASSERT_TRUE(malformed.add(IFLA_LINKINFO, invalid_nested.data(), invalid_nested.size()));
    const int malformed_result = SendAck(route.fd, malformed);
    if (malformed_result == EPERM) GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
    // Linux's deprecated nested parser may discard the short child and then
    // report an unknown kind (EOPNOTSUPP); DragonOS rejects it at the length
    // boundary (EINVAL). In both cases no device may be created.
    EXPECT_TRUE(malformed_result == EINVAL || malformed_result == EOPNOTSUPP)
        << malformed_result;

    const std::string whitespace_name = UniqueName("w") + "\t";
    LinkRequest invalid_outer_name(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4208);
    ASSERT_TRUE(invalid_outer_name.name(whitespace_name));
    AddKind(invalid_outer_name, "bridge");
    EXPECT_EQ(SendAck(route.fd, invalid_outer_name), EINVAL);
    EXPECT_EQ(if_nametoindex(whitespace_name.c_str()), 0u);

    const std::string invalid_peer_name = UniqueName("p") + "\v";
    const std::string valid_outer_name = UniqueName("o");
    std::vector<uint8_t> invalid_peer_payload(sizeof(ifinfomsg), 0);
    AppendAttr(invalid_peer_payload, IFLA_IFNAME, invalid_peer_name.c_str(),
               invalid_peer_name.size() + 1);
    std::vector<uint8_t> invalid_veth_data;
    AppendAttr(invalid_veth_data, VETH_INFO_PEER, invalid_peer_payload.data(),
               invalid_peer_payload.size());
    LinkRequest invalid_peer(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4209);
    ASSERT_TRUE(invalid_peer.name(valid_outer_name));
    AddKind(invalid_peer, "veth", invalid_veth_data);
    EXPECT_EQ(SendAck(route.fd, invalid_peer), EINVAL);
    EXPECT_EQ(if_nametoindex(valid_outer_name.c_str()), 0u);

    LinkRequest bad_qlen(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4207);
    ASSERT_TRUE(bad_qlen.name(bad_name));
    AddKind(bad_qlen, "bridge");
    const uint16_t short_qlen = 0;
    ASSERT_TRUE(bad_qlen.add(IFLA_TXQLEN, &short_qlen, sizeof(short_qlen)));
    // Linux's non-strict NLA_U32 policy reports ERANGE for a short payload;
    // DragonOS currently rejects the malformed payload with EINVAL.
    const int short_qlen_result = SendAck(route.fd, bad_qlen);
    EXPECT_TRUE(short_qlen_result == EINVAL || short_qlen_result == ERANGE)
        << short_qlen_result;

    LinkRequest bad_fd(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4202);
    ASSERT_TRUE(bad_fd.name(bad_name));
    AddKind(bad_fd, "bridge");
    const uint32_t unavailable_fd = 0x7fffffff;
    ASSERT_TRUE(bad_fd.add(IFLA_NET_NS_FD, &unavailable_fd, sizeof(unavailable_fd)));
    EXPECT_EQ(SendAck(route.fd, bad_fd), EBADF);
    LinkRequest bad_pid(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4203);
    ASSERT_TRUE(bad_pid.name(bad_name));
    AddKind(bad_pid, "bridge");
    const uint32_t unavailable_pid = 0x7fffffff;
    ASSERT_TRUE(bad_pid.add(IFLA_NET_NS_PID, &unavailable_pid, sizeof(unavailable_pid)));
    EXPECT_EQ(SendAck(route.fd, bad_pid), ESRCH);
    LinkRequest ambiguous(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4204);
    ASSERT_TRUE(ambiguous.name(bad_name));
    AddKind(ambiguous, "bridge");
    ASSERT_TRUE(ambiguous.add(IFLA_NET_NS_PID, &unavailable_pid, sizeof(unavailable_pid)));
    ASSERT_TRUE(ambiguous.add(IFLA_NET_NS_FD, &unavailable_fd, sizeof(unavailable_fd)));
    EXPECT_EQ(SendAck(route.fd, ambiguous), EINVAL);
    EXPECT_EQ(if_nametoindex(bad_name.c_str()), 0u);

    const int ns_fd = open("/proc/self/ns/net", O_RDONLY | O_CLOEXEC);
    ASSERT_GE(ns_fd, 0);
    LinkRequest duplicate(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4205);
    ASSERT_TRUE(duplicate.name(bad_name));
    AddKind(duplicate, "bridge");
    ASSERT_TRUE(duplicate.add(IFLA_NET_NS_FD, &unavailable_fd, sizeof(unavailable_fd)));
    const uint32_t valid_fd = ns_fd;
    ASSERT_TRUE(duplicate.add(IFLA_NET_NS_FD, &valid_fd, sizeof(valid_fd)));
    EXPECT_EQ(SendAck(route.fd, duplicate), 0) << "last duplicate attribute must win";
    EXPECT_NE(if_nametoindex(bad_name.c_str()), 0u);
    EXPECT_EQ(DeleteByName(route.fd, bad_name, 4206), 0);
    close(ns_fd);
}

TEST(RtnetlinkBridgeVethSemantics, RawSocketRebindDoesNotPinExitedVethNamespace) {
    RouteFd route;
    ASSERT_GE(route.fd, 0);
    const std::string host_name = UniqueName("rh");
    const std::string peer_name = UniqueName("rp");
    CleanupLinks cleanup {route.fd, "", host_name};

    std::vector<uint8_t> peer_payload(sizeof(ifinfomsg), 0);
    AppendAttr(peer_payload, IFLA_IFNAME, peer_name.c_str(), peer_name.size() + 1);
    std::vector<uint8_t> veth_data;
    AppendAttr(veth_data, VETH_INFO_PEER, peer_payload.data(), peer_payload.size());
    LinkRequest create(RTM_NEWLINK, NLM_F_CREATE | NLM_F_EXCL, 4301);
    ASSERT_TRUE(create.name(host_name));
    AddKind(create, "veth", veth_data);
    const int create_result = SendAck(route.fd, create);
    if (create_result == EPERM) GTEST_SKIP() << "CAP_NET_ADMIN unavailable";
    ASSERT_EQ(create_result, 0);

    int ready_pipe[2];
    int go_pipe[2];
    ASSERT_EQ(pipe(ready_pipe), 0);
    ASSERT_EQ(pipe(go_pipe), 0);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready_pipe[0]);
        close(go_pipe[1]);
        const char ready = unshare(CLONE_NEWNET) == 0 ? 'R' : 'S';
        if (write(ready_pipe[1], &ready, 1) != 1 || ready != 'R') _exit(77);
        char go;
        if (read(go_pipe[0], &go, 1) != 1) _exit(78);

        RouteFd child_route;
        if (child_route.fd < 0) _exit(79);
        const unsigned int peer_index = if_nametoindex(peer_name.c_str());
        if (peer_index == 0) _exit(80);
        LinkRequest address(RTM_NEWADDR, NLM_F_CREATE | NLM_F_EXCL, 4303);
        address.header()->nlmsg_len = NLMSG_LENGTH(sizeof(ifaddrmsg));
        auto* info = reinterpret_cast<ifaddrmsg*>(NLMSG_DATA(address.header()));
        std::memset(info, 0, sizeof(*info));
        info->ifa_family = AF_INET;
        info->ifa_prefixlen = 24;
        info->ifa_index = peer_index;
        in_addr local {};
        if (inet_pton(AF_INET, "192.0.2.36", &local) != 1) _exit(81);
        if (!address.add(IFA_LOCAL, &local, sizeof(local)) ||
            !address.add(IFA_ADDRESS, &local, sizeof(local)) ||
            SendAck(child_route.fd, address) != 0) _exit(82);

        // RawSocket initially registers for wildcard receive on lo. Binding
        // to the veth address must remove that old lo registration as well as
        // its smoltcp handle; otherwise lo pins this socket and its netns.
        const int raw = socket(AF_INET, SOCK_RAW, IPPROTO_ICMP);
        if (raw < 0) _exit(83);
        sockaddr_in bind_addr {};
        bind_addr.sin_family = AF_INET;
        bind_addr.sin_addr = local;
        if (bind(raw, reinterpret_cast<sockaddr*>(&bind_addr), sizeof(bind_addr)) != 0) _exit(84);
        close(raw);
        _exit(0);
    }

    close(ready_pipe[1]);
    close(go_pipe[0]);
    char ready = 0;
    const ssize_t read_count = read(ready_pipe[0], &ready, 1);
    close(ready_pipe[0]);
    if (read_count != 1 || ready != 'R') {
        close(go_pipe[1]);
        int status;
        waitpid(child, &status, 0);
        if (read_count == 1 && ready == 'S') {
            GTEST_SKIP() << "network namespace creation unavailable";
        }
        FAIL() << "namespace child failed before reporting readiness";
    }

    LinkRequest move(RTM_NEWLINK, 0, 4302);
    move.body()->ifi_index = if_nametoindex(peer_name.c_str());
    const uint32_t target_pid = static_cast<uint32_t>(child);
    ASSERT_TRUE(move.add(IFLA_NET_NS_PID, &target_pid, sizeof(target_pid)));
    const int move_result = SendAck(route.fd, move);
    const char go = 'G';
    EXPECT_EQ(move_result, 0);
    if (move_result == 0) {
        EXPECT_EQ(write(go_pipe[1], &go, 1), 1);
    }
    close(go_pipe[1]);
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(WEXITSTATUS(status), 0);

    // The final namespace reference can be released via RCU and its veth
    // peer via deferred teardown; allow bounded time for both on DragonOS.
    for (int attempt = 0; attempt < 100 && if_nametoindex(host_name.c_str()) != 0; ++attempt) {
        usleep(100000);
    }
    EXPECT_EQ(if_nametoindex(host_name.c_str()), 0u)
        << "closed raw socket must not pin the child netns or its veth peer";
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
