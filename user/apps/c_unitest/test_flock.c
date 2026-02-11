#define _GNU_SOURCE

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/file.h>
#include <sys/socket.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

static int g_total = 0;
static int g_failed = 0;

#define CHECK(cond, msg)                                                       \
    do {                                                                       \
        g_total++;                                                             \
        if (!(cond)) {                                                         \
            g_failed++;                                                        \
            fprintf(stderr, "FAIL: %s (line %d)\n", msg, __LINE__);            \
        } else {                                                               \
            printf("PASS: %s\n", msg);                                         \
        }                                                                      \
    } while (0)

static int is_wouldblock_errno(int err) { return err == EAGAIN || err == EWOULDBLOCK; }

static int open_rw_file(const char *path) {
    return open(path, O_RDWR | O_CREAT, 0644);
}

static void test_invalid_commands(const char *path) {
    int fd = open_rw_file(path);
    CHECK(fd >= 0, "open file for invalid command test");
    if (fd < 0) {
        return;
    }

    errno = 0;
    CHECK(flock(fd, LOCK_EX | LOCK_SH | LOCK_NB) == -1 && errno == EINVAL,
          "LOCK_EX|LOCK_SH|LOCK_NB returns EINVAL");

    errno = 0;
    CHECK(flock(fd, LOCK_EX | LOCK_UN | LOCK_NB) == -1 && errno == EINVAL,
          "LOCK_EX|LOCK_UN|LOCK_NB returns EINVAL");

    errno = 0;
    CHECK(flock(fd, LOCK_NB) == -1 && errno == EINVAL,
          "LOCK_NB without operation returns EINVAL");

    close(fd);
}

static void test_basic_lock_unlock(const char *path) {
    int fd = open_rw_file(path);
    CHECK(fd >= 0, "open file for basic flock");
    if (fd < 0) {
        return;
    }

    CHECK(flock(fd, LOCK_EX | LOCK_NB) == 0, "LOCK_EX|LOCK_NB succeeds");
    CHECK(flock(fd, LOCK_UN) == 0, "LOCK_UN after exclusive succeeds");
    CHECK(flock(fd, LOCK_SH | LOCK_NB) == 0, "LOCK_SH|LOCK_NB succeeds");
    CHECK(flock(fd, LOCK_UN) == 0, "LOCK_UN after shared succeeds");

    close(fd);
}

static void test_nonblocking_conflict(const char *path) {
    int fd1 = open_rw_file(path);
    int fd2 = open_rw_file(path);
    CHECK(fd1 >= 0 && fd2 >= 0, "open two independent fds");
    if (fd1 < 0 || fd2 < 0) {
        if (fd1 >= 0)
            close(fd1);
        if (fd2 >= 0)
            close(fd2);
        return;
    }

    CHECK(flock(fd1, LOCK_EX | LOCK_NB) == 0, "fd1 takes exclusive lock");
    errno = 0;
    CHECK(flock(fd2, LOCK_EX | LOCK_NB) == -1 && is_wouldblock_errno(errno),
          "fd2 nonblocking exclusive lock conflicts");
    CHECK(flock(fd1, LOCK_UN) == 0, "fd1 unlock succeeds");
    CHECK(flock(fd2, LOCK_EX | LOCK_NB) == 0, "fd2 lock succeeds after fd1 unlock");
    CHECK(flock(fd2, LOCK_UN) == 0, "fd2 unlock succeeds");

    close(fd2);
    close(fd1);
}

static void test_dup_unlock_release(const char *path) {
    int fd = open_rw_file(path);
    int dupfd = dup(fd);
    int other = open_rw_file(path);
    CHECK(fd >= 0 && dupfd >= 0 && other >= 0, "open/dup for dup unlock test");
    if (fd < 0 || dupfd < 0 || other < 0) {
        if (fd >= 0)
            close(fd);
        if (dupfd >= 0)
            close(dupfd);
        if (other >= 0)
            close(other);
        return;
    }

    CHECK(flock(fd, LOCK_EX | LOCK_NB) == 0, "original fd takes exclusive lock");
    errno = 0;
    CHECK(flock(other, LOCK_EX | LOCK_NB) == -1 && is_wouldblock_errno(errno),
          "unrelated fd is blocked by dup-shared lock");
    CHECK(flock(dupfd, LOCK_UN) == 0, "LOCK_UN via dup fd releases lock");
    CHECK(flock(other, LOCK_EX | LOCK_NB) == 0, "unrelated fd can lock after dup unlock");
    CHECK(flock(other, LOCK_UN) == 0, "unrelated fd unlock succeeds");

    close(other);
    close(dupfd);
    close(fd);
}

