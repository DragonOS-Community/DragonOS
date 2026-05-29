#include "netlink_test_lib.h"

#include <net/if.h>

struct link_dump_ctx {
    int seen_veth_a;
    int seen_veth_b;
    int count;
};

static int link_dump_cb(struct nlmsghdr *nlh, void *ctx) {
    struct link_dump_ctx *dump = (struct link_dump_ctx *)ctx;
    struct nl_link_info info;

    if (nl_parse_link_info(nlh, &info) < 0) {
        perror("nl_parse_link_info failed");
        return -1;
    }

    NL_TEST_ASSERT(info.name[0] != '\0', "link name is empty");
    NL_TEST_ASSERT(info.addr_len == 6, "link %s addr len=%zu", info.name, info.addr_len);
    NL_TEST_ASSERT(info.mtu > 0, "link %s mtu=%u", info.name, info.mtu);

    if (strcmp(info.name, "veth_a") == 0) {
        dump->seen_veth_a = 1;
    } else if (strcmp(info.name, "veth_b") == 0) {
        dump->seen_veth_b = 1;
    }
    dump->count++;
    return 0;
}

static int test_getlink_dump(int fd) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
    } req;
    struct link_dump_ctx ctx;

    memset(&req, 0, sizeof(req));
    memset(&ctx, 0, sizeof(ctx));

    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_GETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    req.nlh.nlmsg_seq = 1;
    req.ifi.ifi_family = AF_UNSPEC;

    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_GETLINK dump send failed");
    NL_TEST_ASSERT(nl_recv_dump(fd, 1, link_dump_cb, &ctx) == 0,
                   "RTM_GETLINK dump recv failed");
    NL_TEST_ASSERT(ctx.count >= 4, "unexpected link count=%d", ctx.count);
    NL_TEST_ASSERT(ctx.seen_veth_a, "veth_a missing from RTM_GETLINK dump");
    NL_TEST_ASSERT(ctx.seen_veth_b, "veth_b missing from RTM_GETLINK dump");
    return 0;
}

static int setlink_name(int fd, uint32_t seq, int ifindex, const char *name, int expect_errno) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
        char attrbuf[128];
    } req;

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_SETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;

    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFLA_IFNAME, name, strlen(name) + 1) == 0,
                   "add IFLA_IFNAME failed");
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_SETLINK(name) send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, expect_errno) == 0,
                   "RTM_SETLINK(name) ack mismatch");
    return 0;
}

static int setlink_mtu(int fd, uint32_t seq, int ifindex, unsigned int mtu) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
        char attrbuf[128];
    } req;

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_SETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;

    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFLA_MTU, &mtu, sizeof(mtu)) == 0,
                   "add IFLA_MTU failed");
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_SETLINK(mtu) send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0, "RTM_SETLINK(mtu) ack failed");
    return 0;
}

static int setlink_up(int fd, uint32_t seq, int ifindex, int up) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
    } req;

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_SETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;
    req.ifi.ifi_change = IFF_UP;
    req.ifi.ifi_flags = up ? IFF_UP : 0;

    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_SETLINK(flags) send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0, "RTM_SETLINK(flags) ack failed");
    return 0;
}

int main(void) {
    int fd = -1;
    int ifindex = -1;
    struct nl_link_info by_name;
    struct nl_link_info by_index;
    unsigned int original_mtu;

    fd = nl_open_socket(NETLINK_ROUTE);
    NL_TEST_ASSERT(fd >= 0, "open NETLINK_ROUTE failed");

    NL_TEST_ASSERT(test_getlink_dump(fd) == 0, "test_getlink_dump failed");

    NL_TEST_ASSERT(nl_get_link_by_name(fd, 2, "veth_a", &by_name) == 0,
                   "GETLINK by name veth_a failed");
    ifindex = by_name.ifindex;
    original_mtu = by_name.mtu;

    NL_TEST_ASSERT(nl_get_link_by_index(fd, 3, ifindex, &by_index) == 0,
                   "GETLINK by index failed");
    NL_TEST_ASSERT(strcmp(by_index.name, "veth_a") == 0,
                   "GETLINK by index returned %s", by_index.name);

    NL_TEST_ASSERT(setlink_name(fd, 4, ifindex, "veth_b", EEXIST) == 0,
                   "duplicate rename check failed");

    NL_TEST_ASSERT(setlink_name(fd, 5, ifindex, "veth_a_rt", 0) == 0,
                   "rename to veth_a_rt failed");
    NL_TEST_ASSERT(nl_get_link_by_name(fd, 6, "veth_a_rt", &by_name) == 0,
                   "GETLINK renamed iface failed");
    NL_TEST_ASSERT(by_name.ifindex == ifindex,
                   "ifindex changed after rename: %d -> %d", ifindex, by_name.ifindex);
    NL_TEST_ASSERT(setlink_name(fd, 7, ifindex, "veth_a", 0) == 0,
                   "rename back to veth_a failed");

    NL_TEST_ASSERT(setlink_mtu(fd, 8, ifindex, original_mtu + 128) == 0,
                   "set mtu failed");
    NL_TEST_ASSERT(nl_get_link_by_index(fd, 9, ifindex, &by_index) == 0,
                   "GETLINK after mtu change failed");
    NL_TEST_ASSERT(by_index.mtu == original_mtu + 128,
                   "mtu mismatch after update: %u", by_index.mtu);
    NL_TEST_ASSERT(setlink_mtu(fd, 10, ifindex, original_mtu) == 0,
                   "restore mtu failed");

    NL_TEST_ASSERT(setlink_up(fd, 11, ifindex, 0) == 0, "set link down failed");
    NL_TEST_ASSERT(nl_get_link_by_index(fd, 12, ifindex, &by_index) == 0,
                   "GETLINK after down failed");
    NL_TEST_ASSERT((by_index.flags & IFF_UP) == 0, "IFF_UP still set after down");
    NL_TEST_ASSERT(setlink_up(fd, 13, ifindex, 1) == 0, "set link up failed");
    NL_TEST_ASSERT(nl_get_link_by_index(fd, 14, ifindex, &by_index) == 0,
                   "GETLINK after up failed");
    NL_TEST_ASSERT((by_index.flags & IFF_UP) != 0, "IFF_UP missing after up");

    close(fd);
    printf("rtnetlink link tests passed\n");
    return 0;
}
