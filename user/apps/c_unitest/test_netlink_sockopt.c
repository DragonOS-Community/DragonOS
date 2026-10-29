#include "netlink_test_lib.h"

#include <arpa/inet.h>
#include <sys/epoll.h>

struct dump_seen_ctx {
    int seen;
};

static int verify_empty_list_memberships_linux_abi(int fd) {
    socklen_t optlen;
    uint32_t groups[4];
    unsigned char short_buf[2];

    optlen = 0;
    NL_TEST_ASSERT(getsockopt(fd,
                              SOL_NETLINK,
                              NETLINK_LIST_MEMBERSHIPS,
                              NULL,
                              &optlen) == 0,
                   "getsockopt(NULL, LIST_MEMBERSHIPS) failed: %s", strerror(errno));
    NL_TEST_ASSERT(optlen == 0,
                   "unexpected LIST_MEMBERSHIPS required size=%u",
                   (unsigned int)optlen);

    memset(groups, 0xa5, sizeof(groups));
    optlen = sizeof(groups);
    NL_TEST_ASSERT(getsockopt(fd,
                              SOL_NETLINK,
                              NETLINK_LIST_MEMBERSHIPS,
                              groups,
                              &optlen) == 0,
                   "LIST_MEMBERSHIPS array read failed: %s", strerror(errno));
    NL_TEST_ASSERT(optlen == 0,
                   "LIST_MEMBERSHIPS optlen should report required size, got=%u",
                   (unsigned int)optlen);
    NL_TEST_ASSERT(groups[0] == 0xa5a5a5a5u && groups[1] == 0xa5a5a5a5u,
                   "empty LIST_MEMBERSHIPS should not modify user buffer");

    memset(short_buf, 0xa5, sizeof(short_buf));
    optlen = sizeof(short_buf);
    NL_TEST_ASSERT(getsockopt(fd,
                              SOL_NETLINK,
                              NETLINK_LIST_MEMBERSHIPS,
                              short_buf,
                              &optlen) == 0,
                   "LIST_MEMBERSHIPS short read failed: %s", strerror(errno));
    NL_TEST_ASSERT(optlen == 0,
                   "short LIST_MEMBERSHIPS should still report required size, got=%u",
                   (unsigned int)optlen);
    NL_TEST_ASSERT(short_buf[0] == 0xa5 && short_buf[1] == 0xa5,
                   "empty LIST_MEMBERSHIPS short buffer should stay untouched");
    return 0;
}

static int verify_list_memberships_linux_abi(int fd,
                                             uint32_t expected_mask_low,
                                             uint32_t expected_mask_high,
                                             socklen_t expected_len) {
    socklen_t optlen;
    uint32_t groups[4];
    uint32_t one_word = 0xaaaaaaaa;
    unsigned char short_buf[2];

    optlen = 0;
    NL_TEST_ASSERT(getsockopt(fd,
                              SOL_NETLINK,
                              NETLINK_LIST_MEMBERSHIPS,
                              NULL,
                              &optlen) == 0,
                   "getsockopt(NULL, LIST_MEMBERSHIPS) failed: %s", strerror(errno));
    NL_TEST_ASSERT(optlen == expected_len,
                   "unexpected LIST_MEMBERSHIPS required size=%u expected=%u",
                   (unsigned int)optlen,
                   (unsigned int)expected_len);

    memset(groups, 0xa5, sizeof(groups));
    optlen = sizeof(groups);
    NL_TEST_ASSERT(getsockopt(fd,
                              SOL_NETLINK,
                              NETLINK_LIST_MEMBERSHIPS,
                              groups,
                              &optlen) == 0,
                   "LIST_MEMBERSHIPS array read failed: %s", strerror(errno));
    NL_TEST_ASSERT(optlen == expected_len,
                   "LIST_MEMBERSHIPS optlen should report required size, got=%u",
                   (unsigned int)optlen);
    NL_TEST_ASSERT(groups[0] == expected_mask_low,
                   "unexpected groups[0]=0x%x expected=0x%x",
                   groups[0],
                   expected_mask_low);
    if (expected_len >= 2 * sizeof(uint32_t)) {
        NL_TEST_ASSERT(groups[1] == expected_mask_high,
                       "unexpected groups[1]=0x%x expected=0x%x",
                       groups[1],
                       expected_mask_high);
    }

    one_word = 0xaaaaaaaa;
    optlen = sizeof(one_word);
    NL_TEST_ASSERT(getsockopt(fd,
                              SOL_NETLINK,
                              NETLINK_LIST_MEMBERSHIPS,
                              &one_word,
                              &optlen) == 0,
                   "LIST_MEMBERSHIPS single-word read failed: %s", strerror(errno));
    NL_TEST_ASSERT(optlen == expected_len,
                   "single-word LIST_MEMBERSHIPS should still report required size, got=%u",
                   (unsigned int)optlen);
    NL_TEST_ASSERT(one_word == expected_mask_low,
                   "single-word LIST_MEMBERSHIPS mismatch: got=0x%x expected=0x%x",
                   one_word,
                   expected_mask_low);

    memset(short_buf, 0xa5, sizeof(short_buf));
    optlen = sizeof(short_buf);
    NL_TEST_ASSERT(getsockopt(fd,
                              SOL_NETLINK,
                              NETLINK_LIST_MEMBERSHIPS,
                              short_buf,
                              &optlen) == 0,
                   "LIST_MEMBERSHIPS short read failed: %s", strerror(errno));
    NL_TEST_ASSERT(optlen == expected_len,
                   "short LIST_MEMBERSHIPS should still report required size, got=%u",
                   (unsigned int)optlen);
    NL_TEST_ASSERT(short_buf[0] == 0xa5 && short_buf[1] == 0xa5,
                   "short LIST_MEMBERSHIPS should not copy partial u32 values");
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

    if (nl_addattr_l(&req.nlh, sizeof(req), IFLA_MTU, &mtu, sizeof(mtu)) < 0) {
        return -errno;
    }
    if (nl_send_request(fd, &req, req.nlh.nlmsg_len) < 0) {
        return -errno;
    }
    if (nl_recv_ack(fd, seq, 0) < 0) {
        return -errno;
    }
    return 0;
}

