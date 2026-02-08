/**
 * @file test_fuse_p1_lifecycle.c
 * @brief Phase P1 test: FORGET request lifecycle + DESTROY on umount.
 */

#include "fuse_test_simplefs.h"

static int wait_init(volatile int *init_done) {
    for (int i = 0; i < 200; i++) {
        if (*init_done)
            return 0;
        usleep(10 * 1000);
    }
    errno = ETIMEDOUT;
    return -1;
}

int main(void) {
    const char *mp = "/tmp/test_fuse_p1_lifecycle";
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
    volatile uint32_t forget_count = 0;
    volatile uint64_t forget_nlookup_sum = 0;
    volatile uint32_t destroy_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.forget_count = &forget_count;
    args.forget_nlookup_sum = &forget_nlookup_sum;
    args.destroy_count = &destroy_count;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        return 1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }

    if (wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }

    char p[256];
    snprintf(p, sizeof(p), "%s/hello.txt", mp);
    for (int i = 0; i < 8; i++) {
        struct stat st;
        if (stat(p, &st) != 0) {
            printf("[FAIL] stat(%s): %s (errno=%d)\n", p, strerror(errno), errno);
            umount(mp);
            stop = 1;
            close(fd);
            pthread_join(th, NULL);
            return 1;
        }
    }

    /* Give daemon time to consume queued FORGETs before unmount path. */
    usleep(100 * 1000);

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }

    for (int i = 0; i < 100; i++) {
        if (destroy_count > 0)
            break;
        usleep(10 * 1000);
    }

    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);

    if (forget_count == 0 || forget_nlookup_sum == 0) {
        printf("[FAIL] expected FORGET requests, got count=%u nlookup_sum=%llu\n", forget_count,
               (unsigned long long)forget_nlookup_sum);
        return 1;
    }

    if (destroy_count == 0) {
        printf("[FAIL] expected DESTROY request on umount\n");
        return 1;
    }

    printf("[PASS] fuse_p1_lifecycle (forget_count=%u, forget_nlookup_sum=%llu, destroy_count=%u)\n",
           forget_count, (unsigned long long)forget_nlookup_sum, destroy_count);
    return 0;
}
