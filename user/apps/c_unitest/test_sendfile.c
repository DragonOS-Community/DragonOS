#define _GNU_SOURCE
#include <fcntl.h>
#include <sys/sendfile.h>
#include <sys/stat.h>
#include <unistd.h>
#include <stdio.h>
#include <stdlib.h>

int main(int argc, char *argv[]) {
    if (argc != 3) {
        fprintf(stderr, "用法: %s <源文件> <目标文件>\n", argv[0]);
        return 1;
    }

    const char *src_path = argv[1];
    const char *dst_path = argv[2];

    int src_fd = open(src_path, O_RDONLY);
    if (src_fd < 0) {
        perror("打开源文件失败");
        return 1;
    }

    int dst_fd = open(dst_path, O_WRONLY | O_CREAT | O_TRUNC, 0644);
    if (dst_fd < 0) {
        perror("打开目标文件失败");
        close(src_fd);
        return 1;
    }

    struct stat stat_buf;
    if (fstat(src_fd, &stat_buf) < 0) {
        perror("fstat失败");
        close(src_fd);
        close(dst_fd);
        return 1;
    }

    off_t offset = 0;
    ssize_t sent = sendfile(dst_fd, src_fd, &offset, stat_buf.st_size);
    if (sent < 0) {
        perror("sendfile失败");
        close(src_fd);
        close(dst_fd);
        return 1;
    }

    printf("成功复制 %zd 字节，从 %s 到 %s\n", sent, src_path, dst_path);

    close(src_fd);
    close(dst_fd);
    return 0;
}
