/**
 * @file test_fuse_p3_interrupt.c
 * @brief Phase P3 test: blocked request interrupted by signal -> FUSE_INTERRUPT.
 */

#include "fuse_test_simplefs.h"

#include <signal.h>

static void sigusr1_handler(int signo) {
    (void)signo;
}

static int wait_init(volatile int *init_done) {
    for (int i = 0; i < 200; i++) {
        if (*init_done) {
            return 0;
        }
        usleep(10 * 1000);
    }
    errno = ETIMEDOUT;
    return -1;
}

struct reader_ctx {
    char path[256];
    volatile int done;
    ssize_t nread;
    int err;
};

static void *reader_thread(void *arg) {
    struct reader_ctx *ctx = (struct reader_ctx *)arg;
    int fd = open(ctx->path, O_RDONLY);
    if (fd < 0) {
        ctx->nread = -1;
        ctx->err = errno;
        ctx->done = 1;
        return NULL;
    }

    char buf[64];
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n < 0) {
        ctx->nread = -1;
        ctx->err = errno;
    } else {
        ctx->nread = n;
        ctx->err = 0;
    }
    close(fd);
    ctx->done = 1;
    return NULL;
}

int main(void) {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = sigusr1_handler;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0; /* no SA_RESTART */
    if (sigaction(SIGUSR1, &sa, NULL) != 0) {
        printf("[FAIL] sigaction(SIGUSR1): %s (errno=%d)\n", strerror(errno), errno);
        return 1;
    }

    const char *mp = "/tmp/test_fuse_p3_interrupt";
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
    volatile uint32_t interrupt_count = 0;
    volatile uint64_t blocked_read_unique = 0;
    volatile uint64_t last_interrupt_target = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.block_read_until_interrupt = 1000; /* delay read reply 1s */
    args.interrupt_count = &interrupt_count;
    args.blocked_read_unique = &blocked_read_unique;
    args.last_interrupt_target = &last_interrupt_target;

    pthread_t daemon_th;
    if (pthread_create(&daemon_th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create(daemon)\n");
        close(fd);
        return 1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(daemon_th, NULL);
        return 1;
    }
    if (wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    struct reader_ctx rctx;
    memset(&rctx, 0, sizeof(rctx));
    snprintf(rctx.path, sizeof(rctx.path), "%s/hello.txt", mp);

    pthread_t reader_th;
    if (pthread_create(&reader_th, NULL, reader_thread, &rctx) != 0) {
        printf("[FAIL] pthread_create(reader)\n");
        goto fail;
    }

    for (int i = 0; i < 200; i++) {
        if (blocked_read_unique != 0) {
            break;
        }
        usleep(5 * 1000);
    }
    if (blocked_read_unique == 0) {
        printf("[FAIL] timed out waiting for blocked read request\n");
        stop = 1;
        pthread_join(reader_th, NULL);
        goto fail;
    }

    if (pthread_kill(reader_th, SIGUSR1) != 0) {
        printf("[FAIL] pthread_kill(SIGUSR1)\n");
        stop = 1;
        pthread_join(reader_th, NULL);
        goto fail;
    }
    pthread_join(reader_th, NULL);

    if (rctx.nread != -1 || rctx.err != EINTR) {
        printf("[FAIL] reader expected EINTR, nread=%zd err=%d (%s)\n", rctx.nread, rctx.err,
               strerror(rctx.err));
        goto fail;
    }

    for (int i = 0; i < 500; i++) {
        if (interrupt_count > 0) {
            break;
        }
        usleep(5 * 1000);
    }

    if (interrupt_count == 0) {
        printf("[FAIL] expected FUSE_INTERRUPT request\n");
        goto fail;
    }
    if (last_interrupt_target == 0 || last_interrupt_target != blocked_read_unique) {
        printf("[FAIL] interrupt target mismatch: blocked=%llu interrupt_target=%llu\n",
               (unsigned long long)blocked_read_unique, (unsigned long long)last_interrupt_target);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(daemon_th, NULL);
    rmdir(mp);
    printf("[PASS] fuse_p3_interrupt (interrupt_count=%u)\n", interrupt_count);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(daemon_th, NULL);
    rmdir(mp);
    return 1;
}
