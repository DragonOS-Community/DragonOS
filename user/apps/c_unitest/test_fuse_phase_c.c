/**
 * @file test_fuse_phase_c.c
 * @brief Phase C integration test: LOOKUP/GETATTR/READDIR/OPEN/READ path.
 */

#include "fuse_test_simplefs.h"

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
    const char *mp = "/tmp/test_fuse_c";
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

    /* wait init */
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

    /* readdir */
    DIR *d = opendir(mp);
    if (!d) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
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
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }

    /* stat + cat */
    char p[256];
    snprintf(p, sizeof(p), "%s/hello.txt", mp);

    struct stat st;
    if (stat(p, &st) != 0) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }
    if (!S_ISREG(st.st_mode)) {
        printf("[FAIL] stat: expected regular file\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }

    char buf[128];
    int n = read_all(p, buf, sizeof(buf) - 1);
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }
    buf[n] = '\0';
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return 1;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    printf("[PASS] fuse_phase_c\n");
    return 0;
}