static void test_dup_last_close_release(const char *path) {
    int fd = open_rw_file(path);
    int dupfd = dup(fd);
    int other = open_rw_file(path);
    CHECK(fd >= 0 && dupfd >= 0 && other >= 0, "open/dup for last-close release test");
    if (fd < 0 || dupfd < 0 || other < 0) {
        if (fd >= 0)
            close(fd);
        if (dupfd >= 0)
            close(dupfd);
        if (other >= 0)
            close(other);
        return;
    }

    CHECK(flock(fd, LOCK_EX | LOCK_NB) == 0, "original fd takes exclusive lock");

    close(dupfd);
    errno = 0;
    CHECK(flock(other, LOCK_EX | LOCK_NB) == -1 && is_wouldblock_errno(errno),
          "closing one dup fd does not release lock");

    close(fd);
    CHECK(flock(other, LOCK_EX | LOCK_NB) == 0,
          "last close of open-file-description releases lock");
    CHECK(flock(other, LOCK_UN) == 0, "unlock after last-close release succeeds");

    close(other);
}

static void test_fork_unlock_release(const char *path) {
    int fd = open_rw_file(path);
    int other = open_rw_file(path);
    CHECK(fd >= 0 && other >= 0, "open fds for fork flock test");
    if (fd < 0 || other < 0) {
        if (fd >= 0)
            close(fd);
        if (other >= 0)
            close(other);
        return;
    }

    CHECK(flock(fd, LOCK_EX | LOCK_NB) == 0, "parent acquires exclusive lock");

    pid_t pid = fork();
    CHECK(pid >= 0, "fork for flock test");
    if (pid < 0) {
        close(other);
        close(fd);
        return;
    }

    if (pid == 0) {
        int rc = 0;

        errno = 0;
        if (!(flock(other, LOCK_EX | LOCK_NB) == -1 && is_wouldblock_errno(errno))) {
            rc = 1;
        }

        if (rc == 0 && flock(fd, LOCK_UN) != 0) {
            rc = 1;
        }

        if (rc == 0 && flock(other, LOCK_EX | LOCK_NB) != 0) {
            rc = 1;
        }
        if (rc == 0 && flock(other, LOCK_UN) != 0) {
            rc = 1;
        }

        close(other);
        close(fd);
        _exit(rc);
    }

    int status = 0;
    CHECK(waitpid(pid, &status, 0) == pid, "wait child for flock fork test");
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "child unlock on inherited fd releases shared lock");

    close(other);
    close(fd);
}

static volatile sig_atomic_t g_sigalrm_seen = 0;

static void sigalrm_handler(int sig) {
    (void)sig;
    g_sigalrm_seen = 1;
}

static void test_blocking_interrupted_by_signal(const char *path) {
    int fd1 = open_rw_file(path);
    int fd2 = open_rw_file(path);
    CHECK(fd1 >= 0 && fd2 >= 0, "open two independent fds for EINTR test");
    if (fd1 < 0 || fd2 < 0) {
        if (fd1 >= 0)
            close(fd1);
        if (fd2 >= 0)
            close(fd2);
        return;
    }

    CHECK(flock(fd1, LOCK_EX | LOCK_NB) == 0, "fd1 takes lock before blocking flock");

    struct sigaction old_act;
    struct sigaction act;
    memset(&act, 0, sizeof(act));
    act.sa_handler = sigalrm_handler;
    sigemptyset(&act.sa_mask);
    act.sa_flags = 0;
    CHECK(sigaction(SIGALRM, &act, &old_act) == 0, "install SIGALRM handler");

    g_sigalrm_seen = 0;
    alarm(1);
    errno = 0;
    CHECK(flock(fd2, LOCK_EX) == -1 && errno == EINTR,
          "blocking flock interrupted by signal returns EINTR");
    alarm(0);
    CHECK(g_sigalrm_seen != 0, "SIGALRM handler executed");

    CHECK(sigaction(SIGALRM, &old_act, NULL) == 0, "restore SIGALRM handler");
    CHECK(flock(fd1, LOCK_UN) == 0, "fd1 unlock after EINTR test");

    close(fd2);
    close(fd1);
}

