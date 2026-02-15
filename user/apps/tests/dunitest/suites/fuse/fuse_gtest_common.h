#pragma once

#include <poll.h>

#include "fuse_test_simplefs_local.h"

static inline int fuseg_wait_flag(volatile int *flag, int retries, int sleep_us) {
    for (int i = 0; i < retries; i++) {
        if (*flag) {
            return 0;
        }
        usleep(sleep_us);
    }
    errno = ETIMEDOUT;
    return -1;
}

static inline int fuseg_wait_init(volatile int *init_done) {
    return fuseg_wait_flag(init_done, 200, 10 * 1000);
}

static inline int fuseg_wait_readable(int fd, int timeout_ms) {
    struct pollfd pfd;
    memset(&pfd, 0, sizeof(pfd));
    pfd.fd = fd;
    pfd.events = POLLIN;
    int pr = poll(&pfd, 1, timeout_ms);
    if (pr < 0) {
        return -1;
    }
    if (pr == 0) {
        errno = ETIMEDOUT;
        return -1;
    }
    if ((pfd.revents & POLLIN) == 0) {
        errno = EIO;
        return -1;
    }
    return 0;
}

static inline int fuseg_write_all_fd(int fd, const char *s) {
    size_t left = strlen(s);
    const char *p = s;
    while (left > 0) {
        ssize_t n = write(fd, p, left);
        if (n <= 0) {
            return -1;
        }
        p += n;
        left -= (size_t)n;
    }
    return 0;
}

static inline int fuseg_write_file(const char *path, const char *s) {
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0) {
        return -1;
    }
    int rc = fuseg_write_all_fd(fd, s);
    close(fd);
    return rc;
}

static inline int fuseg_read_file(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    ssize_t n = read(fd, buf, cap);
    close(fd);
    if (n < 0) {
        return -1;
    }
    return (int)n;
}

static inline int fuseg_read_file_cstr(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    ssize_t n = read(fd, buf, cap - 1);
    int saved_errno = errno;
    close(fd);
    if (n < 0) {
        errno = saved_errno;
        return -1;
    }
    buf[n] = '\0';
    return (int)n;
}

static inline int fuseg_do_init_handshake_basic(int fd) {
    if (fuseg_wait_readable(fd, 1000) != 0) {
        return -1;
    }

    unsigned char *buf = (unsigned char *)malloc(FUSE_TEST_BUF_SIZE);
    if (!buf) {
        errno = ENOMEM;
        return -1;
    }
    memset(buf, 0, FUSE_TEST_BUF_SIZE);

    ssize_t n = read(fd, buf, FUSE_TEST_BUF_SIZE);
    if (n < (ssize_t)(sizeof(struct fuse_in_header) + sizeof(struct fuse_init_in))) {
        free(buf);
        return -1;
    }

    struct fuse_in_header in_hdr;
    memcpy(&in_hdr, buf, sizeof(in_hdr));
    if (in_hdr.opcode != FUSE_INIT || in_hdr.len != (uint32_t)n) {
        free(buf);
        errno = EPROTO;
        return -1;
    }

    struct fuse_init_in init_in;
    memcpy(&init_in, buf + sizeof(struct fuse_in_header), sizeof(init_in));
    free(buf);
    if (init_in.major != 7 || init_in.minor == 0 || (init_in.flags == 0 && init_in.flags2 == 0)) {
        errno = EPROTO;
        return -1;
    }

    struct fuse_out_header out_hdr;
    memset(&out_hdr, 0, sizeof(out_hdr));
    out_hdr.len = sizeof(struct fuse_out_header) + sizeof(struct fuse_init_out);
    out_hdr.error = 0;
    out_hdr.unique = in_hdr.unique;

    struct fuse_init_out init_out;
    memset(&init_out, 0, sizeof(init_out));
    init_out.major = 7;
    init_out.minor = 39;
    init_out.flags = FUSE_INIT_EXT | FUSE_MAX_PAGES;
    init_out.flags2 = 0;
    init_out.max_write = 1024 * 1024;
    init_out.max_pages = 256;

    unsigned char reply[sizeof(out_hdr) + sizeof(init_out)];
    memcpy(reply, &out_hdr, sizeof(out_hdr));
    memcpy(reply + sizeof(out_hdr), &init_out, sizeof(init_out));

    ssize_t wn = write(fd, reply, sizeof(reply));
    if (wn != (ssize_t)sizeof(reply)) {
        return -1;
    }

    return 0;
}
