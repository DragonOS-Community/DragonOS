/**
 * Seccomp test suite for DragonOS
 *
 * Tests:
 *   1. PR_GET_SECCOMP returns 0 when disabled
 *   2. Strict mode via prctl (child killed on disallowed syscall)
 *   3. Strict mode via seccomp() syscall
 *   4. BPF filter: allow-all
 *   5. BPF filter: ERRNO on write
 *   6. BPF filter: KILL on getpid
 *   7. Fork inherits filter
 *   8. /proc/self/status Seccomp field
 */

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/prctl.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

/* seccomp constants — may not be in musl headers yet */
#ifndef PR_SET_SECCOMP
#define PR_SET_SECCOMP 22
#endif
#ifndef PR_GET_SECCOMP
#define PR_GET_SECCOMP 21
#endif

#define SECCOMP_SET_MODE_STRICT 0
#define SECCOMP_SET_MODE_FILTER 1

#define SECCOMP_RET_KILL_THREAD  0x00000000U
#define SECCOMP_RET_TRAP         0x00030000U
#define SECCOMP_RET_ERRNO        0x00050000U
#define SECCOMP_RET_LOG          0x7ffc0000U
#define SECCOMP_RET_ALLOW        0x7fff0000U

/* BPF instruction encoding */
#define BPF_LD   0x00
#define BPF_W    0x00
#define BPF_ABS  0x20
#define BPF_JMP  0x05
#define BPF_JEQ  0x10
#define BPF_K    0x00
#define BPF_RET  0x06

struct sock_filter {
    unsigned short code;
    unsigned char jt;
    unsigned char jf;
    unsigned int k;
};

struct sock_fprog {
    unsigned short len;
    struct sock_filter *filter;
};

static int seccomp(unsigned int op, unsigned int flags, void *args)
{
    return syscall(__NR_seccomp, op, flags, args);
}

static int install_filter(struct sock_filter *fprog, unsigned short len)
{
    struct sock_fprog prog = {
        .len = len,
        .filter = fprog,
    };
    return seccomp(SECCOMP_SET_MODE_FILTER, 0, &prog);
}

/* ── helpers ── */

static int g_pass = 0;
static int g_fail = 0;
static int g_skip = 0;

#define TEST(name) printf("  TEST %-45s", name)
#define PASS() do { printf("  PASS\n"); g_pass++; } while(0)
#define FAIL(msg) do { printf("  FAIL: %s\n", msg); g_fail++; } while(0)
#define SKIP(msg) do { printf("  SKIP: %s\n", msg); g_skip++; } while(0)

/* ── test 1: PR_GET_SECCOMP disabled ── */

static void test_get_seccomp_disabled(void)
{
    TEST("PR_GET_SECCOMP returns 0 when disabled");
    int r = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
    if (r == 0)
        PASS();
    else
        FAIL("expected 0");
}

/* ── test 2: strict mode via prctl ── */

static void test_strict_mode_prctl(void)
{
    TEST("strict mode: child killed on forbidden syscall");
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        /* child: enable strict, then try getpid (forbidden) */
        prctl(PR_SET_SECCOMP, 1, 0, 0, 0);
        /* If seccomp didn't work, getpid succeeds and we exit 0.
         * If seccomp works, SIGSYS kills us. */
        getpid();
        _exit(0);
    }
    /* parent: wait for child */
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFSIGNALED(status) && WTERMSIG(status) == SIGSYS)
        PASS();
    else if (WIFEXITED(status) && WEXITSTATUS(status) == 0)
        FAIL("child was NOT killed — seccomp not enforced");
    else
        FAIL("unexpected child exit");
}

/* ── test 3: strict mode via seccomp() syscall ── */

static void test_strict_mode_seccomp_syscall(void)
{
    TEST("seccomp(SET_MODE_STRICT) kills on forbidden syscall");
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        seccomp(SECCOMP_SET_MODE_STRICT, 0, NULL);
        /* getpid is forbidden in strict mode */
        getpid();
        _exit(0);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFSIGNALED(status) && WTERMSIG(status) == SIGSYS)
        PASS();
    else if (WIFEXITED(status) && WEXITSTATUS(status) == 0)
        FAIL("child was NOT killed");
    else
        FAIL("unexpected child exit");
}

