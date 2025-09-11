#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <sys/socket.h>
#include <sys/un.h>
#include <pthread.h>
#include <errno.h>

#define SOCKET_PATH "./test.stream"
#define SOCKET_ABSTRACT_PATH "/abs.stream"
#define MSG1 "Hello, unix stream socket from Client!"
#define MSG2 "Hello, unix stream socket from Server!"
#define BUFFER_SIZE 1024

// 创建Unix域套接字
int create_stream_socket() {
    int fd = socket(AF_UNIX, SOCK_STREAM, 0);
    if (fd == -1) {
        perror("socket");
        return -1;
    }
    printf("create socket success, fd=%d\n", fd);
    return fd;
}

// 绑定套接字到文件系统路径
int bind_socket(int fd) {
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, SOCKET_PATH, sizeof(addr.sun_path) - 1);
    
    if (bind(fd, (struct sockaddr*)&addr, sizeof(addr)) == -1) {
        perror("bind");
        return -1;
    }
    printf("bind_socket");
    return 0;
}

// 绑定套接字到抽象命名空间
int bind_abstract_socket(int fd) {
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    addr.sun_path[0] = '\0';  // 抽象命名空间以空字符开头
    strncpy(&addr.sun_path[1], SOCKET_ABSTRACT_PATH, sizeof(addr.sun_path) - 2);
    
    if (bind(fd, (struct sockaddr*)&addr, sizeof(addr)) == -1) {
        perror("bind abstract");
        return -1;
    }
    return 0;
}

// 监听套接字
int listen_socket(int fd) {
    if (listen(fd, 5) == -1) {
        perror("listen");
        return -1;
    }
    printf("Listen success!\n");
    return 0;
}

// 接受连接
int accept_conn(int fd) {
    int client_fd = accept(fd, NULL, NULL);
    if (client_fd == -1) {
        perror("accept");
        return -1;
    }
    return client_fd;
}

// 发送消息
int send_message(int fd, const char* msg) {
    if (send(fd, msg, strlen(msg), 0) == -1) {
        perror("send");
        return -1;
    }
    return 0;
}
// 接收消息
int recv_message(int fd, char* buffer, size_t buffer_size) {
    ssize_t len = read(fd, buffer, buffer_size - 1);
    if (len == -1) {
        perror("read");
        return -1;
    }
    buffer[len] = '\0';  // 确保字符串以空字符结尾
    return len;
}  

// 服务器线程函数
void* server_thread_func(void* arg) {
    int server_fd = *(int*)arg;
    
    int client_fd = accept_conn(server_fd);
    if (client_fd == -1) {
        return NULL;
    }
    
    printf("accept success!\n");
    
    char buffer[BUFFER_SIZE];
    if (recv_message(client_fd, buffer, BUFFER_SIZE) == -1) {
        close(client_fd);
        return NULL;
    }
    
    printf("Server: Received message: %s\n", buffer);
    
    if (send_message(client_fd, MSG2) == -1) {
        close(client_fd);
        return NULL;
    }
    
    printf("Server send finish\n");
    printf("Server begin close!\n");
    
    close(client_fd);
    close(server_fd);
    
    printf("Server close finish!\n");
    return NULL;
}

// 测试文件系统路径的Unix域套接字
int test_stream() {
    // 删除可能存在的套接字文件
    unlink(SOCKET_PATH);
    
    int server_fd = create_stream_socket();
    if (server_fd == -1) return -1;
    
    if (bind_socket(server_fd) == -1) {
        close(server_fd);
        return -1;
    }
    
    if (listen_socket(server_fd) == -1) {
        close(server_fd);
        return -1;
    }
    
    // 创建服务器线程
    pthread_t server_thread;
    if (pthread_create(&server_thread, NULL, server_thread_func, &server_fd) != 0) {
        perror("pthread_create");
        close(server_fd);
        return -1;
    }
    printf("accepting");
    
    // 等待一秒确保服务器启动
    sleep(1);
    
    // 客户端代码
    int client_fd = create_stream_socket();
    if (client_fd == -1) return -1;
    
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    strncpy(addr.sun_path, SOCKET_PATH, sizeof(addr.sun_path) - 1);
    
    if (connect(client_fd, (struct sockaddr*)&addr, sizeof(addr)) == -1) {
        perror("connect");
        close(client_fd);
        return -1;
    }
    
    if (send_message(client_fd, MSG1) == -1) {
        close(client_fd);
        return -1;
    }
    
    // 获取对等方名称
    struct sockaddr_un peer_addr;
    socklen_t peer_len = sizeof(peer_addr);
    if (getpeername(client_fd, (struct sockaddr*)&peer_addr, &peer_len) == -1) {
        perror("getpeername");
    } else {
        printf("Client: Connected to server at path: %s\n", peer_addr.sun_path);
    }
    
    // 等待服务器线程完成
    pthread_join(server_thread, NULL);
    
    printf("Client try recv!\n");
    char buffer[BUFFER_SIZE];
    if (recv_message(client_fd, buffer, BUFFER_SIZE) == -1) {
        close(client_fd);
        return -1;
    }
    
    printf("Client Received message: %s\n", buffer);
    
    close(client_fd);
    unlink(SOCKET_PATH);
    
    return 0;
}

