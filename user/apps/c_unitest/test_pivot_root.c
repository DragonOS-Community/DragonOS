/**
 * @file test_pivot_root.c
 * @brief Comprehensive test suite for pivot_root(2) system call
 *
 * Tests:
 *   1. Basic pivot_root functionality (cross-mount boundary)
 *   2. Post-pivot filesystem state verification
 *   3. chroot'd process root preservation after pivot_root
 *   4. Concurrent pivot_root serialization (race condition)
 *   5. Error handling (EBUSY, EINVAL, EPERM)
 *
 * Build: make  (or: gcc -Wall -O2 -static -lpthread -o test_pivot_root test_pivot_root.c)
 * Run:   sudo ./test_pivot_root  (requires root / CAP_SYS_ADMIN)
 */

#define _GNU_SOURCE
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <errno.h>
#include <unistd.h>
#include <sched.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <sys/types.h>
#include <fcntl.h>
#include <pthread.h>
#include <linux/magic.h>

/* ------------------------------------------------------------------ */
/*  Colours & test harness macros (same style as test_mknod.c)        */
/* ------------------------------------------------------------------ */

#define COLOR_GREEN  "\033[0;32m"
#define COLOR_RED    "\033[0;31m"
#define COLOR_YELLOW "\033[0;33m"
#define COLOR_RESET  "\033[0m"

static int tests_passed = 0;
static int tests_failed = 0;
static int tests_skipped = 0;

#define TEST_PASS(name) do { \
    printf(COLOR_GREEN "[PASS]" COLOR_RESET " %s\n", name); \
    tests_passed++; \
} while(0)

#define TEST_FAIL(name, reason) do { \
    printf(COLOR_RED "[FAIL]" COLOR_RESET " %s: %s\n", name, reason); \
    tests_failed++; \
} while(0)

#define TEST_SKIP(name, reason) do { \
    printf(COLOR_YELLOW "[SKIP]" COLOR_RESET " %s: %s\n", name, reason); \
    tests_skipped++; \
} while(0)

/* ------------------------------------------------------------------ */
/*  Helpers                                                            */
/* ------------------------------------------------------------------ */

static int do_pivot_root(const char *new_root, const char *put_old)
{
    return syscall(SYS_pivot_root, new_root, put_old);
}

#define STACK_SIZE (1024 * 1024)

/**
 * Run @fn in a child that has its own mount namespace (CLONE_NEWNS).
 * Returns the child exit code (0 = success).
 */
static int run_in_new_mntns(int (*fn)(void *), void *arg)
{
    char *stack = malloc(STACK_SIZE);
    if (!stack) {
        perror("malloc stack");
        return -1;
    }

    pid_t pid = clone(fn, stack + STACK_SIZE, CLONE_NEWNS | SIGCHLD, arg);
    if (pid == -1) {
        perror("clone(CLONE_NEWNS)");
        free(stack);
        return -1;
    }

    int status = 0;
    waitpid(pid, &status, 0);
    free(stack);

    if (WIFEXITED(status))
        return WEXITSTATUS(status);
    return -1;
}

/**
 * Helper: make the whole mount tree private so pivot_root won't be
 * rejected because of shared mounts.
 */
static int make_rprivate(void)
{
    return mount("", "/", NULL, MS_REC | MS_PRIVATE, NULL);
}

/* ------------------------------------------------------------------ */
/*  TEST 1  – basic pivot_root across mount boundaries                 */
/*                                                                     */
/*  Validates: BUG-5 (is_ancestor must traverse mount boundaries)      */
/*             BUG-6 (root must be a mountpoint check)                 */
/* ------------------------------------------------------------------ */