/* ── test 4: strict mode allows read/write/exit ── */

static void test_strict_mode_allows_whitelist(void)
{
    TEST("strict mode: read/write/exit still work");
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        prctl(PR_SET_SECCOMP, 1, 0, 0, 0);
        /* write is in the whitelist */
        write(STDOUT_FILENO, "", 0);
        /* exit with 42 to signal success */
        _exit(42);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 42)
        PASS();
    else
        FAIL("child did not exit normally");
}

/* ── test 5: BPF filter allow-all ── */

static void test_filter_allow_all(void)
{
    TEST("BPF filter: allow-all permits syscalls");
    struct sock_filter filter[] = {
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW },
    };
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if (install_filter(filter, sizeof(filter) / sizeof(filter[0])) < 0) {
            perror("install_filter");
            _exit(1);
        }
        /* These should all succeed */
        getpid();
        write(STDOUT_FILENO, "", 0);
        _exit(42);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 42)
        PASS();
    else
        FAIL("child did not exit normally");
}

/* ── test 6: BPF filter ERRNO on write ── */

static void test_filter_errno_write(void)
{
    TEST("BPF filter: ERRNO(EPERM) on write");
    /* BPF program:
     *   ld [0]                    // A = seccomp_data.nr
     *   jeq __NR_write, 0, 1     // if nr==write: +0 else: +1
     *   ret ERRNO(EPERM)         // ERRNO for write
     *   ret ALLOW                // allow everything else
     */
    struct sock_filter filter[] = {
        { BPF_LD | BPF_W | BPF_ABS, 0, 0, 0 },
        { BPF_JMP | BPF_JEQ | BPF_K, 0, 1, __NR_write },
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | 1 },
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW },
    };
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if (install_filter(filter, sizeof(filter) / sizeof(filter[0])) < 0) {
            perror("install_filter");
            _exit(1);
        }
        /* write should return -1 with errno=EPERM */
        ssize_t r = write(STDOUT_FILENO, "x", 1);
        _exit(r < 0 && errno == EPERM ? 42 : 1);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 42)
        PASS();
    else
        FAIL("write did not fail with EPERM");
}

/* ── test 7: BPF filter KILL on getpid ── */

static void test_filter_kill_getpid(void)
{
    TEST("BPF filter: KILL on getpid");
    struct sock_filter filter[] = {
        { BPF_LD | BPF_W | BPF_ABS, 0, 0, 0 },
        { BPF_JMP | BPF_JEQ | BPF_K, 0, 1, __NR_getpid },
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_KILL_THREAD },
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW },
    };
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if (install_filter(filter, sizeof(filter) / sizeof(filter[0])) < 0) {
            perror("install_filter");
            _exit(1);
        }
        /* write should succeed (not blocked) */
        write(STDOUT_FILENO, "", 0);
        /* getpid should kill us */
        syscall(SYS_getpid);
        _exit(0);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFSIGNALED(status) && WTERMSIG(status) == SIGSYS)
        PASS();
    else if (WIFEXITED(status) && WEXITSTATUS(status) == 0)
        FAIL("child was NOT killed by getpid");
    else
        FAIL("unexpected child exit");
}

/* ── test 8: fork inherits filter ── */

static void test_filter_fork_inherit(void)
{
    TEST("fork child inherits seccomp filter");
    /* Parent installs filter that blocks getpid with ERRNO(ENOENT),
     * then forks. Child tries getpid — should fail with ENOENT. */
    struct sock_filter filter[] = {
        { BPF_LD | BPF_W | BPF_ABS, 0, 0, 0 },
        { BPF_JMP | BPF_JEQ | BPF_K, 0, 1, __NR_getpid },
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | ENOENT },
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW },
    };

    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        /* child: install filter, then fork again */
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if (install_filter(filter, sizeof(filter) / sizeof(filter[0])) < 0) {
            perror("install_filter");
            _exit(1);
        }

        pid_t inner = fork();
        if (inner < 0)
            _exit(2);
        if (inner == 0) {
            errno = 0;
            long mypid = syscall(SYS_getpid);
            _exit(mypid == -1 && errno == ENOENT ? 42 : 3);
        }
        int st = 0;
        waitpid(inner, &st, 0);
        /* propagate grandchild result */
        if (WIFEXITED(st) && WEXITSTATUS(st) == 42)
            _exit(42);
        else
            _exit(4);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 42)
        PASS();
    else
        FAIL("grandchild did not inherit filter");
}

