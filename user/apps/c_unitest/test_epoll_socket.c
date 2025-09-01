#include <arpa/inet.h>
#include <assert.h>
#include <errno.h>
#include <fcntl.h>
#include <netinet/in.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <unistd.h>

#define SERVER_IP "111.111.11.1"
#define CLIENT_IP "111.111.11.2"
#define PORT 8888
#define MAX_EVENTS 10
#define BUFFER_SIZE 1024

// 函数声明
void server_process();
void client_process();

int main() {
    pid_t pid = fork();

    if (pid < 0) {
        perror("fork failed");
        exit(EXIT_FAILURE);
    } else if (pid == 0) {
        // 子进程作为客户端
        // 等待一秒，确保服务器已启动
        sleep(1);
        client_process();
    } else {
        // 父进程作为服务器
        server_process();
    }

    return 0;
}


// 服务器进程逻辑
void server_process() {
    printf("[Server] Starting server process...\n");

    int listen_sock, conn_sock, epoll_fd;
    struct sockaddr_in server_addr, client_addr;
    socklen_t client_len = sizeof(client_addr);
    struct epoll_event ev, events[MAX_EVENTS];
    char buffer[BUFFER_SIZE];
    int data_processed = 0; // 添加标志位，标记是否已处理过数据

    if ((listen_sock = socket(AF_INET, SOCK_STREAM | SOCK_NONBLOCK, 0)) < 0) {
        perror("[Server] socket creation failed");
        exit(EXIT_FAILURE);
    }

    memset(&server_addr, 0, sizeof(server_addr));
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(PORT);
    if (inet_pton(AF_INET, SERVER_IP, &server_addr.sin_addr) <= 0) {
        perror("[Server] inet_pton failed");
        exit(EXIT_FAILURE);
    }

    if (bind(listen_sock,
             (struct sockaddr *)&server_addr,
             sizeof(server_addr)) < 0) {
        perror("[Server] bind failed");
        exit(EXIT_FAILURE);
    }

    if (listen(listen_sock, 1) < 0) {
        perror("[Server] listen failed");
        exit(EXIT_FAILURE);
    }
    printf("[Server] Listening on %s:%d\n", SERVER_IP, PORT);

    if ((epoll_fd = epoll_create1(0)) < 0) {
        perror("[Server] epoll_create1 failed");
        exit(EXIT_FAILURE);
    }

    ev.events = EPOLLIN;
    ev.data.fd = listen_sock;
    if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, listen_sock, &ev) < 0) {
        perror("[Server] epoll_ctl: listen_sock failed");
        exit(EXIT_FAILURE);
    }
    printf("Adding listening socket %d to epoll\n", listen_sock);

    while (!data_processed) { 
        int nfds = epoll_wait(epoll_fd, events, MAX_EVENTS, -1);
        if (nfds < 0) {
            perror("[Server] epoll_wait failed");
            exit(EXIT_FAILURE);
        }
        printf("Fuck epoll_wait returned %d\n", nfds);

        for (int n = 0; n < nfds; ++n) {
            if (events[n].data.fd == listen_sock) {
                printf("trying to accept new connection...\n");
                while (1) {
                    conn_sock = accept(listen_sock,
                                       (struct sockaddr *)&client_addr,
                                       &client_len);
                    if (conn_sock < 0) {
                        if (errno == EAGAIN || errno == EWOULDBLOCK) {
                            printf("All incoming connections have been "
                                   "processed.\n");
                            break;
                        } else {
                            perror("accept error");
                            exit(EXIT_FAILURE);
                            break;
                        }
                    }
                    ev.events = EPOLLIN | EPOLLET; 
                    ev.data.fd = conn_sock;
                    if (epoll_ctl(epoll_fd, EPOLL_CTL_ADD, conn_sock, &ev) <
                        0) {
                        perror("[Server] epoll_ctl: conn_sock failed");
                        exit(EXIT_FAILURE);
                    }
                    printf("[Server] Accepted connection from %s:%d\n",
                           inet_ntoa(client_addr.sin_addr),
                           ntohs(client_addr.sin_port));
                }
            } else {
                printf("[Server] handling client data...\n");
                int client_fd = events[n].data.fd;
                int nread = read(client_fd, buffer, BUFFER_SIZE);
                if (nread == -1) {
                    if (errno != EAGAIN) {
                        perror("[Server] read error");
                        close(client_fd);
                    }
                } else if (nread == 0) {
                    printf("[Server] Client disconnected.\n");
                    close(client_fd);
                } else {
                    buffer[nread] = '\0';
                    printf("[Server] Received from client: %s\n", buffer);
                    write(client_fd, buffer, nread);
                    printf("[Server] Echoed data back to client. Server will "
                           "now exit.\n");
                    data_processed = 1; // 设置退出标志
                    sleep(3);

                    close(client_fd);
                    break; 
                }
            }
        }
    }

    printf("[Server] Server process completed.\n");
    close(listen_sock);
    close(epoll_fd);
}

// 客户端进程逻辑
void client_process() {
    printf("[Client] Starting client process...\n");

    int sock = 0;
    struct sockaddr_in client_bind_addr, server_addr;
    char buffer[BUFFER_SIZE] = {0};

    if ((sock = socket(AF_INET, SOCK_STREAM, 0)) < 0) {
        perror("[Client] socket creation failed");
        exit(EXIT_FAILURE);
    }

    memset(&client_bind_addr, 0, sizeof(client_bind_addr));
    client_bind_addr.sin_family = AF_INET;
    client_bind_addr.sin_port = htons(7777);
    if (inet_pton(AF_INET, CLIENT_IP, &client_bind_addr.sin_addr) <= 0) {
        perror("[Client] inet_pton for bind failed");
        exit(EXIT_FAILURE);
    }

    if (bind(sock,
             (struct sockaddr *)&client_bind_addr,
             sizeof(client_bind_addr)) < 0) {
        perror("[Client] bind failed");
        exit(EXIT_FAILURE);
    }
    printf("[Client] Bound to IP %s\n", CLIENT_IP);


    memset(&server_addr, 0, sizeof(server_addr));
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(PORT);
    if (inet_pton(AF_INET, SERVER_IP, &server_addr.sin_addr) <= 0) {
        perror("[Client] inet_pton for connect failed");
        exit(EXIT_FAILURE);
    }

    if (connect(sock, (struct sockaddr *)&server_addr, sizeof(server_addr)) <
        0) {
        perror("[Client] connect failed");
        exit(EXIT_FAILURE);
    }
    printf("[Client] Connected to server %s:%d\n", SERVER_IP, PORT);


    const char *message = "Hello from client";
    write(sock, message, strlen(message));
    printf("[Client] Sent: %s\n", message);
    sleep(1);

    int valread = read(sock, buffer, BUFFER_SIZE);
    if (valread > 0) {
        buffer[valread] = '\0';
        printf("[Client] Received: %s\n", buffer);
    }

    printf("[Client] Client process completed.\n");
    close(sock);
}