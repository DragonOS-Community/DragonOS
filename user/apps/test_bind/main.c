#include <arpa/inet.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

#define PORT 12580
#define MAX_REQUEST_SIZE 1500
#define MAX_RESPONSE_SIZE 1500
#define EXIT_CODE 1
#define min(a, b) ((a) < (b) ? (a) : (b))

struct sockaddr_in address;
int addrlen = sizeof(address);
char buffer[MAX_REQUEST_SIZE] = {0};
int opt = 1;

void test_tcp_bind()
{
    int tcp_sk_fd1, tcp_sk_fd2, tcp_sk_fd3;
    
    // create tcp sockets
    if ((tcp_sk_fd1 = socket(AF_INET, SOCK_STREAM, 0)) == 0)
    {
        perror("tcp socket (1) failed");
        exit(EXIT_CODE);
    }
    if ((tcp_sk_fd2 = socket(AF_INET, SOCK_STREAM, 0)) == 0)
    {
        perror("tcp socket (2) failed");
        exit(EXIT_CODE);
    }
    if ((tcp_sk_fd3 = socket(AF_INET, SOCK_STREAM, 0)) == 0)
    {
        perror("tcp socket (3) failed");
        exit(EXIT_CODE);
    }
    
    // TEST tcp bind diff ports
    if (bind(tcp_sk_fd1, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("tcp bind (1) failed");
        exit(EXIT_CODE);
    }
    address.sin_port = htons(PORT+1);
    if (bind(tcp_sk_fd2, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("tcp bind (2) failed");
        exit(EXIT_CODE);
    }
    printf("===TEST 4 PASSED===\n");
    
    // TEST tcp bind same ports
    address.sin_port = htons(PORT);
    if (bind(tcp_sk_fd3, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("tcp bind (3) failed");
        // exit(EXIT_CODE);
    }
    printf("===TEST 5 PASSED===\n");
    
    if (close(tcp_sk_fd1) < 0)
    {
        perror("tcp close (1) failed");
        exit(EXIT_CODE);
    }
    if (close(tcp_sk_fd2) < 0)
    {
        perror("tcp close (2) failed");
        exit(EXIT_CODE);
    }
    if (close(tcp_sk_fd3) < 0)
    {
        perror("tcp close (3) failed");
        exit(EXIT_CODE);
    }
    printf("===TEST 6 PASSED===\n");
}

void test_udp_bind()
{
    int udp_sk_fd1, udp_sk_fd2, udp_sk_fd3;
    
    // create tcp sockets
    if ((udp_sk_fd1 = socket(AF_INET, SOCK_DGRAM, 0)) == 0)
    {
        perror("udp socket (1) failed");
        exit(EXIT_CODE);
    }
    if ((udp_sk_fd2 = socket(AF_INET, SOCK_DGRAM, 0)) == 0)
    {
        perror("udp socket (2) failed");
        exit(EXIT_CODE);
    }
    if ((udp_sk_fd3 = socket(AF_INET, SOCK_DGRAM, 0)) == 0)
    {
        perror("udp socket (3) failed");
        exit(EXIT_CODE);
    }
    
    // TEST udp bind diff ports
    if (bind(udp_sk_fd1, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("udp bind (1) failed");
        exit(EXIT_CODE);
    }
    address.sin_port = htons(PORT+1);
    if (bind(udp_sk_fd2, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("udp bind (2) failed");
        exit(EXIT_CODE);
    }
    printf("===TEST 7 PASSED===\n");
    
    // TEST udp bind same ports
    address.sin_port = htons(PORT);
    if (bind(udp_sk_fd3, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("udp bind (3) failed");
        // exit(EXIT_CODE);
    }
    printf("===TEST 8 PASSED===\n");
    
    if (close(udp_sk_fd1) < 0)
    {
        perror("udp close (1) failed");
        exit(EXIT_CODE);
    }
    if (close(udp_sk_fd2) < 0)
    {
        perror("udp close (2) failed");
        exit(EXIT_CODE);
    }
    if (close(udp_sk_fd3) < 0)
    {
        perror("udp close (3) failed");
        exit(EXIT_CODE);
    }
    printf("===TEST 9 PASSED===\n");
}

void test_all_ports() {
    int count = 0;
    
    while (1) {
    	int tcp_fd;
    	if ((tcp_fd = socket(AF_INET, SOCK_STREAM, 0)) == 0)
    	{
            perror("socket failed");
            exit(EXIT_CODE);
    	}
    	
    	address.sin_port = htons(0);
    	if (bind(tcp_fd, (struct sockaddr *)&address, sizeof(address)) < 0)
    	{
            perror("bind failed");
            // exit(EXIT_CODE);
            break;
    	}
    	
    	count++;
    }
    printf("===TEST 10===\n");
    printf("count: %d\n", count);
}

int main(int argc, char const *argv[])
{
    int server_fd;
    int udp_sk_fd;

    // 创建socket
    if ((server_fd = socket(AF_INET, SOCK_STREAM, 0)) == 0)
    {
        perror("tcp socket failed");
        exit(EXIT_CODE);
    }
    
    if ((udp_sk_fd = socket(AF_INET, SOCK_DGRAM, 0)) == 0)
    {
        perror("udp socket failed");
        exit(EXIT_CODE);
    }

    // 设置地址和端口
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = INADDR_ANY;
    address.sin_port = htons(PORT);

    // TEST socket's bind
    if (bind(server_fd, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("tcp bind failed");
        exit(EXIT_CODE);
    }
    address.sin_port = htons(PORT);
    if (bind(udp_sk_fd, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("udp bind failed");
        exit(EXIT_CODE);
    }
    printf("===TEST 1 PASSED===\n");

    // TEST socket's listen
    if (listen(server_fd, 3) < 0)
    {
        perror("listen failed");
        exit(EXIT_CODE);
    }
    printf("===TEST 2 PASSED===\n");

    // TEST socket's close
    if (close(server_fd) < 0)
    {
        perror("tcp close failed");
        exit(EXIT_CODE);
    }
    if (close(udp_sk_fd) < 0)
    {
        perror("udp close failed");
        exit(EXIT_CODE);
    }
    printf("===TEST 3 PASSED===\n");
    
    
    test_tcp_bind();
    test_udp_bind();
    test_all_ports();

    return 0;
}