/* ── test 9: PR_GET_SECCOMP returns 2 in filter mode ── */

static void test_get_seccomp_filter(void)
{
    TEST("PR_GET_SECCOMP returns 2 in filter mode");
    struct sock_filter filter[] = {
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW },
    };
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        install_filter(filter, sizeof(filter) / sizeof(filter[0]));
        int mode = prctl(PR_GET_SECCOMP, 0, 0, 0, 0);
        _exit(mode == 2 ? 42 : mode);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 42)
        PASS();
    else
        FAIL("expected mode 2");
}

/* ── test 10: /proc/self/status Seccomp field ── */

static void test_proc_status_seccomp(void)
{
    TEST("/proc/self/status shows Seccomp: 2 after filter");
    struct sock_filter filter[] = {
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW },
    };
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        install_filter(filter, sizeof(filter) / sizeof(filter[0]));

        FILE *f = fopen("/proc/self/status", "r");
        if (!f) {
            perror("fopen");
            _exit(1);
        }
        char line[256];
        int found = 0;
        while (fgets(line, sizeof(line), f)) {
            if (strncmp(line, "Seccomp:", 8) == 0) {
                int val = 0;
                sscanf(line + 8, "%d", &val);
                if (val == 2)
                    found = 1;
                break;
            }
        }
        fclose(f);
        _exit(found ? 42 : 1);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 42)
        PASS();
    else
        FAIL("Seccomp: 2 not found in /proc/self/status");
}

/* ── test 11: strict mode is irreversible ── */

static void test_strict_irreversible(void)
{
    TEST("strict mode: prctl killed (indirectly proves irreversible)");
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        prctl(PR_SET_SECCOMP, 1, 0, 0, 0);
        struct sock_filter filter[] = {
            { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW },
        };
        prctl(PR_SET_SECCOMP, 2, &filter, 0, 0);
        _exit(0);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFSIGNALED(status) && WTERMSIG(status) == SIGSYS)
        PASS();
    else
        FAIL("child was NOT killed (strict mode may not be enforced)");
}

/* ── test 12: ERRNO returns correct errno value ── */

static void test_filter_errno_value(void)
{
    TEST("BPF filter: ERRNO returns correct errno (ENOENT=2)");
    struct sock_filter filter[] = {
        { BPF_LD | BPF_W | BPF_ABS, 0, 0, 0 },
        { BPF_JMP | BPF_JEQ | BPF_K, 0, 1, __NR_getpid },
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ERRNO | ENOENT },
        { BPF_RET | BPF_K, 0, 0, SECCOMP_RET_ALLOW },
    };
    pid_t pid = fork();
    if (pid < 0) {
        FAIL("fork failed");
        return;
    }
    if (pid == 0) {
        prctl(PR_SET_NO_NEW_PRIVS, 1, 0, 0, 0);
        if (install_filter(filter, sizeof(filter) / sizeof(filter[0])) < 0) {
            perror("install_filter");
            _exit(1);
        }
        errno = 0;
        long r = syscall(SYS_getpid);
        _exit(r == -1 && errno == ENOENT ? 42 : 1);
    }
    int status = 0;
    waitpid(pid, &status, 0);
    if (WIFEXITED(status) && WEXITSTATUS(status) == 42)
        PASS();
    else
        FAIL("getpid did not return -ENOENT");
}

/* ── main ── */

int main(void)
{
    printf("=== Seccomp Test Suite ===\n\n");

    test_get_seccomp_disabled();
    test_strict_mode_prctl();
    test_strict_mode_seccomp_syscall();
    test_strict_mode_allows_whitelist();
    test_strict_irreversible();
    test_filter_allow_all();
    test_filter_errno_write();
    test_filter_kill_getpid();
    test_filter_fork_inherit();
    test_get_seccomp_filter();
    test_proc_status_seccomp();
    test_filter_errno_value();

    printf("\n=== Results: %d passed, %d failed, %d skipped ===\n",
           g_pass, g_fail, g_skip);

    return g_fail > 0 ? 1 : 0;
}
