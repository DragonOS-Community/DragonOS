#include <arpa/inet.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#define CONN_QUEUE_SIZE 20
#define BUFFER_SIZE 1024
#define SERVER_PORT 6970

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
        int len = recvfrom(server_sockfd, buffer, sizeof(buffer), 0, (struct sockaddr*)&client_addr, &client_length);
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
        sendto(server_sockfd, buffer, len, 0, (struct sockaddr*)&client_addr,client_length);
        printf("Send: %s", buffer);
    }
    close(conn);
    close(server_sockfd);
}
void main()
{
    // signal(SIGKILL, signal_handler);
    // signal(SIGINT, signal_handler);
    udp_server();
}