/**
 * @file test_fuse_mount_init.c
 * @brief Phase B integration test: mount -t fuse -o fd=... triggers INIT and accepts init reply.
 */

#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <stdint.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/types.h>
#include <unistd.h>

#ifndef FUSE_INIT
#define FUSE_INIT 26
#endif

struct fuse_in_header {
    uint32_t len;
    uint32_t opcode;
    uint64_t unique;
    uint64_t nodeid;
    uint32_t uid;
    uint32_t gid;
    uint32_t pid;
    uint16_t total_extlen;
    uint16_t padding;
};

struct fuse_out_header {
    uint32_t len;
    int32_t error;
    uint64_t unique;
};

struct fuse_init_in {
    uint32_t major;
    uint32_t minor;
    uint32_t max_readahead;
    uint32_t flags;
    uint32_t flags2;
    uint32_t unused[11];
};

struct fuse_init_out {
    uint32_t major;
    uint32_t minor;
    uint32_t max_readahead;
    uint32_t flags;
    uint16_t max_background;
    uint16_t congestion_threshold;
    uint32_t max_write;
    uint32_t time_gran;
    uint16_t max_pages;
    uint16_t map_alignment;
    uint32_t flags2;
    uint32_t unused[7];
};

static int ensure_dir(const char *path) {
    struct stat st;
    if (stat(path, &st) == 0) {
        if (S_ISDIR(st.st_mode)) {
            return 0;
        }
        errno = ENOTDIR;
        return -1;
    }
    return mkdir(path, 0755);
}

static int wait_readable(int fd, int timeout_ms) {
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

static int do_init_handshake(int fd) {
    if (wait_readable(fd, 1000) != 0) {
        printf("[FAIL] poll for INIT: %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    unsigned char buf[4096];
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n < (ssize_t)(sizeof(struct fuse_in_header) + sizeof(struct fuse_init_in))) {
        printf("[FAIL] read INIT too short: n=%zd errno=%d (%s)\n", n, errno, strerror(errno));
        return -1;
    }

    struct fuse_in_header in_hdr;
    memcpy(&in_hdr, buf, sizeof(in_hdr));
    if (in_hdr.opcode != FUSE_INIT) {
        printf("[FAIL] expected FUSE_INIT opcode=%d got=%u\n", FUSE_INIT, in_hdr.opcode);
        return -1;
    }
    if (in_hdr.len != (uint32_t)n) {
        printf("[FAIL] header.len mismatch: hdr=%u read=%zd\n", in_hdr.len, n);
        return -1;
    }

    struct fuse_init_in init_in;
    memcpy(&init_in, buf + sizeof(struct fuse_in_header), sizeof(init_in));
    if (init_in.major != 7) {
        printf("[FAIL] init_in.major expected 7 got %u\n", init_in.major);
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
    init_out.max_readahead = 0;
    init_out.flags = 0;
    init_out.max_write = 4096;

    unsigned char reply[sizeof(out_hdr) + sizeof(init_out)];
    memcpy(reply, &out_hdr, sizeof(out_hdr));
    memcpy(reply + sizeof(out_hdr), &init_out, sizeof(init_out));

    ssize_t wn = write(fd, reply, sizeof(reply));
    if (wn != (ssize_t)sizeof(reply)) {
        printf("[FAIL] write INIT reply: wn=%zd errno=%d (%s)\n", wn, errno, strerror(errno));
        return -1;
    }

    return 0;
}

int main(void) {
    const char *mp = "/tmp/test_fuse_mp";
    const char *mp2 = "/tmp/test_fuse_mp2";

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return 1;
    }
    if (ensure_dir(mp2) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp2, strerror(errno), errno);
        return 1;
    }

    int fd = open("/dev/fuse", O_RDWR | O_NONBLOCK);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        return 1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);

    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        return 1;
    }

    if (do_init_handshake(fd) != 0) {
        umount(mp);
        close(fd);
        return 1;
    }

    /* After INIT reply, queue should be empty. Nonblock read should return EAGAIN. */
    unsigned char tmp[64];
    ssize_t rn = read(fd, tmp, sizeof(tmp));
    if (rn != -1 || (errno != EAGAIN && errno != EWOULDBLOCK)) {
        printf("[FAIL] expected EAGAIN after init: rn=%zd errno=%d (%s)\n", rn, errno, strerror(errno));
        umount(mp);
        close(fd);
        return 1;
    }

    /*
     * Second mount with same fd should fail (connection already mounted).
     * Use a different mountpoint to avoid any "mount-on-mountpoint-root" corner
     * behavior from interfering with the check.
     */
    if (mount("none", mp2, "fuse", 0, opts) == 0) {
        printf("[FAIL] second mount with same fd unexpectedly succeeded\n");
        umount(mp);
        umount(mp2);
        close(fd);
        return 1;
    }
    if (errno != EINVAL) {
        printf("[FAIL] second mount expected EINVAL got errno=%d (%s)\n", errno, strerror(errno));
        umount(mp);
        close(fd);
        return 1;
    }
    printf("[INFO] second mount failed as expected: errno=%d (%s)\n", errno, strerror(errno));

    umount(mp);
    rmdir(mp);
    rmdir(mp2);
    close(fd);
    printf("[PASS] fuse_mount_init\n");
    return 0;
}