static void test_opath_ebadf(const char *path) {
#ifdef O_PATH
    int fd = open(path, O_RDONLY | O_PATH, 0);
    CHECK(fd >= 0, "open O_PATH file");
    if (fd < 0) {
        return;
    }

    errno = 0;
    CHECK(flock(fd, LOCK_EX | LOCK_NB) == -1 && errno == EBADF,
          "flock on O_PATH fd returns EBADF");
    close(fd);
#else
    (void)path;
    printf("SKIP: O_PATH is unavailable in headers\n");
#endif
}

static void test_pipe_flock(void) {
    int p[2];
    int rc = pipe(p);
    CHECK(rc == 0, "create pipe for flock test");
    if (rc != 0) {
        return;
    }

    CHECK(flock(p[0], LOCK_EX | LOCK_NB) == 0, "pipe read end lock succeeds");
    errno = 0;
    CHECK(flock(p[1], LOCK_EX | LOCK_NB) == -1 && is_wouldblock_errno(errno),
          "pipe write end lock conflicts");
    CHECK(flock(p[0], LOCK_UN) == 0, "pipe read end unlock succeeds");
    CHECK(flock(p[1], LOCK_EX | LOCK_NB) == 0, "pipe write end lock succeeds after unlock");
    CHECK(flock(p[1], LOCK_UN) == 0, "pipe write end unlock succeeds");

    close(p[0]);
    close(p[1]);
}

static void test_socket_flock(void) {
    int sock = socket(AF_UNIX, SOCK_STREAM, 0);
    CHECK(sock >= 0, "create UNIX socket for flock test");
    if (sock < 0) {
        return;
    }

    CHECK(flock(sock, LOCK_EX | LOCK_NB) == 0, "flock on socket succeeds");
    CHECK(flock(sock, LOCK_UN) == 0, "unlock socket flock succeeds");

    close(sock);
}

static void test_blocking_downgrade_wakeup(const char *path) {
    int fd = open_rw_file(path);
    CHECK(fd >= 0, "open file for downgrade wakeup test");
    if (fd < 0)
        return;

    CHECK(flock(fd, LOCK_EX | LOCK_NB) == 0, "parent acquires LOCK_EX");

    int pipefd[2];
    CHECK(pipe(pipefd) == 0, "create pipe for downgrade wakeup test");

    pid_t pid = fork();
    CHECK(pid >= 0, "fork for downgrade wakeup test");
    if (pid < 0) {
        close(fd);
        close(pipefd[0]);
        close(pipefd[1]);
        return;
    }

    if (pid == 0) {
        close(pipefd[0]);
        int child_fd = open_rw_file(path);
        if (child_fd < 0)
            _exit(1);

        /* This should block until the parent downgrades to LOCK_SH */
        if (flock(child_fd, LOCK_SH) != 0)
            _exit(2);

        /* Notify parent that we acquired the lock */
        char ok = 1;
        write(pipefd[1], &ok, 1);

        flock(child_fd, LOCK_UN);
        close(child_fd);
        close(pipefd[1]);
        _exit(0);
    }

    /* Parent: give child time to enter blocking flock */
    close(pipefd[1]);
    usleep(200000);

    /* Downgrade from LOCK_EX to LOCK_SH — should wake the child */
    CHECK(flock(fd, LOCK_SH | LOCK_NB) == 0, "parent downgrades to LOCK_SH");

    /* Wait for child to signal success, with a timeout via alarm */
    struct sigaction old_act;
    struct sigaction act;
    memset(&act, 0, sizeof(act));
    act.sa_handler = sigalrm_handler;
    sigemptyset(&act.sa_mask);
    act.sa_flags = 0;
    sigaction(SIGALRM, &act, &old_act);

    g_sigalrm_seen = 0;
    alarm(5);

    char buf = 0;
    int r = read(pipefd[0], &buf, 1);
    alarm(0);
    sigaction(SIGALRM, &old_act, NULL);

    CHECK(r == 1 && buf == 1, "child acquired LOCK_SH after parent downgrade");

    int status = 0;
    waitpid(pid, &status, 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "child exited successfully in downgrade wakeup test");

    flock(fd, LOCK_UN);
    close(fd);
    close(pipefd[0]);
}

