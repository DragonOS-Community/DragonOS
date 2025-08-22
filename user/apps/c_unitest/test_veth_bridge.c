#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <pthread.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <arpa/inet.h>
#include <assert.h>

#define SERVER_IP "200.0.0.4"
#define CLIENT_IP "200.0.0.1"
#define PORT 34254
#define BUFFER_SIZE 1024

// 错误处理函数
void die(const char *message) {
    perror(message);
    exit(EXIT_FAILURE);
}

// 服务器线程函数
void *server_func(void *arg) {
    int sockfd;
    struct sockaddr_in server_addr, client_addr;
    char buffer[BUFFER_SIZE];
    socklen_t client_len = sizeof(client_addr);

    // 1. 创建 UDP socket
    if ((sockfd = socket(AF_INET, SOCK_DGRAM, 0)) < 0) {
        die("[server] Failed to create socket");
    }

    // 2. 准备服务器地址结构
    memset(&server_addr, 0, sizeof(server_addr));
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(PORT);
    if (inet_pton(AF_INET, SERVER_IP, &server_addr.sin_addr) <= 0) {
        die("[server] Invalid server IP address");
    }

    // 3. 绑定 socket 到指定地址和端口
    if (bind(sockfd, (const struct sockaddr *)&server_addr, sizeof(server_addr)) < 0) {
        die("[server] Failed to bind to " SERVER_IP);
    }
    printf("[server] Listening on %s:%d\n", SERVER_IP, PORT);

    // 4. 接收数据
    ssize_t n = recvfrom(sockfd, buffer, BUFFER_SIZE, 0, (struct sockaddr *)&client_addr, &client_len);
    if (n < 0) {
        die("[server] Failed to receive");
    }
    buffer[n] = '\0'; // 确保字符串正确终止

    char client_ip_str[INET_ADDRSTRLEN];
    inet_ntop(AF_INET, &client_addr.sin_addr, client_ip_str, INET_ADDRSTRLEN);
    printf("[server] Received from %s:%d: %s\n", client_ip_str, ntohs(client_addr.sin_port), buffer);

    // 5. 将数据回显给客户端
    if (sendto(sockfd, buffer, n, 0, (const struct sockaddr *)&client_addr, client_len) < 0) {
        die("[server] Failed to send back");
    }
    printf("[server] Echoed back the message\n");

    close(sockfd);
    printf("server goning to exit\n");
    return NULL;
}

// 客户端线程函数
void *client_func(void *arg) {
    int sockfd;
    struct sockaddr_in client_addr, server_addr;
    char buffer[BUFFER_SIZE];
    const char *msg = "Hello from veth1!";

    // 1. 创建 UDP socket
    if ((sockfd = socket(AF_INET, SOCK_DGRAM, 0)) < 0) {
        die("[client] Failed to create socket");
    }

    // 2. 准备客户端地址结构（用于绑定）
    memset(&client_addr, 0, sizeof(client_addr));
    client_addr.sin_family = AF_INET;
    client_addr.sin_port = htons(0); // 端口为0，由操作系统自动选择
    if (inet_pton(AF_INET, CLIENT_IP, &client_addr.sin_addr) <= 0) {
        die("[client] Invalid client IP address");
    }

    // 3. 绑定 socket 到客户端地址（可选，但为了匹配 Rust 代码的行为，我们这样做）
    if (bind(sockfd, (const struct sockaddr *)&client_addr, sizeof(client_addr)) < 0) {
        die("[client] Failed to bind to " CLIENT_IP);
    }
    
    // 4. 准备服务器地址结构（用于连接）
    memset(&server_addr, 0, sizeof(server_addr));
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(PORT);
    if (inet_pton(AF_INET, SERVER_IP, &server_addr.sin_addr) <= 0) {
        die("[client] Invalid server IP address for connect");
    }

    // 5. 连接到服务器（这会使 UDP socket 记住目标地址）
    if (connect(sockfd, (const struct sockaddr *)&server_addr, sizeof(server_addr)) < 0) {
        die("[client] Failed to connect");
    }

    // 6. 发送消息（因为已连接，可以使用 send 而不是 sendto）
    if (send(sockfd, msg, strlen(msg), 0) < 0) {
        die("[client] Failed to send");
    }
    printf("[client] Sent: %s\n", msg);

    // 7. 接收回显（因为已连接，可以使用 recv 而不是 recvfrom）
    ssize_t n = recv(sockfd, buffer, BUFFER_SIZE, 0);
    if (n < 0) {
        die("[client] Failed to receive");
    }
    buffer[n] = '\0'; // 确保字符串正确终止

    printf("[client] Received echo: %s\n", buffer);

    // 8. 验证消息是否匹配
    assert(strcmp(msg, buffer) == 0 && "[client] Mismatch in echo!");

    close(sockfd);
    printf("client goning to exit\n");
    return NULL;
}

int main() {
    pthread_t server_tid, client_tid;

    // 启动 server 线程
    if (pthread_create(&server_tid, NULL, server_func, NULL) != 0) {
        die("Failed to create server thread");
    }

    // 确保 server 已启动
    usleep(200 * 1000); // 200 milliseconds

    // 启动 client 线程
    if (pthread_create(&client_tid, NULL, client_func, NULL) != 0) {
        die("Failed to create client thread");
    }

    // 等待两个线程结束
    if (pthread_join(server_tid, NULL) != 0) {
        die("Failed to join server thread");
    }
    if (pthread_join(client_tid, NULL) != 0) {
        die("Failed to join client thread");
    }

    printf("\n✅ Test completed: veth0 <--> veth1 UDP communication success\n");

    return EXIT_SUCCESS;
}