#include <arpa/inet.h>
#include <assert.h>
#include <net/if.h>
#include <netinet/in.h>
#include <pthread.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <unistd.h>

#define SERVER_IP "192.168.2.3"
#define CLIENT_IP "192.168.1.1"
#define PORT 34254
#define BUFFER_SIZE 1024
#define CLIENT_DEVICE "veth-ns1"

// 错误处理函数
void handle_error_message(const char *message) {
    perror(message);
    exit(EXIT_FAILURE);
}

// 服务器线程函数
void *server_func(void *arg) {
    int sockfd;
    struct sockaddr_in server_addr, client_addr;
    char buffer[BUFFER_SIZE];
    socklen_t client_len = sizeof(client_addr);

    if ((sockfd = socket(AF_INET, SOCK_DGRAM, 0)) < 0) {
        handle_error_message("[server] Failed to create socket");
    }
    int pktinfo_enabled = 1;
    if (setsockopt(sockfd,
                   IPPROTO_IP,
                   IP_PKTINFO,
                   &pktinfo_enabled,
                   sizeof(pktinfo_enabled)) < 0) {
        handle_error_message("[server] Failed to enable IP_PKTINFO");
    }

    memset(&server_addr, 0, sizeof(server_addr));
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(PORT);
    if (inet_pton(AF_INET, SERVER_IP, &server_addr.sin_addr) <= 0) {
        handle_error_message("[server] Invalid server IP address");
    }

    if (bind(sockfd,
             (const struct sockaddr *)&server_addr,
             sizeof(server_addr)) < 0) {
        handle_error_message("[server] Failed to bind to " SERVER_IP);
    }
    printf("[server] Listening on %s:%d\n", SERVER_IP, PORT);

    char control[CMSG_SPACE(sizeof(struct in_pktinfo))];
    struct iovec iov = {
        .iov_base = buffer,
        .iov_len = BUFFER_SIZE,
    };
    struct msghdr msg = {
        .msg_name = &client_addr,
        .msg_namelen = client_len,
        .msg_iov = &iov,
        .msg_iovlen = 1,
        .msg_control = control,
        .msg_controllen = sizeof(control),
    };
    ssize_t n = recvmsg(sockfd, &msg, 0);
    if (n < 0) {
        handle_error_message("[server] Failed to receive");
    }
    const struct in_pktinfo *pktinfo = NULL;
    for (struct cmsghdr *cmsg = CMSG_FIRSTHDR(&msg); cmsg != NULL;
         cmsg = CMSG_NXTHDR(&msg, cmsg)) {
        if (cmsg->cmsg_level == IPPROTO_IP && cmsg->cmsg_type == IP_PKTINFO &&
            cmsg->cmsg_len >= CMSG_LEN(sizeof(struct in_pktinfo))) {
            pktinfo = (const struct in_pktinfo *)CMSG_DATA(cmsg);
            break;
        }
    }
    assert(pktinfo != NULL && "[server] Missing IP_PKTINFO");
    unsigned int ingress_ifindex = if_nametoindex("veth-host1");
    assert(ingress_ifindex != 0 && "[server] Missing ingress interface");
    assert(pktinfo->ipi_ifindex == (int)ingress_ifindex &&
           "[server] IP_PKTINFO must report the physical ingress interface");
    buffer[n] = '\0'; // 确保字符串正确终止

    // //debug
    // unsigned char *ip_bytes = (unsigned char *)&client_addr.sin_addr.s_addr;
    // printf("[DEBUG] Raw IP bytes received: %d.%d.%d.%d\n",
    //        ip_bytes[0],
    //        ip_bytes[1],
    //        ip_bytes[2],
    //        ip_bytes[3]);

    char client_ip_str[INET_ADDRSTRLEN];
    inet_ntop(AF_INET, &client_addr.sin_addr, client_ip_str, INET_ADDRSTRLEN);
    printf("[server] Received from %s:%d: %s\n",
           client_ip_str,
           ntohs(client_addr.sin_port),
           buffer);

    if (sendto(sockfd,
               buffer,
               n,
               0,
               (const struct sockaddr *)&client_addr,
               client_len) < 0) {
        handle_error_message("[server] Failed to send back");
    }
    sleep(1);
    printf("[server] Echoed back the message\n");

    close(sockfd);
    printf("server going to exit\n");
    return NULL;
}

// 客户端线程函数
void *client_func(void *arg) {
    int sockfd;
    struct sockaddr_in client_addr, server_addr;
    char buffer[BUFFER_SIZE];
    const char *msg = "Hello from veth1!";

    if ((sockfd = socket(AF_INET, SOCK_DGRAM, 0)) < 0) {
        handle_error_message("[client] Failed to create socket");
    }
    if (setsockopt(sockfd,
                   SOL_SOCKET,
                   SO_BINDTODEVICE,
                   CLIENT_DEVICE,
                   sizeof(CLIENT_DEVICE)) < 0) {
        handle_error_message("[client] Failed to bind to " CLIENT_DEVICE);
    }

    memset(&client_addr, 0, sizeof(client_addr));
    client_addr.sin_family = AF_INET;
    client_addr.sin_port = htons(0); // 端口为0，由操作系统自动选择
    if (inet_pton(AF_INET, CLIENT_IP, &client_addr.sin_addr) <= 0) {
        handle_error_message("[client] Invalid client IP address");
    }

    if (bind(sockfd,
             (const struct sockaddr *)&client_addr,
             sizeof(client_addr)) < 0) {
        handle_error_message("[client] Failed to bind to " CLIENT_IP);
    }

    memset(&server_addr, 0, sizeof(server_addr));
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(PORT);
    if (inet_pton(AF_INET, SERVER_IP, &server_addr.sin_addr) <= 0) {
        handle_error_message("[client] Invalid server IP address for connect");
    }

    if (connect(sockfd,
                (const struct sockaddr *)&server_addr,
                sizeof(server_addr)) < 0) {
        handle_error_message("[client] Failed to connect");
    }

    if (send(sockfd, msg, strlen(msg), 0) < 0) {
        handle_error_message("[client] Failed to send");
    }
    if (setsockopt(sockfd, SOL_SOCKET, SO_BINDTODEVICE, "", 0) < 0) {
        handle_error_message("[client] Failed to clear SO_BINDTODEVICE");
    }
    printf("[client] Sent: %s\n", msg);

    ssize_t n = recv(sockfd, buffer, BUFFER_SIZE, 0);
    if (n < 0) {
        handle_error_message("[client] Failed to receive");
    }
    buffer[n] = '\0'; // 确保字符串正确终止

    printf("[client] Received echo: %s\n", buffer);

    assert(strcmp(msg, buffer) == 0 && "[client] Mismatch in echo!");

    close(sockfd);
    printf("client goning to exit\n");
    return NULL;
}

int main() {
    pthread_t server_tid, client_tid;

    if (pthread_create(&server_tid, NULL, server_func, NULL) != 0) {
        handle_error_message("Failed to create server thread");
    }

    usleep(200 * 1000); // 200 milliseconds

    if (pthread_create(&client_tid, NULL, client_func, NULL) != 0) {
        handle_error_message("Failed to create client thread");
    }

    if (pthread_join(server_tid, NULL) != 0) {
        handle_error_message("Failed to join server thread");
    }
    if (pthread_join(client_tid, NULL) != 0) {
        handle_error_message("Failed to join client thread");
    }

    printf("\nTest completed: veth_a <--> veth_d UDP communication success\n");

    return EXIT_SUCCESS;
}
