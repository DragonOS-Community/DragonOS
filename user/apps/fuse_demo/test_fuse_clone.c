/**
 * @file test_fuse_clone.c
 * @brief Phase E test: FUSE_DEV_IOC_CLONE basic behavior.
 *
 * This test mounts a simple FUSE filesystem using a master /dev/fuse fd,
 * then opens a second /dev/fuse fd and uses FUSE_DEV_IOC_CLONE to attach it
 * to the existing connection. After INIT is done, all requests are served
 * via the cloned fd.
 */

#include "fuse_test_simplefs.h"

#include <sys/ioctl.h>

#ifndef FUSE_DEV_IOC_CLONE
#define FUSE_DEV_IOC_CLONE 0x80044600 /* _IOR('F', 0, uint32_t) */
#endif

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
    const char *mp = "/tmp/test_fuse_clone";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return 1;
    }

    int master_fd = open("/dev/fuse", O_RDWR);
    if (master_fd < 0) {
        printf("[FAIL] open(/dev/fuse master): %s (errno=%d)\n", strerror(errno), errno);
        return 1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

    /* INIT responder on master fd, then exit */
    struct fuse_daemon_args master_args;
    memset(&master_args, 0, sizeof(master_args));
    master_args.fd = master_fd;
    master_args.stop = &stop;
    master_args.init_done = &init_done;
    master_args.enable_write_ops = 0;
    master_args.exit_after_init = 1;

    pthread_t master_th;
    if (pthread_create(&master_th, NULL, fuse_daemon_thread, &master_args) != 0) {
        printf("[FAIL] pthread_create(master)\n");
        close(master_fd);
        return 1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", master_fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(master_fd);
        pthread_join(master_th, NULL);
        return 1;
    }

    for (int i = 0; i < 100; i++) {
        if (init_done)
            break;
        usleep(10 * 1000);
    }
    if (!init_done) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(master_fd);
        pthread_join(master_th, NULL);
        return 1;
    }

    pthread_join(master_th, NULL);

    /* Open a new fd and clone it to the master connection */
    int clone_fd = open("/dev/fuse", O_RDWR);
    if (clone_fd < 0) {
        printf("[FAIL] open(/dev/fuse clone): %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(master_fd);
        return 1;
    }

    uint32_t oldfd_u32 = (uint32_t)master_fd;
    if (ioctl(clone_fd, FUSE_DEV_IOC_CLONE, &oldfd_u32) != 0) {
        printf("[FAIL] ioctl(FUSE_DEV_IOC_CLONE): %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(clone_fd);
        close(master_fd);
        return 1;
    }

    /* Serve all subsequent requests via the cloned fd */
    struct fuse_daemon_args clone_args;
    memset(&clone_args, 0, sizeof(clone_args));
    clone_args.fd = clone_fd;
    clone_args.stop = &stop;
    clone_args.init_done = &init_done;
    clone_args.enable_write_ops = 0;
    clone_args.exit_after_init = 0;

    pthread_t clone_th;
    if (pthread_create(&clone_th, NULL, fuse_daemon_thread, &clone_args) != 0) {
        printf("[FAIL] pthread_create(clone)\n");
        umount(mp);
        close(clone_fd);
        close(master_fd);
        return 1;
    }

    /* readdir + stat + read */
    DIR *d = opendir(mp);
    if (!d) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    int found = 0;
    struct dirent *de;
    while ((de = readdir(d)) != NULL) {
        if (strcmp(de->d_name, "hello.txt") == 0) {
            found = 1;
            break;
        }
    }
    closedir(d);
    if (!found) {
        printf("[FAIL] readdir: hello.txt not found\n");
        goto fail;
    }

    char p[256];
    snprintf(p, sizeof(p), "%s/hello.txt", mp);
    struct stat st;
    if (stat(p, &st) != 0) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        goto fail;
    }
    if (!S_ISREG(st.st_mode)) {
        printf("[FAIL] stat: expected regular file\n");
        goto fail;
    }

    char buf[128];
    int n = read_all(p, buf, sizeof(buf) - 1);
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        goto fail;
    }
    buf[n] = '\0';
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        goto fail;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(clone_fd);
    close(master_fd);
    pthread_join(clone_th, NULL);
    printf("[PASS] fuse_clone\n");
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(clone_fd);
    close(master_fd);
    pthread_join(clone_th, NULL);
    return 1;
}