static void test_blocking_upgrade_wakeup(const char *path) {
    int fd1 = open_rw_file(path);
    int fd2 = open_rw_file(path);
    CHECK(fd1 >= 0 && fd2 >= 0, "open fds for upgrade wakeup test");
    if (fd1 < 0 || fd2 < 0) {
        if (fd1 >= 0)
            close(fd1);
        if (fd2 >= 0)
            close(fd2);
        return;
    }

    CHECK(flock(fd1, LOCK_SH | LOCK_NB) == 0, "fd1 acquires LOCK_SH");
    CHECK(flock(fd2, LOCK_SH | LOCK_NB) == 0, "fd2 acquires LOCK_SH");

    int pipefd[2];
    CHECK(pipe(pipefd) == 0, "create pipe for upgrade wakeup test");

    pid_t pid = fork();
    CHECK(pid >= 0, "fork for upgrade wakeup test");
    if (pid < 0) {
        close(fd1);
        close(fd2);
        close(pipefd[0]);
        close(pipefd[1]);
        return;
    }

    if (pid == 0) {
        close(pipefd[0]);
        /* Child inherited both shared locks via fd1 and fd2.
         * Release fd1's shared lock so only fd2 remains from child side. */
        flock(fd1, LOCK_UN);
        close(fd1);

        /* Try to upgrade fd2 to LOCK_EX — should block because parent still holds fd1 LOCK_SH */
        if (flock(fd2, LOCK_EX) != 0)
            _exit(2);

        /* Notify parent that we acquired the lock */
        char ok = 1;
        write(pipefd[1], &ok, 1);

        flock(fd2, LOCK_UN);
        close(fd2);
        close(pipefd[1]);
        _exit(0);
    }

    /* Parent: give child time to enter blocking flock */
    close(pipefd[1]);
    usleep(200000);

    /* Release parent's LOCK_SH on fd1 — should unblock the child's LOCK_EX on fd2 */
    CHECK(flock(fd1, LOCK_UN) == 0, "parent releases LOCK_SH on fd1");

    /* Wait for child to signal success, with a timeout via alarm */
    struct sigaction old_act;
    struct sigaction act;
    memset(&act, 0, sizeof(act));
    act.sa_handler = sigalrm_handler;
    sigemptyset(&act.sa_mask);
    act.sa_flags = 0;
    sigaction(SIGALRM, &act, &old_act);

    g_sigalrm_seen = 0;
    alarm(5);

    char buf = 0;
    int r = read(pipefd[0], &buf, 1);
    alarm(0);
    sigaction(SIGALRM, &old_act, NULL);

    CHECK(r == 1 && buf == 1, "child acquired LOCK_EX after parent released LOCK_SH");

    int status = 0;
    waitpid(pid, &status, 0);
    CHECK(WIFEXITED(status) && WEXITSTATUS(status) == 0,
          "child exited successfully in upgrade wakeup test");

    flock(fd2, LOCK_UN);
    close(fd1);
    close(fd2);
    close(pipefd[0]);
}

int main(void) {
    char path[128];
    snprintf(path, sizeof(path), "/tmp/test_flock_%d.tmp", getpid());

    int initfd = open(path, O_RDWR | O_CREAT | O_TRUNC, 0644);
    CHECK(initfd >= 0, "create flock test file");
    if (initfd >= 0) {
        close(initfd);
    }

    test_invalid_commands(path);
    test_basic_lock_unlock(path);
    test_nonblocking_conflict(path);
    test_dup_unlock_release(path);
    test_dup_last_close_release(path);
    test_fork_unlock_release(path);
    test_blocking_interrupted_by_signal(path);
    test_opath_ebadf(path);
    test_pipe_flock();
    test_socket_flock();
    test_blocking_downgrade_wakeup(path);
    test_blocking_upgrade_wakeup(path);

    unlink(path);

    printf("test_flock summary: total=%d failed=%d\n", g_total, g_failed);
    return g_failed == 0 ? 0 : 1;
}
