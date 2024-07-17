#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <arpa/inet.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <poll.h>
#include <fcntl.h>

#define MAX_CLIENTS 10
#define BUFFER_SIZE 1024

int main() {
    int server_fd, client_fds[MAX_CLIENTS];
    struct sockaddr_in server_address, client_address;
    socklen_t client_address_length;
    char buffer[BUFFER_SIZE];

    // 创建服务器套接字
    server_fd = socket(AF_INET, SOCK_STREAM, 0);
    if (server_fd == -1) {
        perror("socket");
        exit(EXIT_FAILURE);
    }
    
    // if ((server_fd == 0))
    // {
    //     perror("socket failed");
    //     exit(EXIT_FAILURE);
    // }
    // printf("server _fd=%d",server_fd);
    // 设置服务器地址
    memset(&server_address, 0, sizeof(server_address));
    server_address.sin_family = AF_INET;
    server_address.sin_addr.s_addr = INADDR_ANY;
    server_address.sin_port = htons(12580);

    // 绑定服务器地址
    if (bind(server_fd, (struct sockaddr*)&server_address, sizeof(server_address)) == -1) {
        perror("bind");
        exit(EXIT_FAILURE);
    }

    // 监听连接请求
    if (listen(server_fd, MAX_CLIENTS) == -1) {
        perror("listen");
        exit(EXIT_FAILURE);
    }

    // 初始化客户端连接数组
    memset(client_fds, 0, sizeof(client_fds));

    // 创建 pollfd 结构数组
    struct pollfd fds[MAX_CLIENTS + 1];
    fds[0].fd = server_fd;
    fds[0].events = POLLIN;

    printf("Server started. Waiting for connections...\n");
    
    while (1) {
        printf("server_fd=%d,fds.events=%d,fds.revents=%d\n",fds[0].fd,fds[0].events,fds[0].revents);
        printf("client_fds[1]=%d,fds.revents[1]=%d\n",client_fds[1],fds[1].revents);
        fds[0].revents=0; 
        // 调用 poll 等待事件
        int ret = poll(fds, MAX_CLIENTS + 1, -1);
        printf("After poll server_fd=%d,fds.events=%d,fds.revents=%d\n",fds[0].fd,fds[0].events,fds[0].revents);
        printf("After poll client_fds[1]=%d,fds[1].revents=%d\n",client_fds[1],fds[1].revents);
        printf("ret=%d\n",ret);
        // printf("server_fd=%d,fds.revents=%d\n",fds[0],fds[0].revents);

        // ret=1;
        // if(ret!=1){
        //     printf("ret=%d",ret);
        // }
        if (ret == -1) {
            perror("poll");
            exit(EXIT_FAILURE);
        }
        // printf("fds0.revents=%d\n",fds[0].revents);
        
        // 检查服务器套接字是否有新连接请求
        if (fds[0].revents & POLLIN) { 
            printf("serverPollIN\n");
            client_address_length = sizeof(client_address);
            int client_fd = accept(server_fd, (struct sockaddr*)&client_address, &client_address_length);
            printf("client_fd=%d\n",client_fd);
            if (client_fd == -1) {
                perror("accept");
                exit(EXIT_FAILURE);
            }

            // 将新连接添加到客户端连接数组
            int i=0;
            for (i = 1; i <= MAX_CLIENTS; i++) {
                if (client_fds[i] == 0) {
                    client_fds[i] = client_fd;printf("i=%d",i);
                    break;
                }
            }
            // 将新连接添加到 pollfd 结构数组
            fds[i].fd = client_fd;
            fds[i].events = POLLIN;
            
            printf("New client connected: %s:%d\n", inet_ntoa(client_address.sin_addr), ntohs(client_address.sin_port));
        }else{
            // printf("no\n");
        }

        // 检查客户端连接是否有数据可读
        for (int i = 1; i <= MAX_CLIENTS; i++) {
           
            if (client_fds[i] > 0 && (fds[i].revents & POLLIN)) {
                int valread = read(client_fds[i], buffer, BUFFER_SIZE);
                if (valread == 0) {
                    // 客户端关闭连接
                    close(client_fds[i]);
                    client_fds[i] = 0;
                    fds[i].fd = -1;
                    printf("Client disconnected\n");
                } else {
                    // 处理客户端发送的数据
                    buffer[valread] = '\0';
                    printf("Received from client: %s\n", buffer);
                }
            }
             printf("In Read: client_fds[%d]=%d,fds.revents=%d\n",i, client_fds[i],fds[i].revents);
        }
    }

    // 关闭服务器套接字
    close(server_fd);

    return 0;
}