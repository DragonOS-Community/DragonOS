/**
 * @file test_fuse_p4_subtype_mount.c
 * @brief Phase P4 regression: mount with filesystem type "fuse.<subtype>".
 */

#include "fuse_test_simplefs.h"

static int read_all(const char *path, char *buf, size_t cap) {
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

int main(void) {
    const char *mp = "/tmp/test_fuse_p4_subtype";
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
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        return 1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse.fuse3_demo", 0, opts) != 0) {
        printf("[FAIL] mount(fuse.fuse3_demo): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return 1;
    }

    for (int i = 0; i < 200; i++) {
        if (init_done) {
            break;
        }
        usleep(10 * 1000);
    }
    if (!init_done) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return 1;
    }

    char file_path[256];
    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);

    char buf[128];
    if (read_all(file_path, buf, sizeof(buf)) < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return 1;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return 1;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return 1;
    }

    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    printf("[PASS] fuse_p4_subtype_mount\n");
    return 0;
}