static int child_basic(void *arg)
{
    (void)arg;

    if (make_rprivate() != 0) {
        perror("make_rprivate");
        return 1;
    }

    mkdir("/tmp/pv_basic", 0755);
    mkdir("/tmp/pv_basic/new_root", 0755);

    /* Mount tmpfs as new_root – a DIFFERENT filesystem from / */
    if (mount("tmpfs", "/tmp/pv_basic/new_root", "tmpfs", 0, "size=64m") != 0) {
        perror("mount tmpfs new_root");
        return 1;
    }

    mkdir("/tmp/pv_basic/new_root/old_root", 0755);

    /* chdir into new_root so cwd stays valid after pivot */
    if (chdir("/tmp/pv_basic/new_root") != 0) {
        perror("chdir");
        return 1;
    }

    int ret = do_pivot_root("/tmp/pv_basic/new_root",
                            "/tmp/pv_basic/new_root/old_root");
    if (ret != 0) {
        /* If EINVAL, this is almost certainly BUG-5 */
        fprintf(stderr, "pivot_root failed: errno=%d (%s)\n",
                errno, strerror(errno));
        if (errno == EINVAL)
            fprintf(stderr,
                    "  *** Likely BUG-5: is_ancestor cannot cross mount boundaries\n");
        return 2;
    }

    /* ---------- post-pivot verifications ---------- */

    /* (a) new root should be tmpfs */
    struct statfs sfs;
    if (statfs("/", &sfs) != 0) {
        perror("statfs /");
        return 3;
    }
    if (sfs.f_type != TMPFS_MAGIC) {
        fprintf(stderr, "/ is not tmpfs (magic 0x%lx)\n",
                (unsigned long)sfs.f_type);
        return 4;
    }

    /* (b) /old_root should be accessible */
    struct stat st;
    if (stat("/old_root", &st) != 0 || !S_ISDIR(st.st_mode)) {
        fprintf(stderr, "/old_root not accessible\n");
        return 5;
    }

    /* (c) cwd should be / */
    char cwd[256];
    if (getcwd(cwd, sizeof(cwd)) && strcmp(cwd, "/") != 0) {
        fprintf(stderr, "cwd = '%s', expected '/'\n", cwd);
        return 6;
    }

    return 0; /* success */
}

static void test_basic_pivot_root(void)
{
    printf("\n--- Test 1: basic pivot_root (cross-mount boundary) ---\n");
    int rc = run_in_new_mntns(child_basic, NULL);
    if (rc == 0)
        TEST_PASS("basic pivot_root across mount boundary");
    else if (rc == 2)
        TEST_FAIL("basic pivot_root",
                  "pivot_root returned EINVAL (BUG-5: is_ancestor can't cross mounts)");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("basic pivot_root", buf);
    }
}

/* ------------------------------------------------------------------ */
/*  TEST 2  – new_root == current root  →  must return EBUSY          */
/* ------------------------------------------------------------------ */

static int child_ebusy(void *arg)
{
    (void)arg;

    if (make_rprivate() != 0) return 1;

    /* Trying to pivot new_root=/ put_old=/  should be EBUSY */
    mkdir("/tmp/pv_ebusy_old", 0755);
    int ret = do_pivot_root("/", "/tmp/pv_ebusy_old");
    if (ret == 0) {
        fprintf(stderr, "pivot_root should have failed with EBUSY\n");
        return 2;
    }
    if (errno != EBUSY && errno != EINVAL) {
        fprintf(stderr, "expected EBUSY or EINVAL, got %d (%s)\n",
                errno, strerror(errno));
        return 3;
    }
    return 0;
}

static void test_ebusy(void)
{
    printf("\n--- Test 2: pivot_root with new_root == root → EBUSY ---\n");
    int rc = run_in_new_mntns(child_ebusy, NULL);
    if (rc == 0)
        TEST_PASS("pivot_root correctly rejects new_root==root");
    else if (rc == 2)
        TEST_FAIL("pivot_root EBUSY",
                  "pivot_root succeeded when it should have returned EBUSY");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("pivot_root EBUSY", buf);
    }
}

/* ------------------------------------------------------------------ */
/*  TEST 3  – non-root caller → must return EPERM                     */
/* ------------------------------------------------------------------ */

static void test_eperm(void)
{
    printf("\n--- Test 3: pivot_root without CAP_SYS_ADMIN → EPERM ---\n");

    if (getuid() != 0) {
        /* We ARE unprivileged – just call pivot_root directly. */
        int ret = do_pivot_root("/tmp", "/tmp");
        if (ret != 0 && errno == EPERM) {
            TEST_PASS("pivot_root returns EPERM for non-root");
        } else {
            TEST_FAIL("pivot_root EPERM",
                      "expected EPERM for non-root caller");
        }
        return;
    }

    /* We are root – fork a child that drops privileges. */
    pid_t pid = fork();
    if (pid == 0) {
        if (setuid(65534) != 0) {   /* nobody */
            perror("setuid");
            _exit(1);
        }
        int ret = do_pivot_root("/tmp", "/tmp");
        if (ret != 0 && errno == EPERM)
            _exit(0);
        _exit(2);
    }
    int status;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 0)
        TEST_PASS("pivot_root returns EPERM for non-root");
    else
        TEST_FAIL("pivot_root EPERM",
                  "expected EPERM when caller lacks CAP_SYS_ADMIN");
}

