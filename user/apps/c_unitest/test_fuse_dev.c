/**
 * @file test_fuse_dev.c
 * @brief Phase A unit test: /dev/fuse basic semantics (open/read nonblock)
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

static int test_nonblock_read_empty(void) {
    int fd = open("/dev/fuse", O_RDWR | O_NONBLOCK);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    unsigned char buf[256];
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n != -1 || (errno != EAGAIN && errno != EWOULDBLOCK)) {
        printf("[FAIL] nonblock read empty: n=%zd errno=%d (%s)\n", n, errno, strerror(errno));
        close(fd);
        return -1;
    }

    struct pollfd pfd;
    pfd.fd = fd;
    pfd.events = POLLIN;
    int pr = poll(&pfd, 1, 100 /*ms*/);
    if (pr != 0) {
        printf("[FAIL] poll empty expected timeout: pr=%d revents=%x errno=%d (%s)\n",
               pr, pfd.revents, errno, strerror(errno));
        close(fd);
        return -1;
    }

    close(fd);
    printf("[PASS] nonblock_read_empty\n");
    return 0;
}

int main(void) {
    if (test_nonblock_read_empty() != 0) {
        return 1;
    }
    return 0;
}

