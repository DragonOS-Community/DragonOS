#include "netlink_test_lib.h"

#include <arpa/inet.h>

struct route_dump_ctx {
    int target_ifindex;
    int seen_connected;
    int seen_static;
};

static int route_dump_cb(struct nlmsghdr *nlh, void *ctx) {
    struct route_dump_ctx *dump = (struct route_dump_ctx *)ctx;
    struct rtmsg *rtm;
    struct rtattr *tb[RTA_MAX + 1];
    int attr_len;
    uint32_t oif = 0;
    uint32_t priority = 0;
    struct in_addr dst = {0};
    struct in_addr gateway = {0};
    char dst_text[INET_ADDRSTRLEN];
    char gw_text[INET_ADDRSTRLEN];

    NL_TEST_ASSERT(nlh->nlmsg_type == RTM_NEWROUTE, "unexpected nlmsg_type=%u", nlh->nlmsg_type);
    rtm = (struct rtmsg *)NLMSG_DATA(nlh);
    NL_TEST_ASSERT(rtm->rtm_family == AF_INET, "unexpected route family=%u", rtm->rtm_family);

    attr_len = (int)(nlh->nlmsg_len - NLMSG_LENGTH(sizeof(*rtm)));
    nl_parse_rtattr(tb, RTA_MAX, RTM_RTA(rtm), attr_len);
    NL_TEST_ASSERT(tb[RTA_DST] != NULL, "RTA_DST missing");
    NL_TEST_ASSERT(tb[RTA_OIF] != NULL, "RTA_OIF missing");

    memcpy(&dst, RTA_DATA(tb[RTA_DST]), sizeof(dst));
    memcpy(&oif, RTA_DATA(tb[RTA_OIF]), sizeof(oif));
    inet_ntop(AF_INET, &dst, dst_text, sizeof(dst_text));
    if (tb[RTA_GATEWAY] != NULL) {
        memcpy(&gateway, RTA_DATA(tb[RTA_GATEWAY]), sizeof(gateway));
        inet_ntop(AF_INET, &gateway, gw_text, sizeof(gw_text));
    } else {
        gw_text[0] = '\0';
    }
    if (tb[RTA_PRIORITY] != NULL) {
        memcpy(&priority, RTA_DATA(tb[RTA_PRIORITY]), sizeof(priority));
    }

    if ((int)oif == dump->target_ifindex &&
        strcmp(dst_text, "200.0.0.0") == 0 &&
        rtm->rtm_dst_len == 24) {
        dump->seen_connected = 1;
    }

    if ((int)oif == dump->target_ifindex &&
        strcmp(dst_text, "198.18.0.0") == 0 &&
        rtm->rtm_dst_len == 16 &&
        strcmp(gw_text, "200.0.0.2") == 0 &&
        priority == 123) {
        dump->seen_static = 1;
    }

    return 0;
}

static int send_getroute_dump(int fd, uint32_t seq, struct route_dump_ctx *ctx) {
    struct {
        struct nlmsghdr nlh;
        struct rtmsg rtm;
    } req;

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.rtm));
    req.nlh.nlmsg_type = RTM_GETROUTE;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    req.nlh.nlmsg_seq = seq;
    req.rtm.rtm_family = AF_INET;

    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_GETROUTE dump send failed");
    NL_TEST_ASSERT(nl_recv_dump(fd, seq, route_dump_cb, ctx) == 0,
                   "RTM_GETROUTE dump recv failed");
    return 0;
}

static int send_route_del_no_gateway(int fd, uint32_t seq, int ifindex) {
    struct {
        struct nlmsghdr nlh;
        struct rtmsg rtm;
        char attrbuf[128];
    } req;
    struct in_addr dst;
    uint32_t oif = (uint32_t)ifindex;

    NL_TEST_ASSERT(inet_pton(AF_INET, "198.18.0.0", &dst) == 1, "inet_pton(dst) failed");

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.rtm));
    req.nlh.nlmsg_type = RTM_DELROUTE;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.rtm.rtm_family = AF_INET;
    req.rtm.rtm_dst_len = 16;
    req.rtm.rtm_table = RT_TABLE_MAIN;

    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), RTA_DST, &dst, sizeof(dst)) == 0,
                   "add RTA_DST failed");
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), RTA_OIF, &oif, sizeof(oif)) == 0,
                   "add RTA_OIF failed");
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_DELROUTE send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0, "RTM_DELROUTE ack failed");
    return 0;
}

