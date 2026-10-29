#include "netlink_test_lib.h"

#include <fcntl.h>
#include <arpa/inet.h>
#include <linux/if_link.h>
#include <linux/veth.h>
#include <poll.h>
#include <sched.h>
#include <sys/stat.h>

static uint32_t next_seq = 500;

static struct rtattr *begin_nest(struct nlmsghdr *nlh, size_t maxlen,
                                 int type, const void *payload, size_t len) {
    size_t offset = NLMSG_ALIGN(nlh->nlmsg_len);
    if (nl_addattr_l(nlh, maxlen, type, payload, len) < 0)
        return NULL;
    return (struct rtattr *)((char *)nlh + offset);
}

static void end_nest(struct nlmsghdr *nlh, struct rtattr *nest) {
    nest->rta_len = (unsigned short)((char *)nlh + nlh->nlmsg_len - (char *)nest);
}

static int new_veth(int fd, const char *name, const char *peer, int expected_errno) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
        char attrs[512];
    } req;
    struct ifinfomsg peer_ifi;
    struct rtattr *linkinfo, *info_data, *peer_attr;
    uint32_t seq = next_seq++;

    memset(&req, 0, sizeof(req));
    memset(&peer_ifi, 0, sizeof(peer_ifi));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_NEWLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    peer_ifi.ifi_family = AF_UNSPEC;

    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFLA_IFNAME,
                                name, strlen(name) + 1) == 0,
                   "outer name attr failed");
    linkinfo = begin_nest(&req.nlh, sizeof(req), IFLA_LINKINFO, NULL, 0);
    NL_TEST_ASSERT(linkinfo != NULL, "linkinfo nest failed");
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFLA_INFO_KIND,
                                "veth", sizeof("veth")) == 0,
                   "veth kind attr failed");
    info_data = begin_nest(&req.nlh, sizeof(req), IFLA_INFO_DATA, NULL, 0);
    NL_TEST_ASSERT(info_data != NULL, "info data nest failed");
    peer_attr = begin_nest(&req.nlh, sizeof(req), VETH_INFO_PEER,
                           &peer_ifi, sizeof(peer_ifi));
    NL_TEST_ASSERT(peer_attr != NULL, "peer nest failed");
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFLA_IFNAME,
                                peer, strlen(peer) + 1) == 0,
                   "peer name attr failed");
    end_nest(&req.nlh, peer_attr);
    end_nest(&req.nlh, info_data);
    end_nest(&req.nlh, linkinfo);

    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_NEWLINK send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, expected_errno) == 0,
                   "RTM_NEWLINK ack mismatch");
    return 0;
}

static int move_veth(int fd, const char *name, int target_fd,
                     const char *new_name, int expected_errno) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
        char attrs[128];
    } req;
    uint32_t seq = next_seq++;
    int ifindex;

    NL_TEST_ASSERT(nl_lookup_ifindex(fd, name, &ifindex) == 0,
                   "lookup moving veth %s failed", name);
    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_SETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFLA_NET_NS_FD,
                                &target_fd, sizeof(target_fd)) == 0,
                   "target netns attr failed");
    if (new_name) {
        NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFLA_IFNAME,
                                    new_name, strlen(new_name) + 1) == 0,
                       "move name attr failed");
    }
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_SETLINK(move) send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, expected_errno) == 0,
                   "RTM_SETLINK(move) ack mismatch");
    return 0;
}

static int del_link(int fd, const char *name) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
    } req;
    uint32_t seq = next_seq++;
    int ifindex;

    NL_TEST_ASSERT(nl_lookup_ifindex(fd, name, &ifindex) == 0,
                   "lookup deleting link %s failed", name);
    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_DELLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_DELLINK send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0,
                   "RTM_DELLINK ack failed");
    return 0;
}

