#include <arpa/inet.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <unistd.h>

#define PORT 12580
#define MAX_REQUEST_SIZE 1500
#define MAX_RESPONSE_SIZE 1500
// 网页根目录
#define WEB_ROOT "/var/www/html/"
#define EXIT_CODE 1
#define min(a, b) ((a) < (b) ? (a) : (b))

#define DEFAULT_PAGE "/index.html"

int security_check(char *path)
{
    // 检查路径是否包含 ..
    if (strstr(path, ".."))
    {
        return 0;
    }
    return 1;
}

ssize_t send_response(int sockfd, char *response)
{
    return write(sockfd, response, strlen(response));
}

void send_header(int sockfd, int content_length, char *path)
{
    char buffer[MAX_RESPONSE_SIZE];
    // 获取文件类型
    char *content_type;
    if (strstr(path, ".html"))
    {
        content_type = "text/html";
    }
    else if (strstr(path, ".css"))
    {
        content_type = "text/css";
    }
    else if (strstr(path, ".js"))
    {
        content_type = "application/javascript";
    }
    else if (strstr(path, ".png"))
    {
        content_type = "image/png";
    }
    else if (strstr(path, ".jpg"))
    {
        content_type = "image/jpeg";
    }
    else if (strstr(path, ".gif"))
    {
        content_type = "image/gif";
    }
    else
    {
        content_type = "text/plain;charset=utf-8";
    }
    sprintf(buffer, "HTTP/1.1 200 OK\nContent-Type: %s\nContent-Length: %d\n\n", content_type, content_length);
    send_response(sockfd, buffer);
}

void send_file(int sockfd, char *path)
{
    printf("send_file: path: %s\n", path);

    int fd = open(path, 0);
    if (fd == -1)
    {
        send_response(
            sockfd,
            "HTTP/1.1 404 Not Found\nContent-Type: text/html\n\n<html><body><h1>404 Not Found</h1><p>DragonOS Http Server</p></body></html>");
        return;
    }

    int content_length = lseek(fd, 0, SEEK_END);
    int remaining = content_length;
    printf("send_file: content_length: %d\n", content_length);
    lseek(fd, 0, SEEK_SET);
    send_header(sockfd, content_length, path);

    char buffer[1048576];
    int readSize;
    while (remaining)
    {
        // 由于磁盘IO耗时较长，所以每次读取1MB，然后再分批发送
        int to_read = min(1048576, remaining);
        readSize = read(fd, &buffer, to_read);

        remaining -= readSize;
        void *p = buffer;
        while (readSize > 0)
        {
            int wsize = write(sockfd, p, min(readSize, MAX_RESPONSE_SIZE));
            if (wsize <= 0)
            {
                printf("send_file failed: wsize: %d\n", wsize);
                close(fd);
                return;
            }
            p += wsize;
            readSize -= wsize;
        }
    }

    close(fd);
}

void handle_request(int sockfd, char *request)
{
    char *method, *url, *http_version;
    char path[MAX_REQUEST_SIZE];

    method = strtok(request, " ");
    url = strtok(NULL, " ");
    http_version = strtok(NULL, "\r\n");
    
    printf("handle_request: method: %s, url: %s, http_version: %s\n", method, url, http_version);
    // 检查空指针等异常情况
    if (method == NULL || url == NULL || http_version == NULL)
    {
        send_response(sockfd, "HTTP/1.1 400 Bad Request\nContent-Type: text/html\n\n<html><body><h1>400 Bad "
                              "Request</h1><p>DragonOS Http Server</p></body></html>");
        return;
    }
    // 检查url是否为空
    if (strlen(url) == 0)
    {
        send_response(sockfd, "HTTP/1.1 400 Bad Request\nContent-Type: text/html\n\n<html><body><h1>400 Bad "
                              "Request</h1><p>DragonOS Http Server</p></body></html>");
        return;
    }
    int default_page = 0;
    if (url[strlen(url) - 1] == '/')
    {
        default_page = 1;
    }

    if (strcmp(method, "GET") == 0)
    {
        if (default_page)
        {
            sprintf(path, "%s%s%s", WEB_ROOT, url, DEFAULT_PAGE);
        }
        else
        {
            sprintf(path, "%s%s", WEB_ROOT, url);
        }
        if (!security_check(path))
        {
            send_response(
                sockfd,
                "HTTP/1.1 403 Forbidden\nContent-Type: text/html\n\n<html><body><h1>403 Forbidden</h1><p>DragonOS Http Server</p></body></html>");
            return;
        }
        send_file(sockfd, path);
    }
    else
    {
        send_response(sockfd, "HTTP/1.1 501 Not Implemented\nContent-Type: text/html\n\n<html><body><h1>501 Not "
                              "Implemented</h1><p>DragonOS Http Server</p></body></html>");
    }
}

int main(int argc, char const *argv[])
{
    int server_fd, new_socket, valread;
    struct sockaddr_in address;
    int addrlen = sizeof(address);
    char buffer[MAX_REQUEST_SIZE] = {0};
    int opt = 1;

    // 创建socket
    if ((server_fd = socket(AF_INET, SOCK_STREAM, 0)) == 0)
    {
        perror("socket failed");
        exit(EXIT_CODE);
    }

    // 设置socket选项，允许地址重用
    // if (setsockopt(server_fd, SOL_SOCKET, SO_REUSEADDR | SO_REUSEPORT, &opt, sizeof(opt)))
    // {
    //     perror("setsockopt failed");
    //     exit(EXIT_CODE);
    // }

    // 设置地址和端口
    address.sin_family = AF_INET;
    address.sin_addr.s_addr = INADDR_ANY;
    address.sin_port = htons(PORT);

    // 把socket绑定到地址和端口上
    if (bind(server_fd, (struct sockaddr *)&address, sizeof(address)) < 0)
    {
        perror("bind failed");
        exit(EXIT_CODE);
    }

    // 监听socket
    if (listen(server_fd, 3) < 0)
    {
        perror("listen failed");
        exit(EXIT_CODE);
    }

    while (1)
    {
        printf("Waiting for a client...\n");

        // 等待并接受客户端连接
        if ((new_socket = accept(server_fd, (struct sockaddr *)&address, (socklen_t *)&addrlen)) < 0)
        {
            perror("accept failed");
            exit(EXIT_CODE);
        }

        // 接收客户端消息
        valread = read(new_socket, buffer, MAX_REQUEST_SIZE);
        printf("%s\n", buffer);

        // 处理请求
        handle_request(new_socket, buffer);

        // 关闭客户端连接
        close(new_socket);
    }
    // 关闭tcp socket
    close(server_fd);

    return 0;
}