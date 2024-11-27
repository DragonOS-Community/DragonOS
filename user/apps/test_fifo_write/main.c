#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/wait.h>

#define FIFO_PATH "/bin/test_fifo" // 使用 /tmp 目录避免权限问题

// 信号处理函数
void sigpipe_handler(int signo) {
    if (signo == SIGPIPE) {
        printf("Received SIGPIPE signal. Write operation failed.\n");
    }
}

void test_fifo_write(const char *scenario_desc, int nonblocking) {
    int fd;
    char *data = "Hello, FIFO!";
    printf("\n--- Testing: %s (nonblocking=%d) ---\n", scenario_desc, nonblocking);

    // 设置写模式和非阻塞模式标志
    int flags = O_WRONLY;
    if (nonblocking) {
        flags |= O_NONBLOCK;
    }

    // 打开 FIFO 写端
    fd = open(FIFO_PATH, flags);
    if (fd == -1) {
        if (errno == ENXIO) {
            printf("Result: Failed to open FIFO for writing (ENXIO: No readers).\n");
        } else {
            perror("Failed to open FIFO for writing");
        }
        return;
    }

    // 写入数据
    ssize_t bytes_written = write(fd, data, sizeof(data));
    if (bytes_written == -1) {
        if (errno == EPIPE) {
            printf("Result: Write failed with EPIPE (no readers available).\n");
        } else if (errno == ENXIO) {
            printf("Result: Write failed with ENXIO (FIFO never had readers).\n");
        } else if (errno == EAGAIN) {
            printf("Result: Write failed with EAGAIN (nonblocking write, pipe full or no readers).\n");
        } else {
            perror("Write failed with an unexpected error");
        }
    } else {
        printf("Result: Write succeeded. Bytes written: %zd\n", bytes_written);
    }

    // 关闭 FIFO 写端
    close(fd);
}

void test_case1(int nonblocking) {
    // Case 1: No readers (FIFO never had readers)
    test_fifo_write("No readers (FIFO never had readers)", nonblocking);
}

void test_case2(int nonblocking) {
    pid_t reader_pid;

    // Case 2: Reader exists but disconnects
    reader_pid = fork();
    if (reader_pid == 0) {
        // 子进程充当读端
        int reader_fd = open(FIFO_PATH, O_RDONLY);
        if (reader_fd == -1) {
            perror("Reader failed to open FIFO");
            exit(EXIT_FAILURE);
        }
        sleep(1); // 模拟读端短暂存在
        close(reader_fd);
        exit(EXIT_SUCCESS);
    }

    sleep(5); // 确保读端已打开
    test_fifo_write("Reader exists but disconnects", nonblocking);
    waitpid(reader_pid, NULL, 0); // 等待读端子进程退出
}

void test_case3(int nonblocking) {
    pid_t reader_pid;

    // Case 3: Active reader exists
    reader_pid = fork();
    if (reader_pid == 0) {
        // 子进程充当读端
        int reader_fd = open(FIFO_PATH, O_RDONLY);
        if (reader_fd == -1) {
            perror("Reader failed to open FIFO");
            exit(EXIT_FAILURE);
        }
        sleep(5); // 保持读端存在
        close(reader_fd);
        exit(EXIT_SUCCESS);
    }

    sleep(1); // 确保读端已打开
    test_fifo_write("Active reader exists", nonblocking);
    waitpid(reader_pid, NULL, 0); // 等待读端子进程退出
}

int main() {
    // 设置 SIGPIPE 信号处理
    signal(SIGPIPE, sigpipe_handler);

    // 创建 FIFO
    if (mkfifo(FIFO_PATH, 0666) == -1 && errno != EEXIST) {
        perror("mkfifo failed");
        exit(EXIT_FAILURE);
    }

    // 测试阻塞模式下的三种情况
    printf("========== Testing Blocking Mode ==========\n");
    test_case1(0); // 阻塞模式下没有读端
    test_case2(0); // 阻塞模式下读端断开
    test_case3(0); // 阻塞模式下读端存在

    // 测试非阻塞模式下的三种情况
    // printf("\n========== Testing Nonblocking Mode ==========\n");
    // test_case1(1); // 非阻塞模式下没有读端
    // test_case2(1); // 非阻塞模式下读端断开
    // test_case3(1); // 非阻塞模式下读端存在

    // 删除 FIFO
    unlink(FIFO_PATH);

    printf("\nAll tests completed.\n");
    return 0;
}