#ifndef NETLINK_TEST_LIB_H
#define NETLINK_TEST_LIB_H

#define _GNU_SOURCE

#include <errno.h>
#include <linux/netlink.h>
#include <linux/rtnetlink.h>
#include <net/if.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

#define NL_TEST_BUF_SIZE 8192

#define NL_TEST_ASSERT(cond, fmt, ...)                                          \
    do {                                                                        \
        if (!(cond)) {                                                          \
            fprintf(stderr, "ASSERT FAIL: " fmt "\n", ##__VA_ARGS__);           \
            return 1;                                                           \
        }                                                                       \
    } while (0)

typedef int (*nl_msg_cb)(struct nlmsghdr *nlh, void *ctx);

struct nl_link_info {
    int ifindex;
    unsigned int flags;
    unsigned int mtu;
    char name[IFNAMSIZ];
    unsigned char addr[8];
    size_t addr_len;
};

static inline void nl_parse_rtattr(struct rtattr *tb[],
                                   int max,
                                   struct rtattr *rta,
                                   int len) {
    memset(tb, 0, sizeof(struct rtattr *) * (max + 1));
    while (RTA_OK(rta, len)) {
        if (rta->rta_type <= max) {
            tb[rta->rta_type] = rta;
        }
        rta = RTA_NEXT(rta, len);
    }
}

static inline int nl_addattr_l(struct nlmsghdr *nlh,
                               size_t maxlen,
                               int type,
                               const void *data,
                               size_t alen) {
    size_t len = RTA_LENGTH(alen);
    size_t new_len = NLMSG_ALIGN(nlh->nlmsg_len) + RTA_ALIGN(len);
    struct rtattr *rta;

    if (new_len > maxlen) {
        errno = ENOSPC;
        return -1;
    }

    rta = (struct rtattr *)(((char *)nlh) + NLMSG_ALIGN(nlh->nlmsg_len));
    rta->rta_type = type;
    rta->rta_len = (unsigned short)len;
    if (alen > 0 && data != NULL) {
        memcpy(RTA_DATA(rta), data, alen);
    }
    nlh->nlmsg_len = (unsigned int)new_len;
    return 0;
}

static inline int nl_open_socket(int protocol) {
    int fd = socket(AF_NETLINK, SOCK_RAW, protocol);
    struct sockaddr_nl addr;
    struct timeval timeout = {
        .tv_sec = 2,
        .tv_usec = 0,
    };

    if (fd < 0) {
        perror("socket(AF_NETLINK) failed");
        return -1;
    }

    memset(&addr, 0, sizeof(addr));
    addr.nl_family = AF_NETLINK;
    addr.nl_pid = 0;
    addr.nl_groups = 0;

    if (bind(fd, (struct sockaddr *)&addr, sizeof(addr)) < 0) {
        perror("bind(AF_NETLINK) failed");
        close(fd);
        return -1;
    }

    if (setsockopt(fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout)) < 0) {
        perror("setsockopt(SO_RCVTIMEO) failed");
        close(fd);
        return -1;
    }

    return fd;
}

static inline int nl_send_request(int fd, const void *buf, size_t len) {
    struct sockaddr_nl kernel_addr;

    memset(&kernel_addr, 0, sizeof(kernel_addr));
    kernel_addr.nl_family = AF_NETLINK;
    kernel_addr.nl_pid = 0;
    kernel_addr.nl_groups = 0;

    if (sendto(fd,
               buf,
               len,
               0,
               (struct sockaddr *)&kernel_addr,
               sizeof(kernel_addr)) < 0) {
        perror("sendto(AF_NETLINK) failed");
        return -1;
    }

    return 0;
}

static inline int nl_recv_ack(int fd, uint32_t seq, int expected_errno) {
    char buf[NL_TEST_BUF_SIZE];
    ssize_t len;
    struct nlmsghdr *nlh;

    while ((len = recv(fd, buf, sizeof(buf), 0)) > 0) {
        ssize_t remaining = len;

        for (nlh = (struct nlmsghdr *)buf; NLMSG_OK(nlh, remaining);
             nlh = NLMSG_NEXT(nlh, remaining)) {
            struct nlmsgerr *err;

            if (nlh->nlmsg_seq != seq) {
                continue;
            }
            if (nlh->nlmsg_type != NLMSG_ERROR) {
                continue;
            }

            err = (struct nlmsgerr *)NLMSG_DATA(nlh);
            if (expected_errno == 0) {
                if (err->error != 0) {
                    int saved_errno = -err->error;
                    errno = saved_errno;
                    perror("unexpected netlink ack error");
                    errno = saved_errno;
                    return -1;
                }
                return 0;
            }

            if (err->error != -expected_errno) {
                fprintf(stderr,
                        "unexpected netlink error: got=%d expected=%d\n",
                        -err->error,
                        expected_errno);
                errno = -err->error;
                return -1;
            }
            return 0;
        }
    }

    perror("recv ack failed");
    return -1;
}

