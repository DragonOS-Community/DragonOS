#include <gtest/gtest.h>

#include <signal.h>
#include <sys/ioctl.h>
#include <sys/syscall.h>
#include <sys/wait.h>

#include "fuse_gtest_common.h"

#ifndef FUSE_DEV_IOC_CLONE
#define FUSE_DEV_IOC_CLONE 0x8004e500
#endif

static int ext_test_p2_ops() {
    const char *mp = "/tmp/test_fuse_p2_ops";
    int f = -1;
    int dfd = -1;
    ssize_t tn = -1;
    ssize_t rn = -1;
    char hello[256];
    char created[256];
    char symlink_path[256];
    char target_buf[256];
    char hard_path[256];
    char rbuf[64];
    char dst_exist[256];
    char renamed[256];
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
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
    args.access_deny_mask = 2;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,allow_other", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    snprintf(hello, sizeof(hello), "%s/hello.txt", mp);
    if (access(hello, R_OK) != 0) {
        printf("[FAIL] access(R_OK): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (access(hello, W_OK) == 0 || errno != EACCES) {
        printf("[FAIL] access(W_OK) expected EACCES, errno=%d (%s)\n", errno, strerror(errno));
        goto fail;
    }

    snprintf(created, sizeof(created), "%s/p2_create.txt", mp);
    f = open(created, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] open(O_CREAT): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fuseg_write_all_fd(f, "p2-data") != 0) {
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

    snprintf(symlink_path, sizeof(symlink_path), "%s/p2_symlink.txt", mp);
    if (symlink("p2_create.txt", symlink_path) != 0) {
        printf("[FAIL] symlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    tn = readlink(symlink_path, target_buf, sizeof(target_buf) - 1);
    if (tn <= 0) {
        printf("[FAIL] readlink: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    target_buf[tn] = '\0';
    if (strcmp(target_buf, "p2_create.txt") != 0) {
        printf("[FAIL] readlink target mismatch: got=%s\n", target_buf);
        goto fail;
    }

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
    rn = read(f, rbuf, sizeof(rbuf) - 1);
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

    snprintf(dst_exist, sizeof(dst_exist), "%s/p2_dst_exist.txt", mp);
    f = open(dst_exist, O_CREAT | O_RDWR, 0644);
    if (f < 0) {
        printf("[FAIL] create dst_exist: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(f);

    if (syscall(SYS_renameat2, AT_FDCWD, hard_path, AT_FDCWD, dst_exist, RENAME_NOREPLACE) == 0 ||
        errno != EEXIST) {
        printf("[FAIL] renameat2 NOREPLACE expected EEXIST, errno=%d (%s)\n", errno,
               strerror(errno));
        goto fail;
    }

    snprintf(renamed, sizeof(renamed), "%s/p2_renamed.txt", mp);
    if (syscall(SYS_renameat2, AT_FDCWD, hard_path, AT_FDCWD, renamed, RENAME_NOREPLACE) != 0) {
        printf("[FAIL] renameat2 NOREPLACE success path: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    dfd = open(mp, O_RDONLY | O_DIRECTORY);
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

    if (access_count < 2 || flush_count == 0 || fsync_count == 0 || fsyncdir_count == 0 ||
        create_count == 0 || rename2_count < 2) {
        printf("[FAIL] counters access=%u flush=%u fsync=%u fsyncdir=%u create=%u rename2=%u\n",
               access_count, flush_count, fsync_count, fsyncdir_count, create_count,
               rename2_count);
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
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static void ext_sigusr1_handler(int signo) {
    (void)signo;
}

struct ext_reader_ctx {
    char path[256];
    volatile int done;
    ssize_t nread;
    int err;
};

static void *ext_reader_thread(void *arg) {
    struct ext_reader_ctx *ctx = (struct ext_reader_ctx *)arg;
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

static int ext_test_p3_interrupt() {
    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = ext_sigusr1_handler;
    sigemptyset(&sa.sa_mask);
    sa.sa_flags = 0;

    struct sigaction old_sa;
    if (sigaction(SIGUSR1, &sa, &old_sa) != 0) {
        printf("[FAIL] sigaction(SIGUSR1): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }

    const char *mp = "/tmp/test_fuse_p3_interrupt";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
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
    args.block_read_until_interrupt = 1000;
    args.interrupt_count = &interrupt_count;
    args.blocked_read_unique = &blocked_read_unique;
    args.last_interrupt_target = &last_interrupt_target;

    pthread_t daemon_th;
    if (pthread_create(&daemon_th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create(daemon)\n");
        close(fd);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(daemon_th, NULL);
        rmdir(mp);
        sigaction(SIGUSR1, &old_sa, NULL);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    struct ext_reader_ctx rctx;
    memset(&rctx, 0, sizeof(rctx));
    snprintf(rctx.path, sizeof(rctx.path), "%s/hello.txt", mp);

    pthread_t reader_th;
    if (pthread_create(&reader_th, NULL, ext_reader_thread, &rctx) != 0) {
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
    sigaction(SIGUSR1, &old_sa, NULL);
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(daemon_th, NULL);
    rmdir(mp);
    sigaction(SIGUSR1, &old_sa, NULL);
    return -1;
}

static int ext_test_p3_noopen_readdirplus_notify() {
    const char *mp = "/tmp/test_fuse_p3_noopen";
    ssize_t wn = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
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
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&init_done) != 0) {
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
    wn = write(fd, &notify_msg, sizeof(notify_msg));
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
    return 0;

fail:
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_p4_subtype_mount() {
    const char *mp = "/tmp/test_fuse_p4_subtype";
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
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
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", fd);
    if (mount("none", mp, "fuse.fuse3_demo", 0, opts) != 0) {
        printf("[FAIL] mount(fuse.fuse3_demo): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
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
        return -1;
    }

    char file_path[256];
    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);

    char buf[128];
    if (fuseg_read_file_cstr(file_path, buf, sizeof(buf)) < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", file_path, strerror(errno), errno);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (umount(mp) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;
}

static int ext_run_child_drop_priv_and_stat(const char *mp, int expect_errno, int expect_success) {
    pid_t pid = fork();
    if (pid < 0) {
        return -1;
    }
    if (pid == 0) {
        if (setgid(1000) != 0) {
            _exit(30);
        }
        if (setuid(1000) != 0) {
            _exit(31);
        }

        struct stat st;
        int r = stat(mp, &st);
        if (expect_success) {
            if (r != 0)
                _exit(10);
            char p[256];
            snprintf(p, sizeof(p), "%s/hello.txt", mp);
            int fd = open(p, O_RDONLY);
            if (fd < 0)
                _exit(11);
            char buf[64];
            ssize_t n = read(fd, buf, sizeof(buf) - 1);
            close(fd);
            if (n < 0)
                _exit(12);
            buf[n] = '\0';
            if (strcmp(buf, "hello from fuse\n") != 0)
                _exit(13);
            _exit(0);
        }

        if (r != 0 && errno == expect_errno) {
            _exit(0);
        }
        if (r != 0) {
            _exit(21);
        }

        /*
         * Linux 语义下，目录本身的 stat 可能成功；真正的拒绝点通常体现在
         * 访问目录内对象（例如 open/stat 子路径）。
         */
        char p[256];
        snprintf(p, sizeof(p), "%s/hello.txt", mp);
        int fd = open(p, O_RDONLY);
        if (fd >= 0) {
            close(fd);
            _exit(22);
        }
        if (errno != expect_errno) {
            _exit(23);
        }
        _exit(0);
    }

    int status = 0;
    if (waitpid(pid, &status, 0) < 0) {
        return -1;
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        errno = ECHILD;
        return -1;
    }
    return 0;
}

static int ext_run_permission_case(const char *mp, const char *opts, uint32_t root_mode_override,
                                   uint32_t hello_mode_override, int expect_errno,
                                   int expect_success) {
    if (ensure_dir(mp) != 0) {
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;
    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.exit_after_init = 0;
    args.root_mode_override = root_mode_override;
    args.hello_mode_override = hello_mode_override;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        close(fd);
        rmdir(mp);
        return -1;
    }

    char full_opts[512];
    snprintf(full_opts, sizeof(full_opts), "fd=%d,%s", fd, opts);
    if (mount("none", mp, "fuse", 0, full_opts) != 0) {
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (fuseg_wait_init(&init_done) != 0) {
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    if (ext_run_child_drop_priv_and_stat(mp, expect_errno, expect_success) != 0) {
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        rmdir(mp);
        return -1;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    return 0;
}

static int ext_test_permissions() {
    const uint32_t DIR_NO_PERM = 0040000;
    const uint32_t REG_NO_PERM = 0100000;

    {
        const char *mp = "/tmp/test_fuse_perm_owner";
        if (ext_run_permission_case(mp, "rootmode=040755,user_id=0,group_id=0", 0, 0, EACCES, 0) !=
            0) {
            printf("[FAIL] mount owner restriction\n");
            return -1;
        }
    }

    {
        const char *mp = "/tmp/test_fuse_perm_default";
        if (ext_run_permission_case(
                mp, "rootmode=040000,user_id=0,group_id=0,allow_other,default_permissions",
                DIR_NO_PERM, REG_NO_PERM, EACCES, 0) != 0) {
            printf("[FAIL] default_permissions deny\n");
            return -1;
        }
    }

    {
        const char *mp = "/tmp/test_fuse_perm_remote";
        if (ext_run_permission_case(mp, "rootmode=040000,user_id=0,group_id=0,allow_other",
                                    DIR_NO_PERM, REG_NO_PERM, 0, 1) != 0) {
            printf("[FAIL] remote permission model allow\n");
            return -1;
        }
    }

    return 0;
}

static int ext_test_clone() {
    const char *mp = "/tmp/test_fuse_clone";
    DIR *d = NULL;
    int found = 0;
    struct dirent *de = NULL;
    char p[256];
    struct stat st;
    char buf[128];
    int n = -1;
    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        return -1;
    }

    int master_fd = open("/dev/fuse", O_RDWR);
    if (master_fd < 0) {
        printf("[FAIL] open(/dev/fuse master): %s (errno=%d)\n", strerror(errno), errno);
        rmdir(mp);
        return -1;
    }

    volatile int stop = 0;
    volatile int init_done = 0;

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
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0", master_fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        stop = 1;
        close(master_fd);
        pthread_join(master_th, NULL);
        rmdir(mp);
        return -1;
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
        rmdir(mp);
        return -1;
    }

    pthread_join(master_th, NULL);

    int clone_fd = open("/dev/fuse", O_RDWR);
    if (clone_fd < 0) {
        printf("[FAIL] open(/dev/fuse clone): %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(master_fd);
        rmdir(mp);
        return -1;
    }

    uint32_t oldfd_u32 = (uint32_t)master_fd;
    if (ioctl(clone_fd, FUSE_DEV_IOC_CLONE, &oldfd_u32) != 0) {
        printf("[FAIL] ioctl(FUSE_DEV_IOC_CLONE): %s (errno=%d)\n", strerror(errno), errno);
        umount(mp);
        close(clone_fd);
        close(master_fd);
        rmdir(mp);
        return -1;
    }

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
        rmdir(mp);
        return -1;
    }

    d = opendir(mp);
    if (!d) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    found = 0;
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

    snprintf(p, sizeof(p), "%s/hello.txt", mp);
    if (stat(p, &st) != 0) {
        printf("[FAIL] stat(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        goto fail;
    }
    if (!S_ISREG(st.st_mode)) {
        printf("[FAIL] stat: expected regular file\n");
        goto fail;
    }

    n = fuseg_read_file_cstr(p, buf, sizeof(buf));
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", p, strerror(errno), errno);
        goto fail;
    }
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
    return 0;

fail:
    umount(mp);
    stop = 1;
    close(clone_fd);
    close(master_fd);
    pthread_join(clone_th, NULL);
    rmdir(mp);
    return -1;
}

TEST(FuseExtended, OpsAccessCreateSymlinkLinkRename2FlushFsync) {
    ASSERT_EQ(0, ext_test_p2_ops());
}

TEST(FuseExtended, InterruptDeliversFuseInterrupt) {
    ASSERT_EQ(0, ext_test_p3_interrupt());
}

TEST(FuseExtended, NoOpenNoOpendirReaddirplusNotify) {
    ASSERT_EQ(0, ext_test_p3_noopen_readdirplus_notify());
}

TEST(FuseExtended, SubtypeMountFuseDotSubtype) {
    ASSERT_EQ(0, ext_test_p4_subtype_mount());
}

TEST(FuseExtended, PermissionModelAllowOtherDefaultPermissions) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to execute setuid/setgid permission cases";
    }
    ASSERT_EQ(0, ext_test_permissions());
}

TEST(FuseExtended, DevCloneAttachAndServe) {
    ASSERT_EQ(0, ext_test_clone());
}

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
