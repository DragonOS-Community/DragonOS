#define _GNU_SOURCE
#include <arpa/inet.h>
#include <linux/rtnetlink.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/time.h>
#include <unistd.h>

static int open_netlink_route_socket(void) {
    int sock_fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (sock_fd < 0) {
        perror("socket creation failed");
        return -1;
    }

    struct sockaddr_nl sa_nl;
    memset(&sa_nl, 0, sizeof(sa_nl));
    sa_nl.nl_family = AF_NETLINK;
    sa_nl.nl_pid = getpid();

    if (bind(sock_fd, (struct sockaddr *)&sa_nl, sizeof(sa_nl)) < 0) {
        perror("socket bind failed");
        close(sock_fd);
        return -1;
    }

    struct timeval timeout = {
        .tv_sec = 1,
        .tv_usec = 0,
    };
    setsockopt(sock_fd, SOL_SOCKET, SO_RCVTIMEO, &timeout, sizeof(timeout));
    return sock_fd;
}

static int send_short_msg(__u16 nlmsg_type, __u16 nlmsg_flags, int payload_size, __u32 seq) {
    int sock_fd = open_netlink_route_socket();
    if (sock_fd < 0) {
        return -1;
    }

    char buf[128];
    memset(buf, 0, sizeof(buf));
    struct nlmsghdr *nlh = (struct nlmsghdr *)buf;

    nlh->nlmsg_len = NLMSG_LENGTH(payload_size);
    nlh->nlmsg_type = nlmsg_type;
    nlh->nlmsg_flags = nlmsg_flags;
    nlh->nlmsg_seq = seq;
    nlh->nlmsg_pid = getpid();

    struct sockaddr_nl dest_addr;
    memset(&dest_addr, 0, sizeof(dest_addr));
    dest_addr.nl_family = AF_NETLINK;
    dest_addr.nl_pid = 0;
    dest_addr.nl_groups = 0;

    printf("Sending type=%u, nlmsg_len=%u (header=16, payload=%d)\n",
           (unsigned)nlmsg_type,
           (unsigned)nlh->nlmsg_len,
           payload_size);

    ssize_t sent = sendto(sock_fd,
                          buf,
                          nlh->nlmsg_len,
                          0,
                          (struct sockaddr *)&dest_addr,
                          sizeof(dest_addr));
    if (sent < 0) {
        perror("sendto failed");
        close(sock_fd);
        return -1;
    }
    printf("Sent %zd bytes\n", sent);

    close(sock_fd);
    return 0;
}

static int test_short_route_message(void) {
    printf("Test: RTM_GETROUTE with truncated payload (4B < sizeof(rtmsg)=12B)\n");
    return send_short_msg(RTM_GETROUTE, NLM_F_REQUEST | NLM_F_DUMP, 4, 1);
}

static int test_tiny_route_message(void) {
    printf("Test: RTM_GETROUTE with tiny payload (1B)\n");
    return send_short_msg(RTM_GETROUTE, NLM_F_REQUEST, 1, 2);
}

static int test_short_link_message(void) {
    printf("Test: RTM_GETLINK with truncated payload (4B < sizeof(ifinfomsg)=16B)\n");
    return send_short_msg(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 4, 3);
}

static int test_rtgen_link_dump_message(void) {
    printf("Test: RTM_GETLINK dump with rtgenmsg-sized payload (1B)\n");
    return send_short_msg(RTM_GETLINK, NLM_F_REQUEST | NLM_F_DUMP, 1, 4);
}

static int test_rtgen_addr_dump_message(void) {
    printf("Test: RTM_GETADDR dump with rtgenmsg-sized payload (1B)\n");
    return send_short_msg(RTM_GETADDR, NLM_F_REQUEST | NLM_F_DUMP, 1, 5);
}

int main(void) {
    printf("========================================\n");
    printf("Netlink Short Payload Regression Test\n");
    printf("========================================\n\n");

    if (test_short_route_message() < 0) {
        printf("FAIL: test_short_route_message\n");
        return 1;
    }
    printf("PASS: test_short_route_message completed without kernel panic\n");

    if (test_tiny_route_message() < 0) {
        printf("FAIL: test_tiny_route_message\n");
        return 1;
    }
    printf("PASS: test_tiny_route_message completed without kernel panic\n");

    if (test_short_link_message() < 0) {
        printf("FAIL: test_short_link_message\n");
        return 1;
    }
    printf("PASS: test_short_link_message completed without kernel panic\n");

    if (test_rtgen_link_dump_message() < 0) {
        printf("FAIL: test_rtgen_link_dump_message\n");
        return 1;
    }
    printf("PASS: test_rtgen_link_dump_message completed without kernel panic\n");

    if (test_rtgen_addr_dump_message() < 0) {
        printf("FAIL: test_rtgen_addr_dump_message\n");
        return 1;
    }
    printf("PASS: test_rtgen_addr_dump_message completed without kernel panic\n");

    printf("\n========================================\n");
    printf("All regression cases passed!\n");
    printf("========================================\n");

    return 0;
}