static int dump_seen_cb(struct nlmsghdr *nlh, void *arg) {
    struct nl_link_info info;
    struct dump_seen_ctx *state = (struct dump_seen_ctx *)arg;

    if (nl_parse_link_info(nlh, &info) < 0) {
        return -1;
    }
    state->seen = 1;
    return 0;
}

static int verify_epoll_for_dump(int fd) {
    int epfd;
    struct epoll_event ev;
    struct epoll_event out;
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
    } req;
    struct dump_seen_ctx ctx = {0};

    epfd = epoll_create1(0);
    NL_TEST_ASSERT(epfd >= 0, "epoll_create1 failed: %s", strerror(errno));

    memset(&ev, 0, sizeof(ev));
    ev.events = EPOLLIN;
    ev.data.fd = fd;
    NL_TEST_ASSERT(epoll_ctl(epfd, EPOLL_CTL_ADD, fd, &ev) == 0,
                   "epoll_ctl add route fd failed: %s", strerror(errno));

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_GETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    req.nlh.nlmsg_seq = 1;
    req.ifi.ifi_family = AF_UNSPEC;

    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_GETLINK dump send failed");
    NL_TEST_ASSERT(epoll_wait(epfd, &out, 1, 1000) == 1, "epoll_wait dump timeout");
    NL_TEST_ASSERT((out.events & EPOLLIN) != 0, "epoll dump event missing EPOLLIN");
    NL_TEST_ASSERT(nl_recv_dump(fd, 1, dump_seen_cb, &ctx) == 0,
                   "recv dump after epoll failed");
    NL_TEST_ASSERT(ctx.seen, "no RTM_NEWLINK message observed after epoll wake");

    close(epfd);
    return 0;
}

