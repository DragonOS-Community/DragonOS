/**
 * @file test_fuse_p2_ops.c
 * @brief Phase P2 test: ACCESS/CREATE/SYMLINK/READLINK/LINK/RENAME2/FLUSH/FSYNC/FSYNCDIR.
 */

#include "fuse_test_simplefs.h"

#include <sys/syscall.h>

static int wait_init(volatile int *init_done) {
    for (int i = 0; i < 200; i++) {
        if (*init_done)
            return 0;
        usleep(10 * 1000);
    }
    errno = ETIMEDOUT;
    return -1;
}

static int write_all(int fd, const char *s) {
    size_t left = strlen(s);
    const char *p = s;
    while (left > 0) {
        ssize_t n = write(fd, p, left);
        if (n <= 0) {
            return -1;
        }
        p += n;
        left -= (size_t)n;
    }
    return 0;
}

int main(void) {
    const char *mp = "/tmp/test_fuse_p2_ops";
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
    volatile uint32_t access_count = 0;
    volatile uint32_t flush_count = 0;
    volatile uint32_t fsync_count = 0;
    volatile uint32_t fsyncdir_count = 0;
    volatile uint32_t create_count = 0;
    volatile uint32_t rename2_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.access_count = &access_count;
    args.flush_count = &flush_count;
    args.fsync_count = &fsync_count;
    args.fsyncdir_count = &fsyncdir_count;
    args.create_count = &create_count;
    args.rename2_count = &rename2_count;
    args.access_deny_mask = 2; /* MAY_WRITE */

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        return 1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,allow_other", fd);
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

    char hello[256];
    snprintf(hello, sizeof(hello), "%s/hello.txt", mp);
    if (access(hello, R_OK) != 0) {
        printf("[FAIL] access(R_OK): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (access(hello, W_OK) == 0 || errno != EACCES) {
        printf("[FAIL] access(W_OK) expected EACCES, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }

    char created[256];
    snprintf(created, sizeof(created), "%s/p2_create.txt", mp);
    int f = open(created, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] open(O_CREAT): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (write_all(f, "p2-data") != 0) {
        printf("[FAIL] write created file: %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (fsync(f) != 0) {
        printf("[FAIL] fsync(file): %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    close(f);

    char symlink_path[256];
    snprintf(symlink_path, sizeof(symlink_path), "%s/p2_symlink.txt", mp);
    if (symlink("p2_create.txt", symlink_path) != 0) {
        printf("[FAIL] symlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    char target_buf[256];
    ssize_t tn = readlink(symlink_path, target_buf, sizeof(target_buf) - 1);
    if (tn <= 0) {
        printf("[FAIL] readlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    target_buf[tn] = '\0';
    if (strcmp(target_buf, "p2_create.txt") != 0) {
        printf("[FAIL] readlink target mismatch: got=%s\n", target_buf);
        goto fail;
    }

    char hard_path[256];
    snprintf(hard_path, sizeof(hard_path), "%s/p2_hard.txt", mp);
    if (link(created, hard_path) != 0) {
        printf("[FAIL] link: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (unlink(created) != 0) {
        printf("[FAIL] unlink original: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    f = open(hard_path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open hard link: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    char rbuf[64];
    ssize_t rn = read(f, rbuf, sizeof(rbuf) - 1);
    close(f);
    if (rn <= 0) {
        printf("[FAIL] read hard link: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    rbuf[rn] = '\0';
    if (strcmp(rbuf, "p2-data") != 0) {
        printf("[FAIL] hard link content mismatch: got=%s\n", rbuf);
        goto fail;
    }

    char dst_exist[256];
    snprintf(dst_exist, sizeof(dst_exist), "%s/p2_dst_exist.txt", mp);
    f = open(dst_exist, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] create dst_exist: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);

    if (syscall(SYS_renameat2, AT_FDCWD, hard_path, AT_FDCWD, dst_exist, RENAME_NOREPLACE) == 0
        || errno != EEXIST) {
        printf("[FAIL] renameat2 NOREPLACE expected EEXIST, errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }

    char renamed[256];
    snprintf(renamed, sizeof(renamed), "%s/p2_renamed.txt", mp);
    if (syscall(SYS_renameat2, AT_FDCWD, hard_path, AT_FDCWD, renamed, RENAME_NOREPLACE) != 0) {
        printf("[FAIL] renameat2 NOREPLACE success path: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    int dfd = open(mp, O_RDONLY | O_DIRECTORY);
    if (dfd < 0) {
        printf("[FAIL] open mountpoint dirfd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fsync(dfd) != 0) {
        printf("[FAIL] fsync(dirfd): %s (errno=%d)\n", strerror(errno), errno);
        close(dfd);
        goto fail;
    }
    close(dfd);

    usleep(100 * 1000);

    if (access_count < 2 || flush_count == 0 || fsync_count == 0 || fsyncdir_count == 0
        || create_count == 0 || rename2_count < 2) {
        printf("[FAIL] counters access=%u flush=%u fsync=%u fsyncdir=%u create=%u rename2=%u\n",
               access_count, flush_count, fsync_count, fsyncdir_count, create_count,
               rename2_count);
        goto fail;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail_noum;
    }
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);

    printf("[PASS] fuse_p2_ops (access=%u flush=%u fsync=%u fsyncdir=%u create=%u rename2=%u)\n",
           access_count, flush_count, fsync_count, fsyncdir_count, create_count, rename2_count);
    return 0;

fail:
    umount(mp);
fail_noum:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 1;
}