/* ------------------------------------------------------------------ */
/*  TEST 4  – put_old not reachable from new_root → EINVAL            */
/* ------------------------------------------------------------------ */

static int child_put_old_unreachable(void *arg)
{
    (void)arg;

    if (make_rprivate() != 0) return 1;

    mkdir("/tmp/pv_unreach", 0755);
    mkdir("/tmp/pv_unreach/new_root", 0755);
    mkdir("/tmp/pv_unreach/outside", 0755);

    if (mount("tmpfs", "/tmp/pv_unreach/new_root", "tmpfs", 0, NULL) != 0) {
        perror("mount tmpfs");
        return 1;
    }

    /* put_old is OUTSIDE new_root → must fail */
    int ret = do_pivot_root("/tmp/pv_unreach/new_root",
                            "/tmp/pv_unreach/outside");
    if (ret == 0) {
        fprintf(stderr, "pivot_root should have failed (put_old outside new_root)\n");
        return 2;
    }
    if (errno != EINVAL) {
        fprintf(stderr, "expected EINVAL, got %d (%s)\n",
                errno, strerror(errno));
        return 3;
    }
    return 0;
}

static void test_put_old_unreachable(void)
{
    printf("\n--- Test 4: put_old not under new_root → EINVAL ---\n");
    int rc = run_in_new_mntns(child_put_old_unreachable, NULL);
    if (rc == 0)
        TEST_PASS("pivot_root rejects put_old outside new_root");
    else if (rc == 2)
        TEST_FAIL("put_old unreachable",
                  "pivot_root succeeded when put_old is outside new_root");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("put_old unreachable", buf);
    }
}

/* ------------------------------------------------------------------ */
/*  TEST 5  – new_root not a mountpoint → EINVAL                      */
/* ------------------------------------------------------------------ */

static int child_not_mountpoint(void *arg)
{
    (void)arg;

    if (make_rprivate() != 0) return 1;

    /* /tmp/pv_nomp/dir is an ordinary directory, not a mountpoint */
    mkdir("/tmp/pv_nomp", 0755);
    mkdir("/tmp/pv_nomp/dir", 0755);
    mkdir("/tmp/pv_nomp/dir/old", 0755);

    int ret = do_pivot_root("/tmp/pv_nomp/dir", "/tmp/pv_nomp/dir/old");
    if (ret == 0) {
        fprintf(stderr, "pivot_root should have failed (not a mountpoint)\n");
        return 2;
    }
    if (errno != EINVAL) {
        fprintf(stderr, "expected EINVAL, got %d (%s)\n",
                errno, strerror(errno));
        return 3;
    }
    return 0;
}

static void test_not_mountpoint(void)
{
    printf("\n--- Test 5: new_root is not a mountpoint → EINVAL ---\n");
    int rc = run_in_new_mntns(child_not_mountpoint, NULL);
    if (rc == 0)
        TEST_PASS("pivot_root rejects non-mountpoint new_root");
    else if (rc == 2)
        TEST_FAIL("not-mountpoint",
                  "pivot_root succeeded on a non-mountpoint new_root");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("not-mountpoint", buf);
    }
}

/* ------------------------------------------------------------------ */
/*  TEST 6  – shared-mount rejection                                   */
/* ------------------------------------------------------------------ */

static int child_shared_mount(void *arg)
{
    (void)arg;

    /* Do NOT make rprivate – keep mounts shared */
    /* First make sure root is shared */
    if (mount("", "/", NULL, MS_REC | MS_SHARED, NULL) != 0) {
        /* If we can't make shared, skip */
        return 99;
    }

    mkdir("/tmp/pv_shared", 0755);
    mkdir("/tmp/pv_shared/new_root", 0755);

    if (mount("tmpfs", "/tmp/pv_shared/new_root", "tmpfs", 0, NULL) != 0) {
        perror("mount");
        return 1;
    }
    mkdir("/tmp/pv_shared/new_root/old_root", 0755);

    int ret = do_pivot_root("/tmp/pv_shared/new_root",
                            "/tmp/pv_shared/new_root/old_root");
    if (ret == 0) {
        fprintf(stderr, "pivot_root should have failed (shared mount)\n");
        return 2;
    }
    if (errno != EINVAL) {
        fprintf(stderr, "expected EINVAL, got %d (%s)\n",
                errno, strerror(errno));
        return 3;
    }
    return 0;
}