static int set_link_up(int fd, const char *name) {
    struct {
        struct nlmsghdr nlh;
        struct ifinfomsg ifi;
    } req;
    uint32_t seq = next_seq++;
    int ifindex;

    NL_TEST_ASSERT(nl_lookup_ifindex(fd, name, &ifindex) == 0,
                   "lookup link %s for UP failed", name);
    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifi));
    req.nlh.nlmsg_type = RTM_SETLINK;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK;
    req.nlh.nlmsg_seq = seq;
    req.ifi.ifi_family = AF_UNSPEC;
    req.ifi.ifi_index = ifindex;
    req.ifi.ifi_flags = IFF_UP;
    req.ifi.ifi_change = IFF_UP;
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_SETLINK(UP) send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0,
                   "RTM_SETLINK(UP) ack failed");
    return 0;
}

static int add_ipv6_address(int fd, const char *name, const char *address) {
    struct {
        struct nlmsghdr nlh;
        struct ifaddrmsg ifa;
        char attrs[96];
    } req;
    struct in6_addr parsed;
    uint32_t seq = next_seq++;
    int ifindex;

    NL_TEST_ASSERT(inet_pton(AF_INET6, address, &parsed) == 1,
                   "invalid IPv6 test address %s", address);
    NL_TEST_ASSERT(nl_lookup_ifindex(fd, name, &ifindex) == 0,
                   "lookup %s for IPv6 address failed", name);
    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(req.ifa));
    req.nlh.nlmsg_type = RTM_NEWADDR;
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_ACK | NLM_F_CREATE | NLM_F_EXCL;
    req.nlh.nlmsg_seq = seq;
    req.ifa.ifa_family = AF_INET6;
    req.ifa.ifa_prefixlen = 64;
    req.ifa.ifa_index = ifindex;
    NL_TEST_ASSERT(nl_addattr_l(&req.nlh, sizeof(req), IFA_ADDRESS,
                                &parsed, sizeof(parsed)) == 0,
                   "IPv6 address attr failed");
    NL_TEST_ASSERT(nl_send_request(fd, &req, req.nlh.nlmsg_len) == 0,
                   "RTM_NEWADDR send failed");
    NL_TEST_ASSERT(nl_recv_ack(fd, seq, 0) == 0,
                   "RTM_NEWADDR ack failed");
    return 0;
}

static int bind_udp6(const char *address, uint16_t port) {
    struct sockaddr_in6 local;
    int fd = socket(AF_INET6, SOCK_DGRAM | SOCK_NONBLOCK, 0);
    if (fd < 0)
        return -1;
    memset(&local, 0, sizeof(local));
    local.sin6_family = AF_INET6;
    local.sin6_port = htons(port);
    if (inet_pton(AF_INET6, address, &local.sin6_addr) != 1 ||
        bind(fd, (struct sockaddr *)&local, sizeof(local)) != 0) {
        close(fd);
        return -1;
    }
    return fd;
}

static int present(int fd, const char *name) {
    struct nl_link_info link;
    char path[128];
    NL_TEST_ASSERT(nl_get_link_by_name(fd, next_seq++, name, &link) == 0,
                   "GETLINK %s failed", name);
    NL_TEST_ASSERT(snprintf(path, sizeof(path), "/sys/class/net/%s", name) > 0,
                   "sysfs path format failed");
    NL_TEST_ASSERT(access(path, F_OK) == 0,
                   "sysfs entry %s missing", path);
    return 0;
}

static int present_in_netlink(int fd, const char *name) {
    struct nl_link_info link;
    NL_TEST_ASSERT(nl_get_link_by_name(fd, next_seq++, name, &link) == 0,
                   "GETLINK %s failed", name);
    return 0;
}

