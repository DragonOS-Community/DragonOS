#include <arpa/inet.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CONN_QUEUE_SIZE 20
#define BUFFER_SIZE 1024
#define SERVER_PORT 12580

int server_sockfd;
int conn;

void signal_handler(int signo)
{

    printf("Server is exiting...\n");
    close(conn);
    close(server_sockfd);
    exit(0);
}

static char logo[] =
    " ____                                      ___   ____ \n|  _ \\  _ __   __ _   __ _   ___   _ __   / _ \\ / ___| "
    "\n| | | || '__| / _` | / _` | / _ \\ | '_ \\ | | | |\\___ \\  \n| |_| || |   | (_| || (_| || (_) || | | || |_| | "
    "___) |\n|____/ |_|    \\__,_| \\__, | \\___/ |_| |_| \\___/ |____/ \n                     |___/     \n";

void tcp_server()
{
    printf("TCP Server is running...\n");
    server_sockfd = socket(AF_INET, SOCK_STREAM, 0);
    printf("socket() ok, server_sockfd=%d\n", server_sockfd);
    struct sockaddr_in server_sockaddr;
    server_sockaddr.sin_family = AF_INET;
    server_sockaddr.sin_port = htons(SERVER_PORT);
    server_sockaddr.sin_addr.s_addr = htonl(INADDR_ANY);

    if (bind(server_sockfd, (struct sockaddr *)&server_sockaddr, sizeof(server_sockaddr)))
    {
        perror("Server bind error.\n");
        exit(1);
    }

    printf("TCP Server is listening...\n");
    if (listen(server_sockfd, CONN_QUEUE_SIZE) == -1)
    {
        perror("Server listen error.\n");
        exit(1);
    }

    printf("listen() ok\n");

    char buffer[BUFFER_SIZE];
    struct sockaddr_in client_addr;
    socklen_t client_length = sizeof(client_addr);
    /*
        Await a connection on socket FD.
        When a connection arrives, open a new socket to communicate with it,
        set *ADDR (which is *ADDR_LEN bytes long) to the address of the connecting
        peer and *ADDR_LEN to the address's actual length, and return the
        new socket's descriptor, or -1 for errors.
     */
    conn = accept(server_sockfd, (struct sockaddr *)&client_addr, &client_length);
    printf("Connection established.\n");
    if (conn < 0)
    {
        printf("Create connection failed, code=%d\n", conn);
        exit(1);
    }
    send(conn, logo, sizeof(logo), 0);
    while (1)
    {
        memset(buffer, 0, sizeof(buffer));
        int len = recv(conn, buffer, sizeof(buffer), 0);
        if (len <= 0)
        {
            printf("Receive data failed! len=%d\n", len);
            break;
        }
        if (strcmp(buffer, "exit\n") == 0)
        {
            break;
        }

        printf("Received: %s\n", buffer);
        send(conn, buffer, len, 0);
    }
    close(conn);
    close(server_sockfd);
}

void udp_server()
{
    printf("UDP Server is running...\n");
    server_sockfd = socket(AF_INET, SOCK_DGRAM, 0);
    printf("socket() ok, server_sockfd=%d\n", server_sockfd);
    struct sockaddr_in server_sockaddr;
    server_sockaddr.sin_family = AF_INET;
    server_sockaddr.sin_port = htons(SERVER_PORT);
    server_sockaddr.sin_addr.s_addr = htonl(INADDR_ANY);

    if (bind(server_sockfd, (struct sockaddr *)&server_sockaddr, sizeof(server_sockaddr)))
    {
        perror("Server bind error.\n");
        exit(1);
    }

    printf("UDP Server is listening...\n");

    char buffer[BUFFER_SIZE];
    struct sockaddr_in client_addr;
    socklen_t client_length = sizeof(client_addr);

    while (1)
    {
        memset(buffer, 0, sizeof(buffer));
        int len = recvfrom(server_sockfd, buffer, sizeof(buffer), 0, (struct sockaddr *)&client_addr, &client_length);
        if (len <= 0)
        {
            printf("Receive data failed! len=%d", len);
            break;
        }
        if (strcmp(buffer, "exit\n") == 0)
        {
            break;
        }

        printf("Received: %s", buffer);
        sendto(server_sockfd, buffer, len, 0, (struct sockaddr *)&client_addr, client_length);
        printf("Send: %s", buffer);
    }
    close(conn);
    close(server_sockfd);
}

void tcp_client()
{
    printf("Client is running...\n");
    int client_sockfd = socket(AF_INET, SOCK_STREAM, 0);

    struct sockaddr_in server_addr = {0};
    server_addr.sin_family = AF_INET;
    server_addr.sin_port = htons(12581);
    server_addr.sin_addr.s_addr = inet_addr("192.168.199.129");
    printf("to connect\n");
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
        sendbuf[0] = 'a';

        // printf("to send\n");
        send(client_sockfd, sendbuf, strlen(sendbuf), 0);
        // printf("send ok\n");
        if (strcmp(sendbuf, "exit\n") == 0)
        {
            break;
        }

        int x = recv(client_sockfd, recvbuf, sizeof(recvbuf), 0);
        if (x < 0)
        {
            printf("recv error, retval=%d\n", x);
            break;
        }

        fputs(recvbuf, stdout);

        memset(recvbuf, 0, sizeof(recvbuf));
        memset(sendbuf, 0, sizeof(sendbuf));
    }
    close(client_sockfd);
}

void udp_client()
{
    struct sockaddr_in addr;
    int sockfd, len = 0;
    int addr_len = sizeof(struct sockaddr_in);
    char buffer[256];

    /* 建立socket，注意必须是SOCK_DGRAM */
    if ((sockfd = socket(AF_INET, SOCK_DGRAM, 0)) < 0)
    {
        perror("socket");
        exit(1);
    }

    /* 填写sockaddr_in*/
    bzero(&addr, sizeof(addr));
    addr.sin_family = AF_INET;
    addr.sin_port = htons(12581);
    addr.sin_addr.s_addr = inet_addr("192.168.199.129");

    printf("to send logo\n");
    sendto(sockfd, logo, sizeof(logo), 0, (struct sockaddr *)&addr, addr_len);
    printf("send logo ok\n");
    while (1)
    {
        bzero(buffer, sizeof(buffer));

        printf("Please enter a string to send to server: \n");

        /* 从标准输入设备取得字符串*/
        len = read(STDIN_FILENO, buffer, sizeof(buffer));
        printf("to send: %d\n", len);
        /* 将字符串传送给server端*/
        sendto(sockfd, buffer, len, 0, (struct sockaddr *)&addr, addr_len);

        /* 接收server端返回的字符串*/
        len = recvfrom(sockfd, buffer, sizeof(buffer), 0, (struct sockaddr *)&addr, &addr_len);
        printf("Receive from server: %s\n", buffer);
    }

    return 0;
}
void main()
{
    // signal(SIGKILL, signal_handler);
    // signal(SIGINT, signal_handler);
    tcp_server();
    // udp_server();
    // tcp_client();
    // udp_client();
}