static int verify_memberships_and_notify(int control_fd, int ifindex, unsigned int original_mtu) {
    int notify_fd;
    int epfd;
    unsigned int group_id = RTNLGRP_LINK;
    struct epoll_event ev;
    struct epoll_event out;
    char buf[NL_TEST_BUF_SIZE];
    ssize_t len;
    struct nlmsghdr *nlh;
    int ret;
    int saw_notify = 0;

    notify_fd = nl_open_socket(NETLINK_ROUTE);
    NL_TEST_ASSERT(notify_fd >= 0, "open notify NETLINK_ROUTE failed");
    NL_TEST_ASSERT(verify_empty_list_memberships_linux_abi(notify_fd) == 0,
                   "empty LIST_MEMBERSHIPS ABI check failed");

    NL_TEST_ASSERT(setsockopt(notify_fd,
                              SOL_NETLINK,
                              NETLINK_ADD_MEMBERSHIP,
                              &group_id,
                              sizeof(group_id)) == 0,
                   "NETLINK_ADD_MEMBERSHIP failed: %s", strerror(errno));
    NL_TEST_ASSERT(verify_list_memberships_linux_abi(
                       notify_fd, 1u << (RTNLGRP_LINK - 1), 0, 2 * sizeof(uint32_t))
                       == 0,
                   "post-add LIST_MEMBERSHIPS ABI check failed");

    epfd = epoll_create1(0);
    NL_TEST_ASSERT(epfd >= 0, "epoll_create1 failed: %s", strerror(errno));
    memset(&ev, 0, sizeof(ev));
    ev.events = EPOLLIN;
    ev.data.fd = notify_fd;
    NL_TEST_ASSERT(epoll_ctl(epfd, EPOLL_CTL_ADD, notify_fd, &ev) == 0,
                   "epoll_ctl add notify fd failed: %s", strerror(errno));

    ret = setlink_mtu(control_fd, 2, ifindex, original_mtu + 64);
    if (ret < 0) {
        if (-ret == EPERM || -ret == EACCES) {
            fprintf(stderr,
                    "SKIP: RTM_SETLINK requires CAP_NET_ADMIN on this Linux environment\n");
            close(epfd);
            close(notify_fd);
            return 0;
        }
        NL_TEST_ASSERT(0, "trigger RTM_NEWLINK notify failed: %s", strerror(-ret));
    }
    NL_TEST_ASSERT(epoll_wait(epfd, &out, 1, 1000) == 1, "epoll_wait notify timeout");
    NL_TEST_ASSERT((out.events & EPOLLIN) != 0, "notify event missing EPOLLIN");

    len = recv(notify_fd, buf, sizeof(buf), 0);
    NL_TEST_ASSERT(len > 0, "recv notify failed: %s", strerror(errno));
    for (nlh = (struct nlmsghdr *)buf; NLMSG_OK(nlh, len); nlh = NLMSG_NEXT(nlh, len)) {
        struct nl_link_info info;
        if (nlh->nlmsg_type != RTM_NEWLINK) {
            continue;
        }
        if (nl_parse_link_info(nlh, &info) < 0) {
            perror("nl_parse_link_info notify failed");
            return 1;
        }
        if (info.ifindex == ifindex && info.mtu == original_mtu + 64) {
            saw_notify = 1;
            break;
        }
    }
    NL_TEST_ASSERT(saw_notify, "did not observe expected RTM_NEWLINK notify");

    NL_TEST_ASSERT(setlink_mtu(control_fd, 3, ifindex, original_mtu) == 0,
                   "restore mtu after notify test failed");

    NL_TEST_ASSERT(setsockopt(notify_fd,
                              SOL_NETLINK,
                              NETLINK_DROP_MEMBERSHIP,
                              &group_id,
                              sizeof(group_id)) == 0,
                   "NETLINK_DROP_MEMBERSHIP failed: %s", strerror(errno));
    NL_TEST_ASSERT(verify_list_memberships_linux_abi(notify_fd, 0, 0, 2 * sizeof(uint32_t))
                       == 0,
                   "post-drop LIST_MEMBERSHIPS ABI check failed");

    close(epfd);
    close(notify_fd);
    return 0;
}

static int verify_bind_groups_linux_abi(void) {
    int fd;
    struct sockaddr_nl addr;

    fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    NL_TEST_ASSERT(fd >= 0, "socket(AF_NETLINK, NETLINK_ROUTE) failed: %s", strerror(errno));

    memset(&addr, 0, sizeof(addr));
    addr.nl_family = AF_NETLINK;
    addr.nl_groups = RTMGRP_LINK;
    NL_TEST_ASSERT(bind(fd, (struct sockaddr *)&addr, sizeof(addr)) == 0,
                   "bind(nl_groups=RTMGRP_LINK) failed: %s", strerror(errno));
    NL_TEST_ASSERT(verify_list_memberships_linux_abi(fd, RTMGRP_LINK, 0, 2 * sizeof(uint32_t))
                       == 0,
                   "bind-path LIST_MEMBERSHIPS ABI check failed");

    close(fd);
    return 0;
}

int main(void) {
    int control_fd = -1;
    struct nl_link_info info;

    control_fd = nl_open_socket(NETLINK_ROUTE);
    NL_TEST_ASSERT(control_fd >= 0, "open control NETLINK_ROUTE failed");
    if (nl_get_link_by_name(control_fd, 10, "lo", &info) < 0) {
        NL_TEST_ASSERT(nl_get_link_by_name(control_fd, 11, "veth_a", &info) == 0,
                       "GETLINK lo/veth_a failed");
    }

    NL_TEST_ASSERT(verify_epoll_for_dump(control_fd) == 0,
                   "epoll dump readiness test failed");
    NL_TEST_ASSERT(verify_bind_groups_linux_abi() == 0,
                   "bind-path LIST_MEMBERSHIPS test failed");
    NL_TEST_ASSERT(verify_memberships_and_notify(control_fd, info.ifindex, info.mtu) == 0,
                   "membership/notify test failed");

    close(control_fd);
    printf("netlink sockopt and epoll tests passed\n");
    return 0;
}