static inline int nl_recv_dump(int fd, uint32_t seq, nl_msg_cb cb, void *ctx) {
    char buf[NL_TEST_BUF_SIZE];
    ssize_t len;
    struct nlmsghdr *nlh;

    while ((len = recv(fd, buf, sizeof(buf), 0)) > 0) {
        ssize_t remaining = len;

        for (nlh = (struct nlmsghdr *)buf; NLMSG_OK(nlh, remaining);
             nlh = NLMSG_NEXT(nlh, remaining)) {
            int cb_ret;

            if (nlh->nlmsg_seq != seq) {
                continue;
            }
            if (nlh->nlmsg_type == NLMSG_DONE) {
                return 0;
            }
            if (nlh->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *err = (struct nlmsgerr *)NLMSG_DATA(nlh);
                errno = err->error == 0 ? EPROTO : -err->error;
                perror("dump returned netlink error");
                return -1;
            }

            cb_ret = cb(nlh, ctx);
            if (cb_ret < 0) {
                return -1;
            }
        }
    }

    perror("recv dump failed");
    return -1;
}

static inline int nl_recv_single(int fd, uint32_t seq, nl_msg_cb cb, void *ctx) {
    char buf[NL_TEST_BUF_SIZE];
    ssize_t len;
    struct nlmsghdr *nlh;

    while ((len = recv(fd, buf, sizeof(buf), 0)) > 0) {
        ssize_t remaining = len;

        for (nlh = (struct nlmsghdr *)buf; NLMSG_OK(nlh, remaining);
             nlh = NLMSG_NEXT(nlh, remaining)) {
            int cb_ret;

            if (nlh->nlmsg_seq != seq) {
                continue;
            }
            if (nlh->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *err = (struct nlmsgerr *)NLMSG_DATA(nlh);
                errno = err->error == 0 ? EPROTO : -err->error;
                perror("single request returned netlink error");
                return -1;
            }
            if (nlh->nlmsg_type == NLMSG_DONE) {
                continue;
            }

            cb_ret = cb(nlh, ctx);
            if (cb_ret < 0) {
                return -1;
            }
            if (cb_ret == 0) {
                return 0;
            }
        }
    }

    perror("recv single failed");
    return -1;
}

static inline int nl_parse_link_info(struct nlmsghdr *nlh, struct nl_link_info *info) {
    struct ifinfomsg *ifi;
    struct rtattr *tb[IFLA_MAX + 1];
    int attr_len;

    if (nlh->nlmsg_type != RTM_NEWLINK) {
        errno = EPROTO;
        return -1;
    }

    ifi = (struct ifinfomsg *)NLMSG_DATA(nlh);
    attr_len = (int)(nlh->nlmsg_len - NLMSG_LENGTH(sizeof(*ifi)));
    nl_parse_rtattr(tb, IFLA_MAX, IFLA_RTA(ifi), attr_len);

    if (tb[IFLA_IFNAME] == NULL || tb[IFLA_MTU] == NULL || tb[IFLA_ADDRESS] == NULL) {
        errno = EPROTO;
        return -1;
    }

    memset(info, 0, sizeof(*info));
    info->ifindex = ifi->ifi_index;
    info->flags = ifi->ifi_flags;
    info->mtu = *(unsigned int *)RTA_DATA(tb[IFLA_MTU]);
    strncpy(info->name, (const char *)RTA_DATA(tb[IFLA_IFNAME]), sizeof(info->name) - 1);
    info->addr_len = RTA_PAYLOAD(tb[IFLA_ADDRESS]);
    if (info->addr_len > sizeof(info->addr)) {
        info->addr_len = sizeof(info->addr);
    }
    memcpy(info->addr, RTA_DATA(tb[IFLA_ADDRESS]), info->addr_len);
    return 0;
}

struct nl_link_lookup_ctx {
    struct nl_link_info *out;
};

static inline int nl_link_lookup_cb(struct nlmsghdr *nlh, void *ctx) {
    struct nl_link_lookup_ctx *lookup = (struct nl_link_lookup_ctx *)ctx;
    return nl_parse_link_info(nlh, lookup->out);
}

static inline int nl_get_link_by_name(int fd,
                                      uint32_t seq,
                                      const char *name,
                                      struct nl_link_info *out) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
        char attrbuf[128];
    } req;
    struct nl_link_lookup_ctx ctx = {
        .out = out,
    };

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_GETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;

    if (nl_addattr_l(&req.nlh, sizeof(req), IFLA_IFNAME, name, strlen(name) + 1) < 0) {
        perror("nl_addattr_l(IFLA_IFNAME) failed");
        return -1;
    }

    if (nl_send_request(fd, &req, req.nlh.nlmsg_len) < 0) {
        return -1;
    }

    return nl_recv_single(fd, seq, nl_link_lookup_cb, &ctx);
}

static inline int nl_get_link_by_index(int fd,
                                       uint32_t seq,
                                       int ifindex,
                                       struct nl_link_info *out) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
    } req;
    struct nl_link_lookup_ctx ctx = {
        .out = out,
    };

    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_GETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;

    if (nl_send_request(fd, &req, req.nlh.nlmsg_len) < 0) {
        return -1;
    }

    return nl_recv_single(fd, seq, nl_link_lookup_cb, &ctx);
}

static inline int nl_lookup_ifindex(int fd, const char *name, int *ifindex) {
    struct nl_link_info info;

    if (nl_get_link_by_name(fd, 100, name, &info) < 0) {
        return -1;
    }

    *ifindex = info.ifindex;
    return 0;
}

#endif