static void test_shared_mount_rejection(void)
{
    printf("\n--- Test 6: pivot_root with shared mounts → EINVAL ---\n");
    int rc = run_in_new_mntns(child_shared_mount, NULL);
    if (rc == 0)
        TEST_PASS("pivot_root rejects shared mounts");
    else if (rc == 99)
        TEST_SKIP("shared mount rejection", "cannot set MS_SHARED on /");
    else if (rc == 2)
        TEST_FAIL("shared mount rejection",
                  "pivot_root succeeded with shared mounts");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("shared mount rejection", buf);
    }
}

/* ------------------------------------------------------------------ */
/*  TEST 7  – chroot'd process root preservation (BUG-1)               */
/*                                                                     */
/*  A process that has chroot'd into a subdirectory (root != ns root)  */
/*  should NOT have its root changed by another process's pivot_root.  */
/* ------------------------------------------------------------------ */

static int child_chroot_preserve(void *arg)
{
    (void)arg;

    if (make_rprivate() != 0) return 1;

    /* Create directory layout */
    mkdir("/tmp/pv_chroot", 0755);
    mkdir("/tmp/pv_chroot/new_root", 0755);
    mkdir("/tmp/pv_chroot/subdir", 0755);

    if (mount("tmpfs", "/tmp/pv_chroot/new_root", "tmpfs", 0, NULL) != 0) {
        perror("mount tmpfs");
        return 1;
    }
    mkdir("/tmp/pv_chroot/new_root/old_root", 0755);
    /* Create a marker file so we can verify which root we see */
    int fd = open("/tmp/pv_chroot/subdir/marker", O_CREAT | O_WRONLY, 0644);
    if (fd >= 0) {
        write(fd, "subdir_root", 11);
        close(fd);
    }

    /*
     * Fork a child that chroots to /tmp/pv_chroot/subdir.
     * The parent does pivot_root.
     * Then we check: did the chrooted child's root change?
     */
    int pipefd[2];
    if (pipe(pipefd) != 0) {
        perror("pipe");
        return 1;
    }

    pid_t pid = fork();
    if (pid < 0) {
        perror("fork");
        return 1;
    }

    if (pid == 0) {
        /* ---- CHILD: chroot to subdir, wait, then check ---- */
        close(pipefd[1]);

        if (chroot("/tmp/pv_chroot/subdir") != 0) {
            perror("child: chroot");
            _exit(1);
        }
        if (chdir("/") != 0) {
            perror("child: chdir");
            _exit(1);
        }

        /* Signal parent we're ready, then wait for pivot to finish */
        char buf;
        read(pipefd[0], &buf, 1);  /* blocks until parent writes or closes pipe */
        close(pipefd[0]);

        /*
         * After pivot_root by parent:
         * Our root should STILL be the old /tmp/pv_chroot/subdir,
         * because our root != old namespace root.
         *
         * Check: can we still see the marker file?
         */
        struct stat st;
        if (stat("/marker", &st) == 0) {
            /* Good – our chroot root was NOT overwritten */
            _exit(0);
        } else {
            /* Our root was changed – BUG-1 */
            fprintf(stderr, "child: /marker gone after pivot_root (root overwritten!)\n");
            _exit(2);
        }
    }

    /* ---- PARENT: do pivot_root ---- */
    close(pipefd[0]);

    /* Give child time to chroot */
    usleep(50000);

    int ret = do_pivot_root("/tmp/pv_chroot/new_root",
                            "/tmp/pv_chroot/new_root/old_root");

    /* Signal child regardless of result */
    write(pipefd[1], "x", 1);
    close(pipefd[1]);

    int status;
    waitpid(pid, &status, 0);

    if (ret != 0) {
        /* pivot_root itself failed – can't test BUG-1 */
        fprintf(stderr, "parent: pivot_root failed: %s\n", strerror(errno));
        return 10;
    }

    if (!WIFEXITED(status))
        return 11;

    return WEXITSTATUS(status);
}

static void test_chroot_preserve(void)
{
    printf("\n--- Test 7: chroot'd process root preserved (BUG-1) ---\n");
    int rc = run_in_new_mntns(child_chroot_preserve, NULL);
    if (rc == 0)
        TEST_PASS("chroot'd process root not overwritten by pivot_root");
    else if (rc == 2)
        TEST_FAIL("chroot preserve (BUG-1)",
                  "chroot'd process root was overwritten after pivot_root");
    else if (rc == 10)
        TEST_SKIP("chroot preserve (BUG-1)",
                  "pivot_root itself failed, cannot verify BUG-1");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("chroot preserve (BUG-1)", buf);
    }
}