int main(void) {
    int source_ns = -1, target_ns = -1, source_fd = -1, target_fd = -1;
    int old_udp = -1, target_udp = -1, sender_udp = -1;

    source_ns = open("/proc/self/ns/net", O_RDONLY);
    NL_TEST_ASSERT(source_ns >= 0, "open source netns failed: %s", strerror(errno));
    NL_TEST_ASSERT(unshare(CLONE_NEWNET) == 0,
                   "unshare target netns failed: %s", strerror(errno));
    target_ns = open("/proc/self/ns/net", O_RDONLY);
    NL_TEST_ASSERT(target_ns >= 0, "open target netns failed: %s", strerror(errno));
    target_fd = nl_open_socket(NETLINK_ROUTE);
    NL_TEST_ASSERT(target_fd >= 0, "target netlink socket failed");
    NL_TEST_ASSERT(new_veth(target_fd, "dkc36_tgt", "dkc36_tp", 0) == 0,
                   "create target collision link failed");

    NL_TEST_ASSERT(setns(source_ns, CLONE_NEWNET) == 0,
                   "return to source netns failed: %s", strerror(errno));
    source_fd = nl_open_socket(NETLINK_ROUTE);
    NL_TEST_ASSERT(source_fd >= 0, "source netlink socket failed");
    NL_TEST_ASSERT(new_veth(source_fd, "dkc36_tgt", "dkc36_sp", 0) == 0,
                   "create source pair failed");
    NL_TEST_ASSERT(move_veth(source_fd, "dkc36_tgt", target_ns, NULL, EEXIST) == 0,
                   "same-name target move should fail");
    NL_TEST_ASSERT(present(source_fd, "dkc36_tgt") == 0,
                   "source endpoint vanished after failed move");
    NL_TEST_ASSERT(present(source_fd, "dkc36_sp") == 0,
                   "source peer vanished after failed move");

    NL_TEST_ASSERT(set_link_up(source_fd, "dkc36_tgt") == 0,
                   "source endpoint UP failed");
    NL_TEST_ASSERT(add_ipv6_address(source_fd, "dkc36_tgt", "2001:db8:36::1") == 0,
                   "source IPv6 setup failed");
    old_udp = bind_udp6("2001:db8:36::1", 46361);
    NL_TEST_ASSERT(old_udp >= 0, "source UDP6 bind failed: %s", strerror(errno));

    NL_TEST_ASSERT(move_veth(source_fd, "dkc36_tgt", target_ns, "dkc36_moved", 0) == 0,
                   "retry move with free name failed");
    NL_TEST_ASSERT(present(source_fd, "dkc36_sp") == 0,
                   "source peer vanished after successful move");
    NL_TEST_ASSERT(setns(target_ns, CLONE_NEWNET) == 0,
                   "enter target netns for sysfs check failed: %s", strerror(errno));
    // The inherited /sys mount retains its source-netns view across setns;
    // GETLINK on a socket opened in the target checks the moved device here.
    // Kernfs namespace rekeying itself is covered by the in-kernel test.
    NL_TEST_ASSERT(present_in_netlink(target_fd, "dkc36_moved") == 0,
                   "moved endpoint missing in target netns");
    struct nl_link_info moved;
    NL_TEST_ASSERT(nl_get_link_by_name(target_fd, next_seq++, "dkc36_moved", &moved) == 0,
                   "GETLINK moved endpoint failed");
    NL_TEST_ASSERT((moved.flags & IFF_UP) == 0,
                   "moved endpoint must be administratively DOWN until target enables it");
    NL_TEST_ASSERT(add_ipv6_address(target_fd, "dkc36_moved", "2001:db8:36::1") == 0,
                   "target IPv6 setup failed");
    NL_TEST_ASSERT(set_link_up(target_fd, "dkc36_moved") == 0,
                   "target endpoint UP failed");
    target_udp = bind_udp6("2001:db8:36::1", 46361);
    NL_TEST_ASSERT(target_udp >= 0, "target UDP6 bind failed: %s", strerror(errno));
    NL_TEST_ASSERT(setns(source_ns, CLONE_NEWNET) == 0,
                   "return to source netns after sysfs check failed: %s", strerror(errno));
    NL_TEST_ASSERT(add_ipv6_address(source_fd, "dkc36_sp", "2001:db8:36::2") == 0,
                   "source peer IPv6 setup failed");
    NL_TEST_ASSERT(set_link_up(source_fd, "dkc36_sp") == 0,
                   "source peer UP failed");
    sender_udp = bind_udp6("2001:db8:36::2", 0);
    NL_TEST_ASSERT(sender_udp >= 0, "source UDP6 sender bind failed: %s", strerror(errno));
    struct sockaddr_in6 remote;
    struct pollfd ready = {.fd = target_udp, .events = POLLIN};
    char payload[] = "netns-isolated-udp6";
    char received[sizeof(payload)];
    memset(&remote, 0, sizeof(remote));
    remote.sin6_family = AF_INET6;
    remote.sin6_port = htons(46361);
    NL_TEST_ASSERT(inet_pton(AF_INET6, "2001:db8:36::1", &remote.sin6_addr) == 1,
                   "target IPv6 parse failed");
    for (int attempt = 0; attempt < 4 && poll(&ready, 1, 250) == 0; ++attempt) {
        ssize_t sent = sendto(sender_udp, payload, sizeof(payload), 0,
                              (struct sockaddr *)&remote, sizeof(remote));
        NL_TEST_ASSERT(sent == (ssize_t)sizeof(payload) || errno == EAGAIN,
                       "source UDP6 send failed: %s", strerror(errno));
    }
    NL_TEST_ASSERT(poll(&ready, 1, 1500) > 0 && (ready.revents & POLLIN),
                   "target UDP6 packet did not arrive");
    NL_TEST_ASSERT(recv(target_udp, received, sizeof(received), 0) == (ssize_t)sizeof(payload) &&
                       memcmp(received, payload, sizeof(payload)) == 0,
                   "target UDP6 payload mismatch");
    NL_TEST_ASSERT(recv(old_udp, received, sizeof(received), 0) < 0 && errno == EAGAIN,
                   "old namespace UDP6 socket received target packet");
    NL_TEST_ASSERT(access("/sys/class/net/dkc36_tgt", F_OK) != 0 && errno == ENOENT,
                   "source sysfs retained the moved endpoint's old name");
    NL_TEST_ASSERT(access("/sys/class/net/dkc36_moved", F_OK) != 0 && errno == ENOENT,
                   "source sysfs exposed a target-netns endpoint");
    NL_TEST_ASSERT(del_link(source_fd, "dkc36_sp") == 0,
                   "cross-netns pair cleanup failed");
    NL_TEST_ASSERT(del_link(target_fd, "dkc36_tgt") == 0,
                   "target collision pair cleanup failed");

    NL_TEST_ASSERT(new_veth(source_fd, "dkc36_taken", "dkc36_existing", 0) == 0,
                   "create pre-existing outer name failed");
    NL_TEST_ASSERT(new_veth(source_fd, "dkc36_taken", "dkc36_ghost", EEXIST) == 0,
                   "second endpoint collision should fail pair creation");
    NL_TEST_ASSERT(access("/sys/class/net/dkc36_ghost", F_OK) != 0 && errno == ENOENT,
                   "failed pair left a ghost peer in sysfs");
    NL_TEST_ASSERT(del_link(source_fd, "dkc36_taken") == 0,
                   "remove conflicting outer name failed");
    NL_TEST_ASSERT(new_veth(source_fd, "dkc36_taken", "dkc36_ghost", 0) == 0,
                   "retry pair creation failed");
    NL_TEST_ASSERT(present(source_fd, "dkc36_taken") == 0,
                   "retried outer endpoint missing");
    NL_TEST_ASSERT(present(source_fd, "dkc36_ghost") == 0,
                   "retried peer endpoint missing");
    NL_TEST_ASSERT(del_link(source_fd, "dkc36_taken") == 0,
                   "retried pair cleanup failed");

    close(source_fd);
    close(target_fd);
    close(sender_udp);
    close(target_udp);
    close(old_udp);
    close(target_ns);
    close(source_ns);
    printf("rtnetlink veth transaction tests passed\n");
    return 0;
}