// 抽象命名空间服务器线程函数
void* abstract_server_thread_func(void* arg) {
    int server_fd = *(int*)arg;
    
    int client_fd = accept_conn(server_fd);
    if (client_fd == -1) {
        return NULL;
    }
    
    printf("accept success!\n");
    
    char buffer[BUFFER_SIZE];
    if (recv_message(client_fd, buffer, BUFFER_SIZE) == -1) {
        close(client_fd);
        return NULL;
    }
    
    printf("Server: Received message: %s\n", buffer);
    
    if (send_message(client_fd, MSG2) == -1) {
        close(client_fd);
        return NULL;
    }
    
    printf("Server send finish\n");
    
    close(client_fd);
    close(server_fd);
    
    return NULL;
}

// 测试抽象命名空间
int test_abstract_namespace() {
    int server_fd = create_stream_socket();
    if (server_fd == -1) return -1;
    
    if (bind_abstract_socket(server_fd) == -1) {
        close(server_fd);
        return -1;
    }
    
    if (listen_socket(server_fd) == -1) {
        close(server_fd);
        return -1;
    }
    
    // 创建服务器线程
    pthread_t server_thread;
    if (pthread_create(&server_thread, NULL, abstract_server_thread_func, &server_fd) != 0) {
        perror("pthread_create");
        close(server_fd);
        return -1;
    }
    
    // 等待一秒确保服务器启动
    sleep(1);
    
    // 客户端代码
    int client_fd = create_stream_socket();
    if (client_fd == -1) return -1;
    
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    addr.sun_path[0] = '\0';  // 抽象命名空间
    strncpy(&addr.sun_path[1], SOCKET_ABSTRACT_PATH, sizeof(addr.sun_path) - 2);
    
    if (connect(client_fd, (struct sockaddr*)&addr, sizeof(addr)) == -1) {
        perror("connect");
        close(client_fd);
        return -1;
    }
    
    if (send_message(client_fd, MSG1) == -1) {
        close(client_fd);
        return -1;
    }
    
    // 获取对等方名称
    struct sockaddr_un peer_addr;
    socklen_t peer_len = sizeof(peer_addr);
    if (getpeername(client_fd, (struct sockaddr*)&peer_addr, &peer_len) == -1) {
        perror("getpeername");
    } else {
        printf("Client: Connected to server at abstract path\n");
    }
    
    // 等待服务器线程完成
    pthread_join(server_thread, NULL);
    
    printf("Client try recv!\n");
    char buffer[BUFFER_SIZE];
    if (recv_message(client_fd, buffer, BUFFER_SIZE) == -1) {
        close(client_fd);
        return -1;
    }
    
    printf("Client Received message: %s\n", buffer);
    
    close(client_fd);
    
    return 0;
}

// 测试资源释放
int test_resource_free() {
    int client_fd = create_stream_socket();
    if (client_fd == -1) return -1;
    
    struct sockaddr_un addr;
    memset(&addr, 0, sizeof(addr));
    addr.sun_family = AF_UNIX;
    addr.sun_path[0] = '\0';  // 抽象命名空间
    strncpy(&addr.sun_path[1], SOCKET_ABSTRACT_PATH, sizeof(addr.sun_path) - 2);
    
    if (connect(client_fd, (struct sockaddr*)&addr, sizeof(addr)) == -1) {
        perror("connect");
        close(client_fd);
        return -1;
    }
    
    if (send_message(client_fd, MSG1) == -1) {
        close(client_fd);
        return -1;
    }
    
    close(client_fd);
    return 0;
}

int main() {
    if (test_stream() == 0) {
        printf("test for unix stream success\n");
    } else {
        printf("test for unix stream failed\n");
    }
    
    if (test_abstract_namespace() == 0) {
        printf("test for unix abstract namespace success\n");
    } else {
        printf("test for unix abstract namespace failed\n");
    }
    
    if (test_resource_free() == 0) {
        printf("not free!\n");
    } else {
        printf("free!\n");
    }
    
    return 0;
}
