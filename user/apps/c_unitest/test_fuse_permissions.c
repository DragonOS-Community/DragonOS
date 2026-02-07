/**
 * @file test_fuse_permissions.c
 * @brief Phase E test: allow_other/default_permissions + mount owner restriction.
 */

#include "fuse_test_simplefs.h"

#include <sys/wait.h>

static int wait_init(volatile int *init_done) {
    for (int i = 0; i < 200; i++) {
        if (*init_done)
            return 0;
        usleep(10 * 1000);
    }
    errno = ETIMEDOUT;
    return -1;
}

static int run_child_drop_priv_and_stat(const char *mp, int expect_errno, int expect_success) {
    pid_t pid = fork();
    if (pid < 0) {
        return -1;
    }
    if (pid == 0) {
        /* child */
        (void)setgid(1000);
        (void)setuid(1000);

        struct stat st;
        int r = stat(mp, &st);
        if (expect_success) {
            if (r != 0)
                _exit(10);
            /* Also check file read works */
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

        if (r == 0)
            _exit(20);
        if (errno != expect_errno)
            _exit(21);
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

static int run_one(const char *mp, const char *opts, uint32_t root_mode_override,
                   uint32_t hello_mode_override, int expect_errno, int expect_success) {
    if (ensure_dir(mp) != 0) {
        return -1;
    }

    int fd = open("/dev/fuse", O_RDWR);
    if (fd < 0) {
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
        return -1;
    }

    char full_opts[512];
    snprintf(full_opts, sizeof(full_opts), "fd=%d,%s", fd, opts);
    if (mount("none", mp, "fuse", 0, full_opts) != 0) {
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return -1;
    }

    if (wait_init(&init_done) != 0) {
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return -1;
    }

    if (run_child_drop_priv_and_stat(mp, expect_errno, expect_success) != 0) {
        umount(mp);
        stop = 1;
        close(fd);
        pthread_join(th, NULL);
        return -1;
    }

    umount(mp);
    rmdir(mp);
    stop = 1;
    close(fd);
    pthread_join(th, NULL);
    return 0;
}

int main(void) {
    /* Build opts strings with the real fd inside run_one() */
    /* - For deny cases, use a root dir with no permissions to exercise default_permissions. */
    const uint32_t DIR_NO_PERM = 0040000;
    const uint32_t REG_NO_PERM = 0100000;

    /* Case A: mount owner restriction (no allow_other) */
    {
        const char *mp = "/tmp/test_fuse_perm_owner";
        if (run_one(mp, "rootmode=040755,user_id=0,group_id=0", 0, 0, EACCES, 0) != 0) {
            printf("[FAIL] mount owner restriction\n");
            return 1;
        }
    }

    /* Case B: allow_other + default_permissions: DAC should deny with no-perm root */
    {
        const char *mp = "/tmp/test_fuse_perm_default";
        if (run_one(mp,
                    "rootmode=040000,user_id=0,group_id=0,allow_other,default_permissions",
                    DIR_NO_PERM, REG_NO_PERM, EACCES, 0)
            != 0) {
            printf("[FAIL] default_permissions deny\n");
            return 1;
        }
    }

    /* Case C: allow_other without default_permissions: bypass DAC, should succeed */
    {
        const char *mp = "/tmp/test_fuse_perm_remote";
        if (run_one(mp, "rootmode=040000,user_id=0,group_id=0,allow_other", DIR_NO_PERM,
                    REG_NO_PERM, 0, 1)
            != 0) {
            printf("[FAIL] remote permission model allow\n");
            return 1;
        }
    }

    printf("[PASS] fuse_permissions\n");
    return 0;
}
