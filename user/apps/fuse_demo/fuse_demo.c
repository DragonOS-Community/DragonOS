/**
 * @file fuse_demo.c
 * @brief Minimal FUSE demo daemon for DragonOS (no libfuse).
 *
 * Usage:
 *   fuse_demo <mountpoint> [--rw] [--allow-other] [--default-permissions] [--threads N]
 *
 * This demo serves a tiny in-memory filesystem:
 *   /hello.txt  (contains "hello from fuse\n")
 */

#include "fuse_test_simplefs.h"

#include <signal.h>
#include <sys/ioctl.h>
 
#ifndef FUSE_DEV_IOC_CLONE
#define FUSE_DEV_IOC_CLONE 0x8004e500 /* _IOR(229, 0, uint32_t) */
#endif

static volatile int g_stop = 0;

static void on_sigint(int signo) {
    (void)signo;
    g_stop = 1;
}

static int parse_int(const char *s, int *out) {
    char *end = NULL;
    long v = strtol(s, &end, 10);
    if (!s[0] || !end || *end != '\0')
        return -1;
    if (v < 0 || v > 1024)
        return -1;
    *out = (int)v;
    return 0;
}

int main(int argc, char **argv) {
    if (argc < 2) {
        fprintf(stderr,
                "usage: %s <mountpoint> [--rw] [--allow-other] [--default-permissions] [--threads N]\n",
                argv[0]);
        return 1;
    }

    fprintf(stderr, "fuse_demo simplefs rev: %s\n", FUSE_SIMPLEFS_REV);

    const char *mp = argv[1];
    int enable_write_ops = 0;
    int allow_other = 0;
    int default_permissions = 0;
    int threads = 1;

    for (int i = 2; i < argc; i++) {
        if (strcmp(argv[i], "--rw") == 0) {
            enable_write_ops = 1;
        } else if (strcmp(argv[i], "--allow-other") == 0) {
            allow_other = 1;
        } else if (strcmp(argv[i], "--default-permissions") == 0) {
            default_permissions = 1;
        } else if (strcmp(argv[i], "--threads") == 0) {
            if (i + 1 >= argc || parse_int(argv[i + 1], &threads) != 0 || threads < 1) {
                fprintf(stderr, "invalid --threads\n");
                return 1;
            }
            i++;
        } else {
            fprintf(stderr, "unknown arg: %s\n", argv[i]);
            return 1;
        }
    }

    if (ensure_dir(mp) != 0) {
        perror("ensure_dir");
        return 1;
    }

    signal(SIGINT, on_sigint);
    signal(SIGTERM, on_sigint);

    int master_fd = open("/dev/fuse", O_RDWR);
    if (master_fd < 0) {
        perror("open(/dev/fuse)");
        return 1;
    }

    volatile int init_done = 0;
    volatile int stop = 0;

    struct fuse_daemon_args master_args;
    memset(&master_args, 0, sizeof(master_args));
    master_args.fd = master_fd;
    master_args.stop = &stop;
    master_args.init_done = &init_done;
    master_args.enable_write_ops = enable_write_ops;
    master_args.stop_on_destroy = 1;

    pthread_t *ths = calloc((size_t)threads, sizeof(pthread_t));
    struct fuse_daemon_args *args = calloc((size_t)threads, sizeof(struct fuse_daemon_args));
    int *fds = calloc((size_t)threads, sizeof(int));
    if (!ths || !args || !fds) {
        fprintf(stderr, "oom\n");
        close(master_fd);
        return 1;
    }

    fds[0] = master_fd;
    args[0] = master_args;
    if (pthread_create(&ths[0], NULL, fuse_daemon_thread, &args[0]) != 0) {
        fprintf(stderr, "pthread_create(master) failed\n");
        close(master_fd);
        return 1;
    }

    char opts[512];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=%u,group_id=%u%s%s", master_fd,
             (unsigned)getuid(), (unsigned)getgid(), allow_other ? ",allow_other" : "",
             default_permissions ? ",default_permissions" : "");

    if (mount("none", mp, "fuse", 0, opts) != 0) {
        perror("mount(fuse)");
        stop = 1;
        close(master_fd);
        pthread_join(ths[0], NULL);
        return 1;
    }

    for (int i = 0; i < 200; i++) {
        if (init_done)
            break;
        usleep(10 * 1000);
    }
    if (!init_done) {
        fprintf(stderr, "init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(master_fd);
        pthread_join(ths[0], NULL);
        return 1;
    }

    /* Optional extra threads via clone */
    for (int i = 1; i < threads; i++) {
        int fd = open("/dev/fuse", O_RDWR);
        if (fd < 0) {
            perror("open(/dev/fuse) for clone");
            break;
        }
        uint32_t oldfd_u32 = (uint32_t)master_fd;
        if (ioctl(fd, FUSE_DEV_IOC_CLONE, &oldfd_u32) != 0) {
            perror("ioctl(FUSE_DEV_IOC_CLONE)");
            close(fd);
            break;
        }
        fds[i] = fd;
        memset(&args[i], 0, sizeof(args[i]));
        args[i].fd = fd;
        args[i].stop = &stop;
        args[i].init_done = &init_done;
        args[i].enable_write_ops = enable_write_ops;
        args[i].stop_on_destroy = 1;
        if (pthread_create(&ths[i], NULL, fuse_daemon_thread, &args[i]) != 0) {
            perror("pthread_create(clone)");
            close(fd);
            break;
        }
    }

    fprintf(stderr, "fuse_demo mounted at %s (threads=%d). Ctrl-C to stop.\n", mp, threads);

    while (!g_stop) {
        sleep(1);
    }

    /* Best-effort cleanup */
    umount(mp);
    stop = 1;
    for (int i = 0; i < threads; i++) {
        if (fds[i] > 0)
            close(fds[i]);
    }
    for (int i = 0; i < threads; i++) {
        if (ths[i])
            pthread_join(ths[i], NULL);
    }
    free(fds);
    free(args);
    free(ths);
    return 0;
}
