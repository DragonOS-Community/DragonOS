/**
 * @file test_fuse_phase_d.c
 * @brief Phase D integration test: create/write/ftruncate/rename/unlink/mkdir/rmdir.
 */

#include "fuse_test_simplefs.h"

static int write_all(const char *path, const char *s) {
    int fd = open(path, O_CREAT | O_TRUNC | O_RDWR, 0644);
    if (fd < 0)
        return -1;
    size_t len = strlen(s);
    ssize_t wn = write(fd, s, len);
    if (wn != (ssize_t)len) {
        close(fd);
        return -1;
    }
    close(fd);
    return 0;
}

static int read_all(const char *path, char *buf, size_t cap) {
    int fd = open(path, O_RDONLY);
    if (fd < 0)
        return -1;
    ssize_t n = read(fd, buf, cap);
    close(fd);
    if (n < 0)
        return -1;
    return (int)n;
}

int main(void) {
    const char *mp = "/tmp/test_fuse_d";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return 1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        return 1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        return 1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    FUSE_TEST_LOG("step: mount start");
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }
    FUSE_TEST_LOG("step: mount ok");

    for (int i = 0; i < 100; i++) {
        if (init_done)
            break;
        usleep(10 * 1000);
    }
    if (!init_done) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }
    FUSE_TEST_LOG("step: init ok");

    char p1[256];
    snprintf(p1, sizeof(p1), "%s/new.txt", mp);
    FUSE_TEST_LOG("step: write_all start");
    if (write_all(p1, "abcdef") != 0) {
        printf("[FAIL] write_all(%s): %s (errno=%d)\n", p1, strerror(errno), errno);
        goto fail;
    }
    FUSE_TEST_LOG("step: write_all ok");

    /* ftruncate to 3 */
    FUSE_TEST_LOG("step: ftruncate open start");
    int f = open(p1, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open for truncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    FUSE_TEST_LOG("step: ftruncate call start");
    if (ftruncate(f, 3) != 0) {
        printf("[FAIL] ftruncate: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    FUSE_TEST_LOG("step: ftruncate call ok");
    close(f);
    FUSE_TEST_LOG("step: ftruncate close ok");

    char buf[64];
    FUSE_TEST_LOG("step: read_all start");
    int n = read_all(p1, buf, sizeof(buf) - 1);
    if (n < 0) {
        printf("[FAIL] read_all after truncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    FUSE_TEST_LOG("step: read_all ok");
    buf[n] = '\0';
    if (strcmp(buf, "abc") != 0) {
        printf("[FAIL] truncate content mismatch got='%s'\n", buf);
        goto fail;
    }

    /* rename */
    char p2[256];
    snprintf(p2, sizeof(p2), "%s/renamed.txt", mp);
    FUSE_TEST_LOG("step: rename start");
    if (rename(p1, p2) != 0) {
        printf("[FAIL] rename: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    FUSE_TEST_LOG("step: rename ok");

    /* unlink */
    FUSE_TEST_LOG("step: unlink start");
    if (unlink(p2) != 0) {
        printf("[FAIL] unlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    FUSE_TEST_LOG("step: unlink ok");

    /* mkdir + rmdir */
    char d1[256];
    snprintf(d1, sizeof(d1), "%s/dir", mp);
    FUSE_TEST_LOG("step: mkdir start");
    if (mkdir(d1, 0755) != 0) {
        printf("[FAIL] mkdir: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    FUSE_TEST_LOG("step: mkdir ok");
    FUSE_TEST_LOG("step: rmdir start");
    if (rmdir(d1) != 0) {
        printf("[FAIL] rmdir: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    FUSE_TEST_LOG("step: rmdir ok");

    FUSE_TEST_LOG("step: umount start");
    umount(mp);
    FUSE_TEST_LOG("step: umount ok");
    rmdir(mp);
    FUSE_TEST_LOG("step: rmdir mp ok");
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    printf("[PASS] fuse_phase_d\n");
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    return 1;
}