static int send_route_update(int fd, uint32_t seq, int msg_type, int ifindex) {
    struct {
        struct nlmsghdr nlh;
        struct rtmsg rtm;
        char attrbuf[128];
    } req;
    struct in_addr dst;
    struct in_addr gateway;
    uint32_t oif = (uint32_t)ifindex;
    uint32_t priority = 123;

    NL_TEST_ASSERT(inet_pton(AF_INET, "198.18.0.0", &dst) == 1, "inet_pton(dst) failed");
    NL_TEST_ASSERT(inet_pton(AF_INET, "200.0.0.2", &gateway) == 1, "inet_pton(gateway) failed");

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.rtm));
    req.nlh.nlmsg_type = msg_type;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.rtm.rtm_family = AF_INET;
    req.rtm.rtm_dst_len = 16;
    req.rtm.rtm_table = RT_TABLE_MAIN;
    req.rtm.rtm_protocol = RTPROT_BOOT;
    req.rtm.rtm_scope = RT_SCOPE_UNIVERSE;
    req.rtm.rtm_type = RTN_UNICAST;

    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), RTA_DST, &dst, sizeof(dst)) == 0,
                   "add RTA_DST failed");
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), RTA_GATEWAY, &gateway, sizeof(gateway)) == 0,
                   "add RTA_GATEWAY failed");
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), RTA_OIF, &oif, sizeof(oif)) == 0,
                   "add RTA_OIF failed");
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), RTA_PRIORITY, &priority, sizeof(priority)) == 0,
                   "add RTA_PRIORITY failed");
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_NEWROUTE/DELROUTE send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0, "route update ack failed");
    return 0;
}

int main(void) {
    int fd = -1;
    int ifindex = -1;
    struct route_dump_ctx ctx;

    fd = nl_open_socket(NETLINK_ROUTE);
    NL_TEST_ASSERT(fd >= 0, "open NETLINK_ROUTE failed");
    NL_TEST_ASSERT(nl_lookup_ifindex(fd, "veth_a", &ifindex) == 0,
                   "lookup veth_a ifindex failed");

    memset(&ctx, 0, sizeof(ctx));
    ctx.target_ifindex = ifindex;
    NL_TEST_ASSERT(send_getroute_dump(fd, 1, &ctx) == 0, "initial RTM_GETROUTE failed");
    NL_TEST_ASSERT(ctx.seen_connected,
                   "connected route 200.0.0.0/24 missing or not normalized");

    NL_TEST_ASSERT(send_route_update(fd, 2, RTM_NEWROUTE, ifindex) == 0,
                   "RTM_NEWROUTE failed");
    memset(&ctx, 0, sizeof(ctx));
    ctx.target_ifindex = ifindex;
    NL_TEST_ASSERT(send_getroute_dump(fd, 3, &ctx) == 0, "post-add RTM_GETROUTE failed");
    NL_TEST_ASSERT(ctx.seen_static, "static route missing after RTM_NEWROUTE");

    NL_TEST_ASSERT(send_route_del_no_gateway(fd, 4, ifindex) == 0,
                   "RTM_DELROUTE without gateway failed");
    memset(&ctx, 0, sizeof(ctx));
    ctx.target_ifindex = ifindex;
    NL_TEST_ASSERT(send_getroute_dump(fd, 5, &ctx) == 0, "post-del RTM_GETROUTE failed");
    NL_TEST_ASSERT(!ctx.seen_static, "static route still present after RTM_DELROUTE");

    close(fd);
    printf("rtnetlink route tests passed\n");
    return 0;
}
