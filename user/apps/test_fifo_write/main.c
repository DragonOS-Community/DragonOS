#include <stdio.h>
#include <stdlib.h>
#include <fcntl.h>
#include <unistd.h>
#include <errno.h>
#include <sys/stat.h>

#define FIFO_PATH "/bin/test_fifo"

int main() {
    // 创建 FIFO
    if (mkfifo(FIFO_PATH, 0666) == -1 && errno != EEXIST) {
        perror("mkfifo failed");
        exit(EXIT_FAILURE);
    }

    printf("Opening FIFO in write mode...\n");

    // 尝试以非阻塞模式打开 FIFO 的写端
    int fd = open(FIFO_PATH, O_WRONLY|O_NONBLOCK);
    printf("fd: %d\n",fd);
    if (fd == -1) {
        if (errno == ENXIO) {
            printf("Error: No readers (ENXIO).\n");
        } else {
            perror("Failed to open FIFO");
        }
    } else {
        printf("FIFO opened successfully in write mode.\n");
        close(fd);
    }

    // 删除 FIFO
    unlink(FIFO_PATH);

    return 0;
}