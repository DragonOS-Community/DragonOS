#include <gtest/gtest.h>

#include <signal.h>
#include <setjmp.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>

#include "fuse_gtest_common.h"

static sigjmp_buf g_fuse_sigbus_jmp;
static volatile sig_atomic_t g_fuse_sigbus_seen = 0;
static sigjmp_buf g_fuse_sigsegv_jmp;
static volatile sig_atomic_t g_fuse_sigsegv_seen = 0;

static void fuse_sigbus_longjmp_handler(int sig) {
    (void)sig;
    g_fuse_sigbus_seen = 1;
    siglongjmp(g_fuse_sigbus_jmp, 1);
}

static void fuse_sigsegv_longjmp_handler(int sig) {
    (void)sig;
    g_fuse_sigsegv_seen = 1;
    siglongjmp(g_fuse_sigsegv_jmp, 1);
}

#ifndef FUSE_DEV_IOC_CLONE
#define FUSE_DEV_IOC_CLONE 0x8004e500
#endif

#ifndef POSIX_FADV_NOREUSE
#define POSIX_FADV_NOREUSE 5
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

static int ext_test_open_zero_fh_valid() {
    const char *mp = "/tmp/test_fuse_zero_fh";
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
    volatile uint32_t read_count = 0;
    volatile uint64_t last_open_fh = UINT64_MAX;
    volatile uint64_t last_read_fh = UINT64_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.last_open_fh = &last_open_fh;
    args.last_read_fh = &last_read_fh;
    args.has_hello_open_fh_override = 1;
    args.hello_open_fh_override = 0;

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

    char path[256];
    char buf[128];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    if (fuseg_read_file_cstr(path, buf, sizeof(buf)) < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (strcmp(buf, "hello from fuse\n") != 0) {
        printf("[FAIL] content mismatch: got='%s'\n", buf);
        goto fail;
    }

    usleep(100 * 1000);
    if (open_count == 0 || read_count == 0 || last_open_fh != 0 || last_read_fh != 0) {
        printf("[FAIL] fh counters open=%u read=%u open_fh=%llu read_fh=%llu\n", open_count,
               read_count, (unsigned long long)last_open_fh, (unsigned long long)last_read_fh);
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

static int ext_test_noopen_fsync_uses_zero_fh() {
    const char *mp = "/tmp/test_fuse_noopen_fsync";
    int f = -1;
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
    volatile uint32_t fsync_count = 0;
    volatile uint32_t release_count = 0;
    volatile uint64_t last_fsync_fh = UINT64_MAX;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.fsync_count = &fsync_count;
    args.release_count = &release_count;
    args.last_fsync_fh = &last_fsync_fh;
    args.force_open_enosys = 1;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPEN_SUPPORT;

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

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (fsync(f) != 0) {
        printf("[FAIL] fsync(no-open file): %s (errno=%d)\n", strerror(errno), errno);
        close(f);
        goto fail;
    }
    close(f);

    usleep(100 * 1000);
    if (open_count != 1 || fsync_count == 0 || release_count != 0 || last_fsync_fh != 0) {
        printf("[FAIL] counters open=%u fsync=%u release=%u fsync_fh=%llu\n", open_count,
               fsync_count, release_count, (unsigned long long)last_fsync_fh);
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

static int ext_test_open_release_flags_match_linux() {
    const char *mp = "/tmp/test_fuse_open_flags";
    int requested = O_RDWR | O_NOCTTY | O_TRUNC | O_APPEND | O_NONBLOCK;
    uint32_t expected_open = (uint32_t)(requested & ~(O_CREAT | O_EXCL | O_NOCTTY));
    int f = -1;
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
    volatile uint32_t last_open_flags = 0;
    volatile uint32_t last_release_flags = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.last_open_in_flags = &last_open_flags;
    args.last_release_in_flags = &last_release_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ATOMIC_O_TRUNC;

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

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, requested);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    close(f);

    usleep(100 * 1000);
    if (last_open_flags != expected_open) {
        printf("[FAIL] open flags got=0%o expected=0%o\n", last_open_flags, expected_open);
        goto fail;
    }
    if (last_release_flags != (uint32_t)requested) {
        printf("[FAIL] release flags got=0%o expected=0%o\n", last_release_flags,
               (uint32_t)requested);
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

static int ext_test_atomic_otrunc_uses_open_without_setattr() {
    const char *mp = "/tmp/test_fuse_atomic_otrunc";
    int requested = O_RDWR | O_TRUNC;
    int f = -1;
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
    volatile uint32_t last_open_flags = 0;
    volatile uint32_t open_count = 0;
    volatile uint32_t setattr_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 0;
    args.stop_on_destroy = 1;
    args.open_count = &open_count;
    args.setattr_count = &setattr_count;
    args.last_open_in_flags = &last_open_flags;
    args.init_out_flags_override = FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_ATOMIC_O_TRUNC;

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

    char path[256];
    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, requested);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    close(f);
    f = -1;

    usleep(100 * 1000);
    if (open_count != 1 || (last_open_flags & O_TRUNC) == 0) {
        printf("[FAIL] open counters/flags open=%u flags=0%o\n", open_count, last_open_flags);
        goto fail;
    }
    if (setattr_count != 0) {
        printf("[FAIL] atomic O_TRUNC unexpectedly sent SETATTR count=%u\n", setattr_count);
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
    if (f >= 0) {
        close(f);
    }
    umount(mp);
fail_no_umount:
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_init_requests_linux_no_open_support() {
    const char *mp = "/tmp/test_fuse_init_flags";
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
    volatile uint32_t init_flags = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.init_in_flags = &init_flags;

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

    if ((init_flags & FUSE_NO_OPEN_SUPPORT) == 0 ||
        (init_flags & FUSE_NO_OPENDIR_SUPPORT) == 0) {
        printf("[FAIL] INIT flags missing no-open support bits: flags=0x%x\n", init_flags);
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

static int ext_test_large_read_over_max_write() {
    const char *mp = "/tmp/test_fuse_large_read";
    const size_t data_size = 6000;
    char path[256];
    char *buf = NULL;
    int n = -1;

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
    volatile uint32_t read_count = 0;
    volatile uint64_t read_offsets[4] = {0};
    volatile uint32_t read_sizes[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_sizes = read_sizes;
    args.read_trace_capacity = 4;
    args.hello_data_size_override = data_size;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
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

    buf = (char *)malloc(data_size);
    if (!buf) {
        printf("[FAIL] malloc read buffer\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    n = fuseg_read_file(path, buf, data_size);
    if (n < 0) {
        printf("[FAIL] read(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if ((size_t)n != data_size) {
        printf("[FAIL] read size mismatch: got=%d expected=%zu read_count=%u\n", n, data_size,
               read_count);
        goto fail;
    }
    for (size_t i = 0; i < data_size; i++) {
        char expected = (char)('A' + (i % 26));
        if (buf[i] != expected) {
            printf("[FAIL] read data mismatch at %zu: got=%d expected=%d\n", i, buf[i],
                   expected);
            goto fail;
        }
    }
    if (read_count != 2 || read_offsets[0] != 0 || read_offsets[1] != 4096 ||
        read_sizes[0] != 4096 || read_sizes[1] > 4096 || read_sizes[1] == 0) {
        printf("[FAIL] unexpected FUSE_READ split: count=%u off0=%llu size0=%u off1=%llu size1=%u\n",
               read_count, (unsigned long long)read_offsets[0], read_sizes[0],
               (unsigned long long)read_offsets[1], read_sizes[1]);
        goto fail;
    }

    free(buf);
    buf = NULL;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (buf) {
        free(buf);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_read_uses_open_fh_without_extra_open() {
    const char *mp = "/tmp/test_fuse_cached_read_fh";
    char path[256];
    char buf[32];
    int f = -1;
    ssize_t n = -1;
    ssize_t first_n = -1;

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
    volatile uint32_t read_count = 0;
    volatile uint64_t read_fhs[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.read_fhs = read_fhs;
    args.read_trace_capacity = 4;
    args.next_open_fh = 100;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
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

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    n = pread(f, buf, sizeof(buf), 0);
    if (n <= 0) {
        printf("[FAIL] first pread got=%zd errno=%d\n", n, errno);
        close(f);
        goto fail;
    }
    first_n = n;
    memset(buf, 0, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 0);
    close(f);
    f = -1;
    if (n != first_n) {
        printf("[FAIL] second pread got=%zd errno=%d\n", n, errno);
        goto fail;
    }
    if (open_count != 1 || read_count != 1 || read_fhs[0] != 100) {
        printf("[FAIL] cached read counters open=%u read=%u fh0=%llu\n", open_count,
               read_count, (unsigned long long)read_fhs[0]);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_short_read_updates_eof() {
    const char *mp = "/tmp/test_fuse_cached_short_read";
    char path[256];
    char buf[32];
    int f = -1;
    ssize_t n = -1;

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
    volatile uint32_t read_count = 0;
    volatile uint64_t read_offsets[4] = {0};
    volatile uint32_t read_sizes[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.read_count = &read_count;
    args.read_offsets = read_offsets;
    args.read_sizes = read_sizes;
    args.read_trace_capacity = 4;
    args.hello_data_size_override = 8192;
    args.hello_read_size_override = 5;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
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

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    memset(buf, 0x7f, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 0);
    if (n != 5 || memcmp(buf, "ABCDE", 5) != 0) {
        printf("[FAIL] short cached pread got=%zd data='%.*s' read=%u errno=%d\n", n, 5, buf,
               read_count, errno);
        goto fail;
    }
    memset(buf, 0x7f, sizeof(buf));
    n = pread(f, buf, sizeof(buf), 5);
    if (n != 0) {
        printf("[FAIL] EOF cached pread got=%zd read=%u errno=%d\n", n, read_count, errno);
        goto fail;
    }

    if (read_count != 1 || read_offsets[0] != 0 || read_sizes[0] != 4096) {
        printf("[FAIL] short read trace count=%u off0=%llu size0=%u\n", read_count,
               (unsigned long long)read_offsets[0], read_sizes[0]);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_cached_read_sees_write_through_update() {
    const char *mp = "/tmp/test_fuse_cached_read_write";
    char path[256];
    char buf[16];
    int f = -1;

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
    volatile uint32_t read_count = 0;
    volatile uint32_t write_count = 0;
    volatile uint64_t last_write_fh = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.enable_write_ops = 1;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.write_count = &write_count;
    args.last_write_fh = &last_write_fh;
    args.next_open_fh = 300;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
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

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] first cached pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    if (pwrite(f, "CACHE", 5, 0) != 5) {
        printf("[FAIL] pwrite CACHE: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "CACHE", 5) != 0) {
        printf("[FAIL] second cached pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    if (open_count != 1 || read_count != 1 || write_count != 1 || last_write_fh != 300) {
        printf("[FAIL] cached write counters open=%u read=%u write=%u wfh=%llu\n", open_count,
               read_count, write_count, (unsigned long long)last_write_fh);
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_fault_uses_open_fh_without_extra_open() {
    const char *mp = "/tmp/test_fuse_mmap_fh";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t child = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint64_t read_fhs[4];
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    child = fork();
    if (child < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (child == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.read_fhs = shared->read_fhs;
        child_args.read_trace_capacity = 4;
        child_args.next_open_fh = 200;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }
    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] mmap first byte got=%d\n", c);
        munmap(addr, 4096);
        close(f);
        goto fail;
    }
    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;

    if (shared->open_count != 1 || shared->read_count != 1 || shared->read_fhs[0] != 200) {
        printf("[FAIL] mmap counters open=%u read=%u fh0=%llu\n", shared->open_count,
               shared->read_count, (unsigned long long)shared->read_fhs[0]);
        goto fail;
    }

    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(child, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (child > 0) {
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_direct_io_read_bypasses_page_cache() {
    const char *mp = "/tmp/test_fuse_direct_read";
    char path[256];
    char buf[32];
    int f = -1;

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
    volatile uint32_t read_count = 0;
    volatile uint64_t read_fhs[4] = {0};

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.open_count = &open_count;
    args.read_count = &read_count;
    args.read_fhs = read_fhs;
    args.read_trace_capacity = 4;
    args.next_open_fh = 700;
    args.hello_open_out_flags = FOPEN_DIRECT_IO;

    pthread_t th;
    if (pthread_create(&th, NULL, fuse_daemon_thread, &args) != 0) {
        printf("[FAIL] pthread_create\n");
        close(fd);
        rmdir(mp);
        return -1;
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
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

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] first direct pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    memset(buf, 0, sizeof(buf));
    if (pread(f, buf, 5, 0) != 5 || memcmp(buf, "hello", 5) != 0) {
        printf("[FAIL] second direct pread got='%.*s' read=%u errno=%d\n", 5, buf, read_count,
               errno);
        goto fail;
    }
    close(f);
    f = -1;

    if (open_count != 1 || read_count != 2 || read_fhs[0] != 700 || read_fhs[1] != 700) {
        printf("[FAIL] direct read counters open=%u read=%u fh0=%llu fh1=%llu\n", open_count,
               read_count, (unsigned long long)read_fhs[0], (unsigned long long)read_fhs[1]);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_direct_io_mmap_policy() {
    const char *mp = "/tmp/test_fuse_direct_mmap";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    char warm = 0;
    pid_t child = -1;
    struct direct_mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_out_flags;
        volatile unsigned char first_byte;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
        volatile uint64_t read_fhs[4];
    };
    struct direct_mmap_shared_state *shared =
        (struct direct_mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                                MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    child = fork();
    if (child < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (child == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.read_fhs = shared->read_fhs;
        child_args.read_trace_capacity = 4;
        child_args.next_open_fh = 800;
        child_args.dynamic_hello_open_out_flags = &shared->open_out_flags;
        child_args.dynamic_hello_first_byte = &shared->first_byte;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    if (pread(f, &warm, 1, 0) != 1 || warm != 'h') {
        printf("[FAIL] warm cached read got=%d read=%u errno=%d\n", warm, shared->read_count,
               errno);
        goto fail;
    }
    close(f);
    f = -1;

    shared->open_out_flags = FOPEN_DIRECT_IO;
    shared->first_byte = 'Z';

    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] direct open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    errno = 0;
    addr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, f, 0);
    if (addr != MAP_FAILED) {
        printf("[FAIL] direct_io MAP_SHARED unexpectedly succeeded\n");
        munmap(addr, 4096);
        addr = MAP_FAILED;
        goto fail;
    }
    if (errno != ENODEV) {
        printf("[FAIL] direct_io MAP_SHARED errno=%d expected=%d\n", errno, ENODEV);
        goto fail;
    }

    addr = mmap(NULL, 4096, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] direct_io MAP_PRIVATE mmap: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    c = ((volatile char *)addr)[0];
    if (c != 'Z') {
        printf("[FAIL] direct_io MAP_PRIVATE first byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 2 || shared->read_count != 2 || shared->read_fhs[1] != 801) {
        printf("[FAIL] direct mmap counters open=%u read=%u fh0=%llu fh1=%llu\n",
               shared->open_count, shared->read_count, (unsigned long long)shared->read_fhs[0],
               (unsigned long long)shared->read_fhs[1]);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(child, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (child > 0) {
        kill(child, SIGTERM);
        waitpid(child, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_writable_mmap_fault_sigbus() {
    const char *mp = "/tmp/test_fuse_mmap_shared_write";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    pid_t daemon = -1;
    struct sigaction old_bus;
    bool bus_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigbus_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGBUS, &sa, &old_bus) != 0) {
        printf("[FAIL] sigaction(SIGBUS): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    bus_handler_installed = true;
    g_fuse_sigbus_seen = 0;
    if (sigsetjmp(g_fuse_sigbus_jmp, 1) == 0) {
        volatile char c = ((volatile char *)addr)[0];
        (void)c;
    }
    sigaction(SIGBUS, &old_bus, NULL);
    bus_handler_installed = false;
    if (!g_fuse_sigbus_seen) {
        printf("[FAIL] shared writable mmap access did not SIGBUS open=%u read=%u\n",
               shared->open_count, shared->read_count);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 0) {
        printf("[FAIL] shared writable mmap counters open=%u read=%u\n", shared->open_count,
               shared->read_count);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (bus_handler_installed) {
        sigaction(SIGBUS, &old_bus, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_mprotect_write_denied() {
    const char *mp = "/tmp/test_fuse_mmap_mprotect_write";
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, 4096, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'h') {
        printf("[FAIL] mmap first byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1) {
        printf("[FAIL] before mprotect counters open=%u read=%u\n", shared->open_count,
               shared->read_count);
        goto fail;
    }
    errno = 0;
    if (mprotect(addr, 4096, PROT_READ | PROT_WRITE) == 0) {
        printf("[FAIL] mprotect unexpectedly allowed shared writable FUSE mapping\n");
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1) {
        printf("[FAIL] after mprotect counters open=%u read=%u\n", shared->open_count,
               shared->read_count);
        goto fail;
    }

    munmap(addr, 4096);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (addr != MAP_FAILED) {
        munmap(addr, 4096);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_subrange_mprotect_failure_preserves_vma() {
    const char *mp = "/tmp/test_fuse_mmap_mprotect_subrange";
    const size_t page_size = 4096;
    const size_t map_len = page_size * 2;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_segv;
    bool segv_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.hello_data_size_override = map_len;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[0];
    if (c != 'A') {
        printf("[FAIL] first page byte got=%d\n", c);
        goto fail;
    }
    c = ((volatile char *)addr)[page_size];
    if (c != 'O') {
        printf("[FAIL] second page byte got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 2) {
        printf("[FAIL] before subrange mprotect counters open=%u read=%u\n",
               shared->open_count, shared->read_count);
        goto fail;
    }
    if (mprotect((char *)addr + page_size, page_size, PROT_READ | PROT_WRITE) == 0) {
        printf("[FAIL] subrange mprotect unexpectedly allowed shared writable FUSE mapping\n");
        goto fail;
    }
    if (mprotect(addr, page_size, PROT_NONE) != 0) {
        printf("[FAIL] mprotect(PROT_NONE first page): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigsegv_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, &old_segv) != 0) {
        printf("[FAIL] sigaction(SIGSEGV): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    segv_handler_installed = true;
    g_fuse_sigsegv_seen = 0;
    if (sigsetjmp(g_fuse_sigsegv_jmp, 1) == 0) {
        c = ((volatile char *)addr)[0];
        (void)c;
    }
    sigaction(SIGSEGV, &old_segv, NULL);
    segv_handler_installed = false;
    if (!g_fuse_sigsegv_seen) {
        printf("[FAIL] first page remained readable after PROT_NONE\n");
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (segv_handler_installed) {
        sigaction(SIGSEGV, &old_segv, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_shared_mmap_unfaulted_mprotect_prot_none() {
    const char *mp = "/tmp/test_fuse_mmap_unfaulted_mprotect";
    const size_t page_size = 4096;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_segv;
    bool segv_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.hello_data_size_override = page_size;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, page_size, PROT_READ, MAP_SHARED, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 0) {
        printf("[FAIL] before unfaulted mprotect counters open=%u read=%u\n",
               shared->open_count, shared->read_count);
        goto fail;
    }
    if (mprotect(addr, page_size, PROT_NONE) != 0) {
        printf("[FAIL] mprotect(PROT_NONE unfaulted): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigsegv_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGSEGV, &sa, &old_segv) != 0) {
        printf("[FAIL] sigaction(SIGSEGV): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    segv_handler_installed = true;
    g_fuse_sigsegv_seen = 0;
    if (sigsetjmp(g_fuse_sigsegv_jmp, 1) == 0) {
        c = ((volatile char *)addr)[0];
        (void)c;
    }
    sigaction(SIGSEGV, &old_segv, NULL);
    segv_handler_installed = false;
    if (!g_fuse_sigsegv_seen) {
        printf("[FAIL] unfaulted PROT_NONE mapping remained readable\n");
        goto fail;
    }
    if (shared->read_count != 0) {
        printf("[FAIL] unfaulted PROT_NONE triggered read_count=%u\n", shared->read_count);
        goto fail;
    }

    munmap(addr, page_size);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (segv_handler_installed) {
        sigaction(SIGSEGV, &old_segv, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, page_size);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_mmap_truncate_unmaps_stale_page() {
    const char *mp = "/tmp/test_fuse_mmap_truncate";
    const size_t page_size = 4096;
    const size_t map_len = page_size * 2;
    char path[256];
    int f = -1;
    void *addr = MAP_FAILED;
    volatile char c = 0;
    pid_t daemon = -1;
    struct sigaction old_bus;
    bool bus_handler_installed = false;
    struct mmap_shared_state {
        volatile int stop;
        volatile int init_done;
        volatile uint32_t open_count;
        volatile uint32_t read_count;
    };
    struct mmap_shared_state *shared =
        (struct mmap_shared_state *)mmap(NULL, sizeof(*shared), PROT_READ | PROT_WRITE,
                                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (shared == MAP_FAILED) {
        printf("[FAIL] mmap(shared counters): %s (errno=%d)\n", strerror(errno), errno);
        return -1;
    }
    memset(shared, 0, sizeof(*shared));

    if (ensure_dir(mp) != 0) {
        printf("[FAIL] ensure_dir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
        printf("[FAIL] open(/dev/fuse): %s (errno=%d)\n", strerror(errno), errno);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }

    daemon = fork();
    if (daemon < 0) {
        printf("[FAIL] fork fuse daemon: %s (errno=%d)\n", strerror(errno), errno);
        close(fd);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (daemon == 0) {
        struct fuse_daemon_args child_args;
        memset(&child_args, 0, sizeof(child_args));
        child_args.fd = fd;
        child_args.stop = &shared->stop;
        child_args.init_done = &shared->init_done;
        child_args.stop_on_destroy = 1;
        child_args.enable_write_ops = 1;
        child_args.open_count = &shared->open_count;
        child_args.read_count = &shared->read_count;
        child_args.hello_data_size_override = map_len;
        fuse_daemon_thread(&child_args);
        _exit(0);
    }

    char opts[256];
    snprintf(opts, sizeof(opts), "fd=%d,rootmode=040755,user_id=0,group_id=0,max_read=4096",
             fd);
    if (mount("none", mp, "fuse", 0, opts) != 0) {
        printf("[FAIL] mount(fuse): %s (errno=%d)\n", strerror(errno), errno);
        shared->stop = 1;
        close(fd);
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
        munmap(shared, sizeof(*shared));
        rmdir(mp);
        return -1;
    }
    if (fuseg_wait_init(&shared->init_done) != 0) {
        printf("[FAIL] init handshake timeout\n");
        goto fail;
    }

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDWR);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }
    addr = mmap(NULL, map_len, PROT_READ, MAP_PRIVATE, f, 0);
    if (addr == MAP_FAILED) {
        printf("[FAIL] mmap(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        close(f);
        goto fail;
    }

    c = ((volatile char *)addr)[page_size];
    if (c != 'O') {
        printf("[FAIL] second page byte before truncate got=%d\n", c);
        goto fail;
    }
    if (shared->open_count != 1 || shared->read_count != 1) {
        printf("[FAIL] before truncate counters open=%u read=%u\n", shared->open_count,
               shared->read_count);
        goto fail;
    }
    if (ftruncate(f, page_size) != 0) {
        printf("[FAIL] ftruncate: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    struct sigaction sa;
    memset(&sa, 0, sizeof(sa));
    sa.sa_handler = fuse_sigbus_longjmp_handler;
    sigemptyset(&sa.sa_mask);
    if (sigaction(SIGBUS, &sa, &old_bus) != 0) {
        printf("[FAIL] sigaction(SIGBUS): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    bus_handler_installed = true;
    g_fuse_sigbus_seen = 0;
    if (sigsetjmp(g_fuse_sigbus_jmp, 1) == 0) {
        c = ((volatile char *)addr)[page_size];
        (void)c;
    }
    sigaction(SIGBUS, &old_bus, NULL);
    bus_handler_installed = false;
    if (!g_fuse_sigbus_seen) {
        printf("[FAIL] truncated second page remained readable read=%u\n", shared->read_count);
        goto fail;
    }
    if (shared->read_count != 1) {
        printf("[FAIL] truncated EOF fault issued extra FUSE_READ count=%u\n", shared->read_count);
        goto fail;
    }

    munmap(addr, map_len);
    addr = MAP_FAILED;
    close(f);
    f = -1;
    umount(mp);
    shared->stop = 1;
    close(fd);
    waitpid(daemon, NULL, 0);
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return 0;

fail:
    if (bus_handler_installed) {
        sigaction(SIGBUS, &old_bus, NULL);
    }
    if (addr != MAP_FAILED) {
        munmap(addr, map_len);
    }
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    shared->stop = 1;
    close(fd);
    if (daemon > 0) {
        kill(daemon, SIGTERM);
        waitpid(daemon, NULL, 0);
    }
    munmap(shared, sizeof(*shared));
    rmdir(mp);
    return -1;
}

static int ext_test_fadvise_without_page_cache() {
    const char *mp = "/tmp/test_fuse_fadvise";
    char path[256];
    int f = -1;
    const int advices[] = {
        POSIX_FADV_NORMAL,     POSIX_FADV_RANDOM, POSIX_FADV_SEQUENTIAL,
        POSIX_FADV_WILLNEED,   POSIX_FADV_DONTNEED,
        POSIX_FADV_NOREUSE,
    };

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

    snprintf(path, sizeof(path), "%s/hello.txt", mp);
    f = open(path, O_RDONLY);
    if (f < 0) {
        printf("[FAIL] open(%s): %s (errno=%d)\n", path, strerror(errno), errno);
        goto fail;
    }

    for (size_t i = 0; i < sizeof(advices) / sizeof(advices[0]); i++) {
        int rc = posix_fadvise(f, 0, 0, advices[i]);
        if (rc != 0) {
            printf("[FAIL] posix_fadvise(advice=%d): rc=%d\n", advices[i], rc);
            goto fail;
        }
    }

    if (posix_fadvise(f, 0, -1, POSIX_FADV_NORMAL) != EINVAL) {
        printf("[FAIL] posix_fadvise negative len should return EINVAL\n");
        goto fail;
    }

    close(f);
    f = -1;
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (f >= 0) {
        close(f);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_mount_on_fuse_dir_uses_namespace_path() {
    const char *mp = "/tmp/test_fuse_mount_target";
    char dir_path[512];
    char marker_path[1024];
    int ramfs_mounted = 0;

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
    args.enable_write_ops = 1;
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

    snprintf(dir_path, sizeof(dir_path), "%s/ramfs_target", mp);
    if (mkdir(dir_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail;
    }

    if (mount("", dir_path, "ramfs", 0, NULL) != 0) {
        printf("[FAIL] mount(ramfs on fuse dir): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    ramfs_mounted = 1;

    snprintf(marker_path, sizeof(marker_path), "%s/marker", dir_path);
    if (fuseg_write_file(marker_path, "mounted") != 0) {
        printf("[FAIL] write marker under ramfs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    if (umount(dir_path) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail_no_ramfs_umount;
    }
    ramfs_mounted = 0;
    if (rmdir(dir_path) != 0) {
        printf("[FAIL] rmdir(%s): %s (errno=%d)\n", dir_path, strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (ramfs_mounted) {
        umount(dir_path);
    }
fail_no_ramfs_umount:
    rmdir(dir_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_rename_updates_fuse_dir_cwd_path() {
    const char *mp = "/tmp/test_fuse_rename_path";
    char old_path[512];
    char new_path[512];
    char cwd[512];
    int dir_fd = -1;
    int ramfs_mounted = 0;

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
    args.enable_write_ops = 1;
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

    snprintf(old_path, sizeof(old_path), "%s/old_dir", mp);
    snprintf(new_path, sizeof(new_path), "%s/new_dir", mp);
    if (mkdir(old_path, 0755) != 0) {
        printf("[FAIL] mkdir(%s): %s (errno=%d)\n", old_path, strerror(errno), errno);
        goto fail;
    }
    dir_fd = open(old_path, O_RDONLY | O_DIRECTORY);
    if (dir_fd < 0) {
        printf("[FAIL] open dir fd %s: %s (errno=%d)\n", old_path, strerror(errno), errno);
        goto fail;
    }
    if (rename(old_path, new_path) != 0) {
        printf("[FAIL] rename(%s -> %s): %s (errno=%d)\n", old_path, new_path, strerror(errno),
               errno);
        goto fail;
    }
    if (fchdir(dir_fd) != 0) {
        printf("[FAIL] fchdir renamed dir fd: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (!getcwd(cwd, sizeof(cwd))) {
        printf("[FAIL] getcwd after rename: %s (errno=%d)\n", strerror(errno), errno);
        goto fail_chdir_root;
    }
    if (strcmp(cwd, new_path) != 0) {
        printf("[FAIL] getcwd after rename: got '%s', want '%s'\n", cwd, new_path);
        goto fail_chdir_root;
    }
    if (chdir("/") != 0) {
        printf("[FAIL] chdir(/): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(dir_fd);
    dir_fd = -1;

    if (mount("", new_path, "ramfs", 0, NULL) != 0) {
        printf("[FAIL] mount(ramfs on renamed fuse dir): %s (errno=%d)\n", strerror(errno),
               errno);
        goto fail;
    }
    ramfs_mounted = 1;
    if (umount(new_path) != 0) {
        printf("[FAIL] umount(%s): %s (errno=%d)\n", new_path, strerror(errno), errno);
        goto fail_no_ramfs_umount;
    }
    ramfs_mounted = 0;
    if (rmdir(new_path) != 0) {
        printf("[FAIL] rmdir(%s): %s (errno=%d)\n", new_path, strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail_chdir_root:
    {
        int ignored_chdir = chdir("/");
        (void)ignored_chdir;
    }
fail:
    if (dir_fd >= 0) {
        close(dir_fd);
    }
    if (ramfs_mounted) {
        umount(new_path);
    }
fail_no_ramfs_umount:
    rmdir(new_path);
    rmdir(old_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_readdirplus_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_readdirplus_generation";
    char file_path[512];
    int old_fd = -1;
    int new_fd = -1;
    DIR *dir = NULL;
    int saw = 0;

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
    volatile uint32_t readdirplus_count = 0;

    struct fuse_daemon_args args;
    memset(&args, 0, sizeof(args));
    args.fd = fd;
    args.stop = &stop;
    args.init_done = &init_done;
    args.stop_on_destroy = 1;
    args.readdirplus_count = &readdirplus_count;
    args.force_opendir_enosys = 1;
    args.init_out_flags_override =
        FUSE_INIT_EXT | FUSE_MAX_PAGES | FUSE_NO_OPENDIR_SUPPORT | FUSE_DO_READDIRPLUS;

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

    snprintf(file_path, sizeof(file_path), "%s/hello.txt", mp);
    old_fd = open(file_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    char buf[64];
    if (read(old_fd, buf, sizeof(buf)) <= 0) {
        printf("[FAIL] initial read old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    args.fs.nodes[1].generation = 2;
    dir = opendir(mp);
    if (!dir) {
        printf("[FAIL] opendir(%s): %s (errno=%d)\n", mp, strerror(errno), errno);
        goto fail;
    }
    struct dirent *de;
    while ((de = readdir(dir)) != NULL) {
        if (strcmp(de->d_name, "hello.txt") == 0) {
            saw = 1;
        }
    }
    closedir(dir);
    dir = NULL;
    if (!saw || readdirplus_count == 0) {
        printf("[FAIL] expected hello.txt from READDIRPLUS, saw=%d count=%u\n", saw,
               readdirplus_count);
        goto fail;
    }

    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd read unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;

    new_fd = open(file_path, O_RDONLY);
    if (new_fd < 0) {
        printf("[FAIL] open fresh hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (read(new_fd, buf, sizeof(buf)) <= 0) {
        printf("[FAIL] read fresh hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    close(new_fd);
    new_fd = -1;

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (dir) {
        closedir(dir);
    }
    if (new_fd >= 0) {
        close(new_fd);
    }
    if (old_fd >= 0) {
        close(old_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_create_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_create_generation";
    char old_path[512];
    char new_path[512];
    int old_fd = -1;
    int new_fd = -1;

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
    args.enable_write_ops = 1;
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

    snprintf(old_path, sizeof(old_path), "%s/hello.txt", mp);
    snprintf(new_path, sizeof(new_path), "%s/reused.txt", mp);
    old_fd = open(old_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (unlink(old_path) != 0) {
        printf("[FAIL] unlink old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    args.create_reuse_nodeid = 2;
    args.create_generation_override = 2;
    new_fd = open(new_path, O_CREAT | O_RDWR, 0644);
    if (new_fd < 0) {
        printf("[FAIL] create reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    char buf[64];
    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd after create unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;
    close(new_fd);
    new_fd = -1;

    if (unlink(new_path) != 0) {
        printf("[FAIL] unlink reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (new_fd >= 0) {
        close(new_fd);
    }
    if (old_fd >= 0) {
        close(old_fd);
    }
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_link_generation_mismatch_stales_old_node() {
    const char *mp = "/tmp/test_fuse_link_generation";
    char old_path[512];
    char hard_path[512];
    int old_fd = -1;

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
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.link_reuse_old_nodeid = 1;
    args.link_generation_override = 2;

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

    snprintf(old_path, sizeof(old_path), "%s/hello.txt", mp);
    snprintf(hard_path, sizeof(hard_path), "%s/hard.txt", mp);
    old_fd = open(old_path, O_RDONLY);
    if (old_fd < 0) {
        printf("[FAIL] open old hello: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (link(old_path, hard_path) != 0) {
        printf("[FAIL] link reused node: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }

    char buf[64];
    errno = 0;
    if (pread(old_fd, buf, sizeof(buf), 0) >= 0) {
        printf("[FAIL] stale old fd after link unexpectedly succeeded\n");
        goto fail;
    }
    close(old_fd);
    old_fd = -1;

    unlink(hard_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail:
    if (old_fd >= 0) {
        close(old_fd);
    }
    unlink(hard_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return -1;
}

static int ext_test_rename_replace_clears_old_target_path() {
    const char *mp = "/tmp/test_fuse_rename_replace";
    char old_path[512];
    char victim_path[512];
    char cwd[512];
    int old_fd = -1;
    int victim_fd = -1;

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
    args.enable_write_ops = 1;
    args.stop_on_destroy = 1;
    args.allow_rename_replace = 1;

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

    snprintf(old_path, sizeof(old_path), "%s/old_dir", mp);
    snprintf(victim_path, sizeof(victim_path), "%s/victim_dir", mp);
    if (mkdir(old_path, 0755) != 0 || mkdir(victim_path, 0755) != 0) {
        printf("[FAIL] mkdir rename-replace dirs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    old_fd = open(old_path, O_RDONLY | O_DIRECTORY);
    victim_fd = open(victim_path, O_RDONLY | O_DIRECTORY);
    if (old_fd < 0 || victim_fd < 0) {
        printf("[FAIL] open rename-replace dirs: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (rename(old_path, victim_path) != 0) {
        printf("[FAIL] rename replace: %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    if (fchdir(old_fd) != 0 || !getcwd(cwd, sizeof(cwd)) || strcmp(cwd, victim_path) != 0) {
        printf("[FAIL] source fd path after rename replace: cwd='%s' errno=%d (%s)\n", cwd, errno,
               strerror(errno));
        goto fail_chdir_root;
    }
    if (chdir("/") != 0) {
        printf("[FAIL] chdir(/): %s (errno=%d)\n", strerror(errno), errno);
        goto fail;
    }
    errno = 0;
    if (fchdir(victim_fd) == 0) {
        printf("[FAIL] replaced target fd still resolved to a path\n");
        goto fail_chdir_root;
    }
    close(old_fd);
    close(victim_fd);
    old_fd = -1;
    victim_fd = -1;

    rmdir(victim_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    rmdir(mp);
    return 0;

fail_chdir_root:
    {
        int ignored_chdir = chdir("/");
        (void)ignored_chdir;
    }
fail:
    if (old_fd >= 0) {
        close(old_fd);
    }
    if (victim_fd >= 0) {
        close(victim_fd);
    }
    rmdir(victim_path);
    rmdir(old_path);
    umount(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
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

TEST(FuseExtended, OpenReturnsZeroFhIsValid) {
    ASSERT_EQ(0, ext_test_open_zero_fh_valid());
}

TEST(FuseExtended, LargeReadSplitsOverMaxWrite) {
    ASSERT_EQ(0, ext_test_large_read_over_max_write());
}

TEST(FuseExtended, CachedReadUsesOpenFhWithoutExtraOpen) {
    ASSERT_EQ(0, ext_test_cached_read_uses_open_fh_without_extra_open());
}

TEST(FuseExtended, CachedShortReadUpdatesEof) {
    ASSERT_EQ(0, ext_test_cached_short_read_updates_eof());
}

TEST(FuseExtended, CachedReadSeesWriteThroughUpdate) {
    ASSERT_EQ(0, ext_test_cached_read_sees_write_through_update());
}

TEST(FuseExtended, MmapFaultUsesOpenFhWithoutExtraOpen) {
    ASSERT_EQ(0, ext_test_mmap_fault_uses_open_fh_without_extra_open());
}

TEST(FuseExtended, DirectIoReadBypassesPageCache) {
    ASSERT_EQ(0, ext_test_direct_io_read_bypasses_page_cache());
}

TEST(FuseExtended, DirectIoMmapPolicy) {
    ASSERT_EQ(0, ext_test_direct_io_mmap_policy());
}

TEST(FuseExtended, SharedWritableMmapFaultSigbus) {
    ASSERT_EQ(0, ext_test_shared_writable_mmap_fault_sigbus());
}

TEST(FuseExtended, SharedMmapMprotectWriteDenied) {
    ASSERT_EQ(0, ext_test_shared_mmap_mprotect_write_denied());
}

TEST(FuseExtended, SharedMmapSubrangeMprotectFailurePreservesVma) {
    ASSERT_EQ(0, ext_test_shared_mmap_subrange_mprotect_failure_preserves_vma());
}

TEST(FuseExtended, SharedMmapUnfaultedMprotectProtNone) {
    ASSERT_EQ(0, ext_test_shared_mmap_unfaulted_mprotect_prot_none());
}

TEST(FuseExtended, MmapTruncateUnmapsStalePage) {
    ASSERT_EQ(0, ext_test_mmap_truncate_unmaps_stale_page());
}

TEST(FuseExtended, FadviseWithoutPageCacheSucceeds) {
    ASSERT_EQ(0, ext_test_fadvise_without_page_cache());
}

TEST(FuseExtended, MountRamfsOnFuseDirectoryUsesNamespacePath) {
    ASSERT_EQ(0, ext_test_mount_on_fuse_dir_uses_namespace_path());
}

TEST(FuseExtended, RenameUpdatesFuseDirectoryCwdPath) {
    ASSERT_EQ(0, ext_test_rename_updates_fuse_dir_cwd_path());
}

TEST(FuseExtended, ReaddirplusGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_readdirplus_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, CreateGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_create_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, LinkGenerationMismatchStalesOldNode) {
    ASSERT_EQ(0, ext_test_link_generation_mismatch_stales_old_node());
}

TEST(FuseExtended, RenameReplaceClearsOldTargetPath) {
    ASSERT_EQ(0, ext_test_rename_replace_clears_old_target_path());
}

TEST(FuseExtended, NoOpenFsyncUsesZeroFh) {
    ASSERT_EQ(0, ext_test_noopen_fsync_uses_zero_fh());
}

TEST(FuseExtended, OpenFlagsMatchLinuxMask) {
    ASSERT_EQ(0, ext_test_open_release_flags_match_linux());
}

TEST(FuseExtended, AtomicOTruncUsesOpenWithoutSetattr) {
    ASSERT_EQ(0, ext_test_atomic_otrunc_uses_open_without_setattr());
}

TEST(FuseExtended, InitRequestsLinuxNoOpenSupport) {
    ASSERT_EQ(0, ext_test_init_requests_linux_no_open_support());
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