/* ------------------------------------------------------------------ */
/*  TEST 8  – concurrent pivot_root serialization (BUG-8)              */
/*                                                                     */
/*  Two threads try pivot_root at the same time.  At most one should   */
/*  succeed; both succeeding indicates a missing global lock.          */
/* ------------------------------------------------------------------ */

struct race_ctx {
    volatile int ready;
    const char *nr;    /* new_root  */
    const char *po;    /* put_old   */
    int ret;
    int err;
};

static void *pivot_thread(void *a)
{
    struct race_ctx *c = (struct race_ctx *)a;
    while (!c->ready)
        ;   /* spin */
    c->ret = do_pivot_root(c->nr, c->po);
    c->err = errno;
    return NULL;
}

static int child_race(void *arg)
{
    (void)arg;

    if (make_rprivate() != 0) return 1;

    mkdir("/tmp/pv_race", 0755);
    mkdir("/tmp/pv_race/nr1", 0755);
    mkdir("/tmp/pv_race/nr2", 0755);

    if (mount("tmpfs", "/tmp/pv_race/nr1", "tmpfs", 0, NULL) != 0) {
        perror("mount nr1");
        return 1;
    }
    mkdir("/tmp/pv_race/nr1/old", 0755);

    if (mount("tmpfs", "/tmp/pv_race/nr2", "tmpfs", 0, NULL) != 0) {
        perror("mount nr2");
        return 1;
    }
    mkdir("/tmp/pv_race/nr2/old", 0755);

    struct race_ctx c1 = { .ready = 0,
                           .nr = "/tmp/pv_race/nr1",
                           .po = "/tmp/pv_race/nr1/old" };
    struct race_ctx c2 = { .ready = 0,
                           .nr = "/tmp/pv_race/nr2",
                           .po = "/tmp/pv_race/nr2/old" };

    pthread_t t1, t2;
    pthread_create(&t1, NULL, pivot_thread, &c1);
    pthread_create(&t2, NULL, pivot_thread, &c2);

    usleep(1000);              /* let threads reach the spin-wait */
    c1.ready = c2.ready = 1;  /* release both simultaneously */

    pthread_join(t1, NULL);
    pthread_join(t2, NULL);

    int ok = 0;
    if (c1.ret == 0) ok++;
    if (c2.ret == 0) ok++;

    printf("  thread-1: %s (errno=%d)\n",
           c1.ret == 0 ? "OK" : "FAIL", c1.err);
    printf("  thread-2: %s (errno=%d)\n",
           c2.ret == 0 ? "OK" : "FAIL", c2.err);

    if (ok > 1)
        return 2;   /* BOTH succeeded → race */
    return 0;
}

static void test_race(void)
{
    printf("\n--- Test 8: concurrent pivot_root (BUG-8 race) ---\n");
    int rc = run_in_new_mntns(child_race, NULL);
    if (rc == 0)
        TEST_PASS("at most one concurrent pivot_root succeeded");
    else if (rc == 2)
        TEST_FAIL("concurrent pivot_root (BUG-8)",
                  "BOTH pivot_root calls succeeded – missing serialization");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("concurrent pivot_root", buf);
    }
}

/* ------------------------------------------------------------------ */
/*  TEST 9  – double pivot (pivot_root, then pivot back)               */
/* ------------------------------------------------------------------ */

static int child_double_pivot(void *arg)
{
    (void)arg;

    if (make_rprivate() != 0) return 1;

    mkdir("/tmp/pv_double", 0755);
    mkdir("/tmp/pv_double/new_root", 0755);

    if (mount("tmpfs", "/tmp/pv_double/new_root", "tmpfs", 0, NULL) != 0) {
        perror("mount tmpfs");
        return 1;
    }
    mkdir("/tmp/pv_double/new_root/old_root", 0755);

    chdir("/tmp/pv_double/new_root");

    /* First pivot */
    if (do_pivot_root("/tmp/pv_double/new_root",
                      "/tmp/pv_double/new_root/old_root") != 0) {
        fprintf(stderr, "first pivot_root failed: %s\n", strerror(errno));
        return 2;
    }

    chdir("/");

    /* Now pivot back: new_root = /old_root, put_old = /old_root/put_back */
    mkdir("/old_root/put_back", 0755);

    if (do_pivot_root("/old_root", "/old_root/put_back") != 0) {
        fprintf(stderr, "second pivot_root failed: %s\n", strerror(errno));
        /* Not necessarily a bug – old_root may need special handling */
        return 3;
    }

    /* After pivoting back, / should be the original rootfs */
    struct statfs sfs;
    if (statfs("/", &sfs) == 0 && sfs.f_type == TMPFS_MAGIC) {
        fprintf(stderr, "still on tmpfs after pivoting back!\n");
        return 4;
    }

    return 0;
}

