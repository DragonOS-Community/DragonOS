#define _GNU_SOURCE
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#include <arpa/inet.h>
#include <linux/rtnetlink.h>
#include <sys/socket.h>

// 定义一个足够大的缓冲区来接收Netlink消息
#define NL_BUFSIZE 8192

// 结构体，用于将请求消息封装起来
struct nl_req_t {
    struct nlmsghdr nlh;
    struct ifaddrmsg ifa;
};

void parse_rtattr(struct rtattr *tb[], int max, struct rtattr *rta, int len) {
    memset(tb, 0, sizeof(struct rtattr *) * (max + 1));
    while (RTA_OK(rta, len)) {
        if (rta->rta_type <= max) {
            tb[rta->rta_type] = rta;
        }
        rta = RTA_NEXT(rta, len);
    }
}

int run_netlink_test() {
    int sock_fd;
    struct sockaddr_nl sa_nl;
    struct nl_req_t req;

    // struct iovec iov;
    // struct msghdr msg;
    // struct sockaddr_nl src_addr; // 用于接收发送方的地址

    char buf[NL_BUFSIZE];
    ssize_t len;
    struct nlmsghdr *nlh;

    // 创建Netlink套接字
    sock_fd = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
    if (sock_fd < 0) {
        perror("socket creation failed");
        return EXIT_FAILURE;
    }

    // 设置Netlink地址
    memset(&sa_nl, 0, sizeof(sa_nl));
    sa_nl.nl_family = AF_NETLINK;
    sa_nl.nl_pid = getpid(); // 使用进程ID作为地址

    // 绑定Netlink套接字
    if (bind(sock_fd, (struct sockaddr *)&sa_nl, sizeof(sa_nl)) < 0) {
        perror("socket bind failed");
        close(sock_fd);
        return EXIT_FAILURE;
    }

    // 构建RTM_GETADDR请求消息
    memset(&req, 0, sizeof(req));
    req.nlh.nlmsg_len = NLMSG_LENGTH(sizeof(struct ifaddrmsg));
    // 这是关键：设置DUMP标志以获取所有地址
    req.nlh.nlmsg_flags = NLM_F_REQUEST | NLM_F_DUMP;
    req.nlh.nlmsg_type = RTM_GETADDR;
    req.nlh.nlmsg_seq = 1; // 序列号，用于匹配请求和响应
    req.nlh.nlmsg_pid = getpid();
    req.ifa.ifa_family =
        AF_INET; // 只请求 IPv4 地址 (也可以用 AF_UNSPEC 获取所有)

    // 内核的目标地址
    struct sockaddr_nl dest_addr;
    memset(&dest_addr, 0, sizeof(dest_addr));
    dest_addr.nl_family = AF_NETLINK;
    dest_addr.nl_pid = 0;    // 0 for the kernel
    dest_addr.nl_groups = 0; // Unicast

    // 发送请求到内核
    if (sendto(sock_fd,
               &req,
               req.nlh.nlmsg_len,
               0,
               (struct sockaddr *)&dest_addr,
               sizeof(dest_addr)) < 0) {
        perror("send failed");
        close(sock_fd);
        return EXIT_FAILURE;
    }

    printf("Sent RTM_GETADDR request with DUMP flag.\n\n");

    // // 准备 recvmsg 所需的结构体
    // // iovec 指向我们的主数据缓冲区
    // iov.iov_base = buf;
    // iov.iov_len = sizeof(buf);

    // // msghdr 将所有部分组合在一起
    // msg.msg_name = &src_addr; // 填充发送方地址
    // msg.msg_namelen = sizeof(src_addr);
    // msg.msg_iov = &iov; // 指向数据缓冲区
    // msg.msg_iovlen = 1;
    // msg.msg_control = NULL; // 我们暂时不处理控制消息
    // msg.msg_controllen = 0;

    int received_messages = 0;

    // 循环接收响应
    while ((len = recv(sock_fd, buf, sizeof(buf), 0)) > 0) {
        // ssize_t len = recvmsg(sock_fd, &msg, 0);

        // if (len < 0) {
        //     perror("recvmsg failed");
        //     break;
        // }

        // if (len == 0) {
        //     printf("EOF on netlink socket\n");
        //     break;
        // }

        // if (msg.msg_flags & MSG_TRUNC) {
        //     fprintf(
        //         stderr,
        //         "Warning: Message was truncated. Buffer may be too small.\n");
        // }


        // 使用 NLMSG_OK 遍历缓冲区中可能存在的多条消息
        for (nlh = (struct nlmsghdr *)buf; NLMSG_OK(nlh, len);
             nlh = NLMSG_NEXT(nlh, len)) {

            // 如果是 DUMP 结束的标志，则退出循环
            if (nlh->nlmsg_type == NLMSG_DONE) {
                printf("--- End of DUMP ---\n");
                if (!received_messages) {
                    printf("(Received an empty list as expected)\n");
                }
                close(sock_fd);
                return EXIT_SUCCESS;
            }

            // 如果是错误消息
            if (nlh->nlmsg_type == NLMSG_ERROR) {
                struct nlmsgerr *err = (struct nlmsgerr *)NLMSG_DATA(nlh);
                fprintf(stderr,
                        "Netlink error received: %s\n",
                        strerror(-err->error));
                close(sock_fd);
                return EXIT_FAILURE;
            }

            // 只处理我们期望的 RTM_NEWADDR 消息
            if (nlh->nlmsg_type != RTM_NEWADDR) {
                printf("Received unexpected message type: %d\n",
                       nlh->nlmsg_type);
                continue;
            }

            // printf("Received message from PID: %u\n", src_addr.nl_pid);

            // 表明我们至少接收到了一条信息
            received_messages = 1;

            struct ifaddrmsg *ifa = (struct ifaddrmsg *)NLMSG_DATA(nlh);
            struct rtattr *rta_tb[IFA_MAX + 1];

            // 解析消息中的路由属性
            int rta_len = nlh->nlmsg_len - NLMSG_LENGTH(sizeof(*ifa));
            parse_rtattr(rta_tb, IFA_MAX, IFA_RTA(ifa), rta_len);

            printf("Interface Index: %d, PrefixLen: %d, Scope: %d\n",
                   ifa->ifa_index,
                   ifa->ifa_prefixlen,
                   ifa->ifa_scope);

            char ip_addr_str[INET6_ADDRSTRLEN];

            // 打印 IFA_LABEL (对应你的 AddrAttr::Label)
            if (rta_tb[IFA_LABEL]) {
                printf("\tLabel: %s\n", (char *)RTA_DATA(rta_tb[IFA_LABEL]));
            }

            // 打印 IFA_ADDRESS (对应你的 AddrAttr::Address)
            if (rta_tb[IFA_ADDRESS]) {
                inet_ntop(ifa->ifa_family,
                          RTA_DATA(rta_tb[IFA_ADDRESS]),
                          ip_addr_str,
                          sizeof(ip_addr_str));
                printf("\tAddress: %s\n", ip_addr_str);
            }

            // 打印 IFA_LOCAL (对应你的 AddrAttr::Local)
            if (rta_tb[IFA_LOCAL]) {
                inet_ntop(ifa->ifa_family,
                          RTA_DATA(rta_tb[IFA_LOCAL]),
                          ip_addr_str,
                          sizeof(ip_addr_str));
                printf("\tLocal: %s\n", ip_addr_str);
            }
            printf("----------------------------------------\n");
        }
    }

    if (len < 0) {
        perror("recv failed");
    }

    close(sock_fd);
    return EXIT_SUCCESS;
}

int main(int argc, char *argv[]) {

    printf("=========== STAGE 1: Testing in Default Network Namespace "
           "===========\n");
    if (run_netlink_test() != 0) {
        fprintf(stderr, "Test failed in the default namespace.\n");
        return EXIT_FAILURE;
    }

    printf("\n\n=========== STAGE 2: Creating and Testing in a New Network "
           "Namespace ===========\n");

    // ** 关键步骤：创建新的网络命名空间 **
    // 这个调用会将当前进程移入一个新的、隔离的网络栈中
    if (unshare(CLONE_NEWNET) == -1) {
        perror("unshare(CLONE_NEWNET) failed");
        fprintf(stderr,
                "This test requires root privileges (e.g., 'sudo "
                "./your_program').\n");
        return EXIT_FAILURE;
    }
    printf("Successfully created and entered a new network namespace.\n");

    // 在新的命名空间中再次运行同样的测试
    if (run_netlink_test() != 0) {
        fprintf(stderr, "Test failed in the new namespace.\n");
        return EXIT_FAILURE;
    }

    printf("\nAll tests completed successfully.\n");
    return EXIT_SUCCESS;
}