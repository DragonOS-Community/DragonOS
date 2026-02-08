/**
 * @file test_fuse_p3_noopen_readdirplus_notify.c
 * @brief Phase P3 test: NO_OPEN/NO_OPENDIR + READDIRPLUS + notify(unique=0).
 */

#include "fuse_test_simplefs.h"

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

int main(void) {
    const char *mp = "/tmp/test_fuse_p3_noopen";
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
    volatile uint32_t open_count = 0;
    volatile uint32_t opendir_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint32_t releasedir_count = 0;
    volatile uint32_t readdirplus_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.opendir_count = &opendir_count;
    args.release_count = &release_count;
    args.releasedir_count = &releasedir_count;
    args.readdirplus_count = &readdirplus_count;
    args.force_open_enosys = 1;
    args.force_opendir_enosys = 1;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPEN_SUPPORT |
                                   FUSE_NO_OPENDIR_SUPPORT | FUSE_DO_READDIRPLUS;

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
        goto fail;
    }

    char file_path[256];
    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);
    for (int i = 0; i < 2; i++) {
        int f = open(file_path, O_RDONLY);
        if (f < 0) {
            printf("[FAIL] open(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
            goto fail;
        }
        char buf[64];
        ssize_t n = read(f, buf, sizeof(buf) - 1);
        close(f);
        if (n <= 0) {
            printf("[FAIL] read(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
            goto fail;
        }
    }

    for (int i = 0; i < 2; i++) {
        DIR *dir = opendir(mp);
        if (!dir) {
            printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
            goto fail;
        }
        int saw = 0;
        struct dirent *de;
        while ((de = readdir(dir)) != NULL) {
            if (strcmp(de->d_name, "hello.txt") == 0) {
                saw = 1;
            }
        }
        closedir(dir);
        if (!saw) {
            printf("[FAIL] readdir didn't see hello.txt\n");
            goto fail;
        }
    }

    struct {
        struct fuse_out_header out;
        struct fuse_notify_inval_inode_out inval;
    } notify_msg;
    memset(&notify_msg, 0, sizeof(notify_msg));
    notify_msg.out.len = sizeof(notify_msg);
    notify_msg.out.error = FUSE_NOTIFY_INVAL_INODE;
    notify_msg.out.unique = 0;
    notify_msg.inval.ino = 2;
    notify_msg.inval.off = 0;
    notify_msg.inval.len = -1;
    ssize_t wn = write(fd, &notify_msg, sizeof(notify_msg));
    if (wn != (ssize_t)sizeof(notify_msg)) {
        printf("[FAIL] write notify: wn=%zd errno=%d (%s)\n", wn, errno, strerror(errno));
        goto fail;
    }

    usleep(100 * 1000);

    if (open_count != 1 || opendir_count != 1 || release_count != 0 || releasedir_count != 0 ||
        readdirplus_count == 0) {
        printf("[FAIL] counters open=%u opendir=%u release=%u releasedir=%u readdirplus=%u\n",
               open_count, opendir_count, release_count, releasedir_count, readdirplus_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_no_umount;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    printf("[PASS] fuse_p3_noopen_readdirplus_notify\n");
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 1;
}
