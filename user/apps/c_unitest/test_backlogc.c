#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <netinet/in.h>
#include <errno.h>
#include <pthread.h>



int main()
{
    int sockfd;
    struct sockaddr_in server_addr;

    // 创建套接字
    sockfd = socket(AF_INET, SOCK_STREAM, 0);
    if (sockfd < 0)
    {
        perror("socket creation failed");
        return EXIT_FAILURE;
    }

    // 设置 SO_REUSEADDR 选项
    // int optval = 1;
    // if (setsockopt(sockfd, SOL_SOCKET, SO_REUSEADDR, &optval, sizeof(optval)) < 0) {
    //     perror("setsockopt(SO_REUSEADDR) failed");
    //     close(sockfd);
    //     exit(EXIT_FAILURE);
    // }

    // 配置服务器地址
    memset(&server_addr, 0, sizeof(server_addr));
    server_addr.sin_family = AF_INET;
    server_addr.sin_addr.s_addr = INADDR_ANY;
    server_addr.sin_port = htons(12580);

    // 绑定套接字
    if (bind(sockfd, (struct sockaddr *)&server_addr, sizeof(server_addr)) < 0)
    {
        perror("bind failed");
        close(sockfd);
        return EXIT_FAILURE;
    }

    // 调用 listen 系统调用
    if (listen(sockfd, 10) < 0)
    {
        perror("listen failed");
        close(sockfd);
        return EXIT_FAILURE;
    }

    printf("Listening on port 12580......\n");

    struct sockaddr_in client_addr;
    socklen_t client_len = sizeof(client_addr);
    char buffer[1024];
    int cnt = 0;

    while (cnt < 100)
    {
        int *client_sockfd = malloc(sizeof(int));
        if (!client_sockfd)
        {
            perror("malloc failed");
            continue;
        }

        // 接受客户端连接
        *client_sockfd = accept(sockfd, (struct sockaddr *)&client_addr, &client_len);
        if (*client_sockfd < 0)
        {
            perror("accept failed");
            free(client_sockfd);
            continue;
        }
        printf("the %dth connection\n", ++cnt);
        close(*client_sockfd);
    
    }

    // 关闭套接字
    close(sockfd);
    return EXIT_SUCCESS;
}