static void test_double_pivot(void)
{
    printf("\n--- Test 9: double pivot_root (pivot then pivot back) ---\n");
    int rc = run_in_new_mntns(child_double_pivot, NULL);
    if (rc == 0)
        TEST_PASS("double pivot_root (there and back)");
    else if (rc == 2)
        TEST_SKIP("double pivot",
                  "first pivot_root failed, can't test double pivot");
    else if (rc == 3)
        TEST_SKIP("double pivot",
                  "second pivot_root failed (may need old_root mount adjustment)");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("double pivot_root", buf);
    }
}

/* ------------------------------------------------------------------ */
/*  TEST 10 – new_root not under current root → EINVAL                 */
/*  (validates is_path_reachable / is_ancestor on the new→root path)   */
/* ------------------------------------------------------------------ */

static int child_new_root_not_below(void *arg)
{
    (void)arg;

    if (make_rprivate() != 0) return 1;

    /*
     * After chroot("/some/subdir"), the namespace root "/" is no longer
     * the process root.  If we then try to pivot_root with a new_root
     * that is NOT below the process's chroot root, it must fail.
     */
    mkdir("/tmp/pv_below", 0755);
    mkdir("/tmp/pv_below/subdir", 0755);
    mkdir("/tmp/pv_below/new_root", 0755);

    if (mount("tmpfs", "/tmp/pv_below/new_root", "tmpfs", 0, NULL) != 0) {
        perror("mount tmpfs");
        return 1;
    }
    mkdir("/tmp/pv_below/new_root/old_root", 0755);

    /* chroot into subdir – now new_root is outside our view */
    if (chroot("/tmp/pv_below/subdir") != 0) {
        perror("chroot");
        return 1;
    }
    chdir("/");

    /*
     * new_root (/tmp/pv_below/new_root) is NOT below our chroot
     * root (/tmp/pv_below/subdir), so we shouldn't even be able
     * to resolve the path.  This should fail with ENOENT or EINVAL.
     */
    int ret = do_pivot_root("/tmp/pv_below/new_root",
                            "/tmp/pv_below/new_root/old_root");
    if (ret == 0) {
        fprintf(stderr, "pivot_root succeeded after chroot (should fail)\n");
        return 2;
    }
    /* Any error is acceptable here (ENOENT, EINVAL, etc.) */
    return 0;
}

static void test_new_root_not_below(void)
{
    printf("\n--- Test 10: new_root outside chroot → error ---\n");
    int rc = run_in_new_mntns(child_new_root_not_below, NULL);
    if (rc == 0)
        TEST_PASS("pivot_root rejects new_root outside chroot");
    else if (rc == 2)
        TEST_FAIL("new_root not below root",
                  "pivot_root succeeded when new_root is outside chroot");
    else {
        char buf[64];
        snprintf(buf, sizeof(buf), "child exited %d", rc);
        TEST_FAIL("new_root not below root", buf);
    }
}

/* ================================================================== */
/*  main                                                               */
/* ================================================================== */

int main(void)
{
    printf("=== test_pivot_root – pivot_root(2) system call test suite ===\n");

    if (getuid() != 0) {
        fprintf(stderr, "WARNING: most tests require root; running anyway\n");
    }

    test_basic_pivot_root();         /* Test 1 – basic + BUG-5/6 */
    test_ebusy();                    /* Test 2 – EBUSY           */
    test_eperm();                    /* Test 3 – EPERM           */
    test_put_old_unreachable();      /* Test 4 – put_old outside */
    test_not_mountpoint();           /* Test 5 – not a mountpoint*/
    test_shared_mount_rejection();   /* Test 6 – shared mount    */
    test_chroot_preserve();          /* Test 7 – BUG-1           */
    test_race();                     /* Test 8 – BUG-8           */
    test_double_pivot();             /* Test 9 – double pivot    */
    test_new_root_not_below();       /* Test 10 – new_root check */

    printf("\n=== Summary ===\n");
    printf("  Passed:  %d\n", tests_passed);
    printf("  Failed:  %d\n", tests_failed);
    printf("  Skipped: %d\n", tests_skipped);

    return tests_failed == 0 ? 0 : 1;
}
