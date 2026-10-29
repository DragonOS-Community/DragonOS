#include "netlink_test_lib.h"

#include <arpa/inet.h>

#ifndef NDA_RTA
#define NDA_RTA(r)                                                              \
    ((struct rtattr *)(((char *)(r)) + NLMSG_ALIGN(sizeof(struct ndmsg))))
#endif

struct neigh_dump_ctx {
    int target_ifindex;
    int seen_entry;
};

static int neigh_dump_cb(struct nlmsghdr *nlh, void *ctx) {
    struct neigh_dump_ctx *dump = (struct neigh_dump_ctx *)ctx;
    struct ndmsg *ndm;
    struct rtattr *tb[NDA_MAX + 1];
    int attr_len;
    struct in_addr dst;
    char dst_text[INET_ADDRSTRLEN];

    NL_TEST_ASSERT(nlh->nlmsg_type == RTM_NEWNEIGH, "unexpected nlmsg_type=%u", nlh->nlmsg_type);
    ndm = (struct ndmsg *)NLMSG_DATA(nlh);
    NL_TEST_ASSERT(ndm->ndm_family == AF_INET, "unexpected neigh family=%u", ndm->ndm_family);
    NL_TEST_ASSERT(ndm->ndm_ifindex == dump->target_ifindex,
                   "unexpected neigh ifindex=%d expected=%d",
                   ndm->ndm_ifindex,
                   dump->target_ifindex);

    attr_len = (int)(nlh->nlmsg_len - NLMSG_LENGTH(sizeof(*ndm)));
    nl_parse_rtattr(tb, NDA_MAX, NDA_RTA(ndm), attr_len);
    NL_TEST_ASSERT(tb[NDA_DST] != NULL, "NDA_DST missing");

    memcpy(&dst, RTA_DATA(tb[NDA_DST]), sizeof(dst));
    inet_ntop(AF_INET, &dst, dst_text, sizeof(dst_text));
    if (strcmp(dst_text, "198.18.0.254") == 0) {
        NL_TEST_ASSERT(tb[NDA_LLADDR] != NULL, "NDA_LLADDR missing");
        NL_TEST_ASSERT(RTA_PAYLOAD(tb[NDA_LLADDR]) == 6,
                       "unexpected lladdr len=%lu",
                       (unsigned long)RTA_PAYLOAD(tb[NDA_LLADDR]));
        dump->seen_entry = 1;
    }

    return 0;
}

static int send_getneigh_dump(int fd, uint32_t seq, int ifindex, struct neigh_dump_ctx *ctx) {
    struct {
        struct nlmsghdr nlh;
        struct ndmsg ndm;
    } req;

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ndm));
    req.nlh.nlmsg_type = RTM_GETNEIGH;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    req.nlh.nlmsg_seq = seq;
    req.ndm.ndm_family = AF_INET;
    req.ndm.ndm_ifindex = ifindex;

    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_GETNEIGH dump send failed");
    NL_TEST_ASSERT(nl_recv_dump(fd, seq, neigh_dump_cb, ctx) == 0,
                   "RTM_GETNEIGH dump recv failed");
    return 0;
}

static int send_neigh_update(int fd, uint32_t seq, int msg_type, int ifindex) {
    struct {
        struct nlmsghdr nlh;
        struct ndmsg ndm;
        char attrbuf[128];
    } req;
    struct in_addr dst;
    unsigned char lladdr[6] = {0x02, 0x00, 0x00, 0x00, 0x00, 0x44};

    NL_TEST_ASSERT(inet_pton(AF_INET, "198.18.0.254", &dst) == 1, "inet_pton(dst) failed");

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ndm));
    req.nlh.nlmsg_type = msg_type;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ndm.ndm_family = AF_INET;
    req.ndm.ndm_ifindex = ifindex;
    req.ndm.ndm_state = NUD_PERMANENT;

    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), NDA_DST, &dst, sizeof(dst)) == 0,
                   "add NDA_DST failed");
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), NDA_LLADDR, lladdr, sizeof(lladdr)) == 0,
                   "add NDA_LLADDR failed");
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_NEWNEIGH/DELNEIGH send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0, "neigh update ack failed");
    return 0;
}

int main(void) {
    int fd = -1;
    int ifindex = -1;
    struct neigh_dump_ctx ctx;

    fd = nl_open_socket(NETLINK_ROUTE);
    NL_TEST_ASSERT(fd >= 0, "open NETLINK_ROUTE failed");
    NL_TEST_ASSERT(nl_lookup_ifindex(fd, "veth_a", &ifindex) == 0,
                   "lookup veth_a ifindex failed");

    NL_TEST_ASSERT(send_neigh_update(fd, 1, RTM_NEWNEIGH, ifindex) == 0,
                   "RTM_NEWNEIGH failed");
    memset(&ctx, 0, sizeof(ctx));
    ctx.target_ifindex = ifindex;
    NL_TEST_ASSERT(send_getneigh_dump(fd, 2, ifindex, &ctx) == 0,
                   "post-add RTM_GETNEIGH failed");
    NL_TEST_ASSERT(ctx.seen_entry, "neigh entry missing after RTM_NEWNEIGH");

    NL_TEST_ASSERT(send_neigh_update(fd, 3, RTM_DELNEIGH, ifindex) == 0,
                   "RTM_DELNEIGH failed");
    memset(&ctx, 0, sizeof(ctx));
    ctx.target_ifindex = ifindex;
    NL_TEST_ASSERT(send_getneigh_dump(fd, 4, ifindex, &ctx) == 0,
                   "post-del RTM_GETNEIGH failed");
    NL_TEST_ASSERT(!ctx.seen_entry, "neigh entry still present after RTM_DELNEIGH");

    close(fd);
    printf("rtnetlink neigh tests passed\n");
    return 0;
}
