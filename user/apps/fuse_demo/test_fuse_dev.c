/**
 * @file test_fuse_dev.c
 * @brief Phase P0 unit test: /dev/fuse read buffer and nonblock semantics.
 */

#include "fuse_test_simplefs.h"

static int test_nonblock_read_empty(void) {
    int fd = open("/dev/fuse", O_RDWR | O_NONBLOCK);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    unsigned char small[FUSE_MIN_READ_BUFFER / 2];
    ssize_t n = read(fd, small, sizeof(small));
    if (n != -1 || errno != EINVAL) {
        printf("[FAIL] nonblock read with small buffer: n=%zd errno=%d (%s)\n", n, errno,
               strerror(errno));
        close(fd);
        return -1;
    }

    unsigned char *big = malloc(FUSE_TEST_BUF_SIZE);
    if (!big) {
        printf("[FAIL] malloc big buffer failed\n");
        close(fd);
        return -1;
    }
    memset(big, 0, FUSE_TEST_BUF_SIZE);

    n = read(fd, big, FUSE_TEST_BUF_SIZE);
    if (n != -1 || (errno != EAGAIN && errno != EWOULDBLOCK)) {
        printf("[FAIL] nonblock read empty: n=%zd errno=%d (%s)\n", n, errno, strerror(errno));
        free(big);
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
        free(big);
        close(fd);
        return -1;
    }

    free(big);
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
