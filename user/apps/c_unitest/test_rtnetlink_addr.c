#include "netlink_test_lib.h"

#include <arpa/inet.h>

struct addr_dump_ctx {
    int target_ifindex;
    int seen_primary;
    int seen_extra;
};

static int parse_addr_msg(struct nlmsghdr *nlh,
                          struct ifaddrmsg **ifa_out,
                          struct rtattr *tb[]) {
    struct ifaddrmsg *ifa;
    int attr_len;

    NL_TEST_ASSERT(nlh->nlmsg_type == RTM_NEWADDR, "unexpected nlmsg_type=%u", nlh->nlmsg_type);
    ifa = (struct ifaddrmsg *)NLMSG_DATA(nlh);
    attr_len = (int)(nlh->nlmsg_len - NLMSG_LENGTH(sizeof(*ifa)));
    nl_parse_rtattr(tb, IFA_MAX, IFA_RTA(ifa), attr_len);
    *ifa_out = ifa;
    return 0;
}

static int addr_dump_cb(struct nlmsghdr *nlh, void *ctx) {
    struct addr_dump_ctx *dump = (struct addr_dump_ctx *)ctx;
    struct ifaddrmsg *ifa;
    struct rtattr *tb[IFA_MAX + 1];
    struct in_addr addr;
    char text[INET_ADDRSTRLEN];

    if (parse_addr_msg(nlh, &ifa, tb) != 0) {
        return -1;
    }

    NL_TEST_ASSERT(ifa->ifa_family == AF_INET, "unexpected family=%u", ifa->ifa_family);
    NL_TEST_ASSERT((int)ifa->ifa_index == dump->target_ifindex,
                   "unexpected ifindex=%u expected=%d",
                   ifa->ifa_index,
                   dump->target_ifindex);
    NL_TEST_ASSERT(tb[IFA_LOCAL] != NULL, "IFA_LOCAL missing");

    memcpy(&addr, RTA_DATA(tb[IFA_LOCAL]), sizeof(addr));
    inet_ntop(AF_INET, &addr, text, sizeof(text));
    if (strcmp(text, "200.0.0.1") == 0) {
        dump->seen_primary = 1;
    } else if (strcmp(text, "198.18.10.1") == 0) {
        dump->seen_extra = 1;
    }

    return 0;
}

static int send_getaddr_dump(int fd, uint32_t seq, int ifindex, struct addr_dump_ctx *ctx) {
    struct {
        struct nlmsghdr nlh;
        struct ifaddrmsg ifa;
    } req;

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifa));
    req.nlh.nlmsg_type = RTM_GETADDR;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    req.nlh.nlmsg_seq = seq;
    req.ifa.ifa_family = AF_INET;
    req.ifa.ifa_index = ifindex;

    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_GETADDR dump send failed");
    NL_TEST_ASSERT(nl_recv_dump(fd, seq, addr_dump_cb, ctx) == 0,
                   "RTM_GETADDR dump recv failed");
    return 0;
}

static int send_addr_update(int fd, uint32_t seq, int msg_type, int ifindex, const char *ip) {
    struct {
        struct nlmsghdr nlh;
        struct ifaddrmsg ifa;
        char attrbuf[128];
    } req;
    struct in_addr addr;

    NL_TEST_ASSERT(inet_pton(AF_INET, ip, &addr) == 1, "inet_pton(%s) failed", ip);

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifa));
    req.nlh.nlmsg_type = msg_type;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ifa.ifa_family = AF_INET;
    req.ifa.ifa_prefixlen = 24;
    req.ifa.ifa_index = ifindex;

    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFA_LOCAL, &addr, sizeof(addr)) == 0,
                   "add IFA_LOCAL failed");
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFA_ADDRESS, &addr, sizeof(addr)) == 0,
                   "add IFA_ADDRESS failed");
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_NEWADDR/DELADDR send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0, "addr update ack failed");
    return 0;
}

int main(void) {
    int fd = -1;
    int ifindex = -1;
    struct addr_dump_ctx ctx;

    fd = nl_open_socket(NETLINK_ROUTE);
    NL_TEST_ASSERT(fd >= 0, "open NETLINK_ROUTE failed");
    NL_TEST_ASSERT(nl_lookup_ifindex(fd, "veth_a", &ifindex) == 0,
                   "lookup veth_a ifindex failed");

    memset(&ctx, 0, sizeof(ctx));
    ctx.target_ifindex = ifindex;
    NL_TEST_ASSERT(send_getaddr_dump(fd, 1, ifindex, &ctx) == 0,
                   "initial RTM_GETADDR dump failed");
    NL_TEST_ASSERT(ctx.seen_primary, "200.0.0.1 missing from filtered GETADDR");

    NL_TEST_ASSERT(send_addr_update(fd, 2, RTM_NEWADDR, ifindex, "198.18.10.1") == 0,
                   "RTM_NEWADDR failed");

    memset(&ctx, 0, sizeof(ctx));
    ctx.target_ifindex = ifindex;
    NL_TEST_ASSERT(send_getaddr_dump(fd, 3, ifindex, &ctx) == 0,
                   "post-add RTM_GETADDR dump failed");
    NL_TEST_ASSERT(ctx.seen_extra, "198.18.10.1 missing after RTM_NEWADDR");

    NL_TEST_ASSERT(send_addr_update(fd, 4, RTM_DELADDR, ifindex, "198.18.10.1") == 0,
                   "RTM_DELADDR failed");

    memset(&ctx, 0, sizeof(ctx));
    ctx.target_ifindex = ifindex;
    NL_TEST_ASSERT(send_getaddr_dump(fd, 5, ifindex, &ctx) == 0,
                   "post-del RTM_GETADDR dump failed");
    NL_TEST_ASSERT(!ctx.seen_extra, "198.18.10.1 still present after RTM_DELADDR");

    close(fd);
    printf("rtnetlink addr tests passed\n");
    return 0;
}
