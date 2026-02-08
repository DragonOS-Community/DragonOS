/**
 * @file test_fuse_mount_init.c
 * @brief Phase P0 integration test: mount/INIT handshake and single-use fd.
 */

#include "fuse_test_simplefs.h"

static int wait_readable(int fd, int timeout_ms) {
    struct pollfd pfd;
    memset(&pfd, 0, sizeof(pfd));
    pfd.fd = fd;
    pfd.events = POLLIN;
    int pr = poll(&pfd, 1, timeout_ms);
    if (pr < 0)
        return -1;
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

    unsigned char *buf = malloc(FUSE_TEST_BUF_SIZE);
    if (!buf) {
        errno = ENOMEM;
        printf("[FAIL] malloc init buffer failed\n");
        return -1;
    }
    memset(buf, 0, FUSE_TEST_BUF_SIZE);

    ssize_t n = read(fd, buf, FUSE_TEST_BUF_SIZE);
    if (n < (ssize_t)(sizeof(struct fuse_in_header) + sizeof(struct fuse_init_in))) {
        printf("[FAIL] read INIT too short: n=%zd errno=%d (%s)\n", n, errno, strerror(errno));
        free(buf);
        return -1;
    }

    struct fuse_in_header in_hdr;
    memcpy(&in_hdr, buf, sizeof(in_hdr));
    if (in_hdr.opcode != FUSE_INIT) {
        printf("[FAIL] expected FUSE_INIT opcode=%d got=%u\n", FUSE_INIT, in_hdr.opcode);
        free(buf);
        return -1;
    }
    if (in_hdr.len != (uint32_t)n) {
        printf("[FAIL] header.len mismatch: hdr=%u read=%zd\n", in_hdr.len, n);
        free(buf);
        return -1;
    }

    struct fuse_init_in init_in;
    memcpy(&init_in, buf + sizeof(struct fuse_in_header), sizeof(init_in));
    if (init_in.major != 7 || init_in.minor == 0) {
        printf("[FAIL] invalid init_in version major=%u minor=%u\n", init_in.major, init_in.minor);
        free(buf);
        return -1;
    }
    if (init_in.flags == 0 && init_in.flags2 == 0) {
        printf("[FAIL] expected non-zero init flags\n");
        free(buf);
        return -1;
    }
    free(buf);

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

    unsigned char *tmp = malloc(FUSE_TEST_BUF_SIZE);
    if (!tmp) {
        printf("[FAIL] malloc tmp buffer failed\n");
        umount(mp);
        close(fd);
        return 1;
    }
    memset(tmp, 0, FUSE_TEST_BUF_SIZE);

    ssize_t rn = read(fd, tmp, FUSE_TEST_BUF_SIZE);
    if (rn != -1 || (errno != EAGAIN && errno != EWOULDBLOCK)) {
        printf("[FAIL] expected EAGAIN after init: rn=%zd errno=%d (%s)\n", rn, errno, strerror(errno));
        free(tmp);
        umount(mp);
        close(fd);
        return 1;
    }
    free(tmp);

    /*
     * Second mount with same fd should fail (connection already mounted).
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
