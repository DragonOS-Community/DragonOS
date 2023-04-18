#include <arpa/inet.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define BUFFER_SIZE 1024
#define SERVER_PORT 6970

int main()
{
    printf("Client is running...\n");
    int client_sockfd = socket(AF_INET, SOCK_STREAM, 0);

    struct sockaddr_in server_addr = {0};
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(SERVER_PORT);
    server_addr.sin_addr.s_addr = inet_addr("127.0.0.1");

    if (connect(client_sockfd, (struct sockaddr *)&server_addr, sizeof(server_addr)) < 0)
    {
        perror("Failed to establish connection to server\n");
        exit(1);
    }
    printf("connected to server\n");

    char sendbuf[BUFFER_SIZE] = {0};
    char recvbuf[BUFFER_SIZE] = {0};

    int x = recv(client_sockfd, recvbuf, sizeof(recvbuf), 0);

    fputs(recvbuf, stdout);

    memset(recvbuf, 0, sizeof(recvbuf));

    while (1)
    {
        fgets(sendbuf, sizeof(sendbuf), stdin);

        // printf("to send\n");
        send(client_sockfd, sendbuf, strlen(sendbuf), 0);
        // printf("send ok\n");
        if (strcmp(sendbuf, "exit\n") == 0)
        {
            break;
        }

        int x = recv(client_sockfd, recvbuf, sizeof(recvbuf), 0);

        fputs(recvbuf, stdout);

        memset(recvbuf, 0, sizeof(recvbuf));
        memset(sendbuf, 0, sizeof(sendbuf));
    }
    close(client_sockfd);
}