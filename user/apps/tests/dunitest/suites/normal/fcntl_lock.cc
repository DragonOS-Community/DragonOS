#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <string.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {

struct TempFile {
    std::string path;
    int fd;
};

TempFile make_temp_file() {
    char tmpl[] = "/tmp/dunitest_fcntl_lock_XXXXXX";
    int fd = mkstemp(tmpl);
    EXPECT_GE(fd, 0) << "mkstemp failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (fd >= 0) {
        char buf[64] = {};
        ssize_t n = write(fd, buf, sizeof(buf));
        EXPECT_EQ(static_cast<ssize_t>(sizeof(buf)), n);
    }
    return TempFile{tmpl, fd};
}

int set_lock_errno(int fd, int cmd, short type, short whence, off_t start, off_t len) {
    struct flock fl = {};
    fl.l_type = type;
    fl.l_whence = whence;
    fl.l_start = start;
    fl.l_len = len;
    if (fcntl(fd, cmd, &fl) == 0) {
        return 0;
    }
    return errno;
}

int get_lock_errno(int fd, short type, short whence, off_t start, off_t len, struct flock* out) {
    struct flock fl = {};
    fl.l_type = type;
    fl.l_whence = whence;
    fl.l_start = start;
    fl.l_len = len;
    if (fcntl(fd, F_GETLK, &fl) == 0) {
        if (out) {
            *out = fl;
        }
        return 0;
    }
    return errno;
}

bool is_lock_conflict_errno(int e) {
    return e == EAGAIN || e == EACCES;
}

bool waitpid_with_timeout(pid_t pid, int* status, int timeout_ms) {
    int waited = 0;
    while (waited <= timeout_ms) {
        pid_t r = waitpid(pid, status, WNOHANG);
        if (r == pid) {
            return true;
        }
        if (r < 0) {
            return false;
        }
        usleep(10 * 1000);
        waited += 10;
    }
    return false;
}

void write_or_die(int fd, const void* buf, size_t len, int exit_code) {
    ssize_t n = write(fd, buf, len);
    if (n != static_cast<ssize_t>(len)) {
        _exit(exit_code);
    }
}

}  // namespace

TEST(FcntlLock, GetlkDoesNotConflictWithSameOwner) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);

    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    struct flock fl = {};
    ASSERT_EQ(0, get_lock_errno(tf.fd, F_RDLCK, SEEK_SET, 0, 16, &fl));
    EXPECT_EQ(F_UNLCK, fl.l_type);

    EXPECT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0));
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, SetlkAndGetlkConflictAcrossProcesses) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(1);
        }

        int err = set_lock_errno(fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16);
        if (!is_lock_conflict_errno(err)) {
            _exit(2);
        }

        struct flock out = {};
        err = get_lock_errno(fd, F_WRLCK, SEEK_SET, 0, 16, &out);
        if (err != 0) {
            _exit(3);
        }
        if (out.l_type != F_WRLCK || out.l_pid != getppid()) {
            _exit(4);
        }

        close(fd);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    EXPECT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0));
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, SetlkwBlocksUntilUnlock) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    int pipefd[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipefd));
    int flags = fcntl(pipefd[0], F_GETFL, 0);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(pipefd[0], F_SETFL, flags | O_NONBLOCK));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(pipefd[0]);
        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(1);
        }

        int err = set_lock_errno(fd, F_SETLKW, F_WRLCK, SEEK_SET, 0, 16);
        if (err != 0) {
            _exit(2);
        }

        char ok = '1';
        write_or_die(pipefd[1], &ok, 1, 3);
        close(fd);
        close(pipefd[1]);
        _exit(0);
    }

    close(pipefd[1]);
    sleep(1);

    char c = 0;
    ssize_t n = read(pipefd[0], &c, 1);
    EXPECT_EQ(-1, n);
    EXPECT_EQ(EAGAIN, errno);

    EXPECT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0));

    for (;;) {
        n = read(pipefd[0], &c, 1);
        if (n == 1) {
            break;
        }
        if (n == -1 && errno == EAGAIN) {
            usleep(10 * 1000);
            continue;
        }
        FAIL() << "unexpected read result n=" << n << ", errno=" << errno;
        break;
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    close(pipefd[0]);
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, CloseAnyFdReleasesOwnerPosixLocks) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    int fd2 = open(tf.path.c_str(), O_RDWR);
    ASSERT_GE(fd2, 0);

    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    int p2c[2] = {-1, -1};
    int c2p[2] = {-1, -1};
    ASSERT_EQ(0, pipe(p2c));
    ASSERT_EQ(0, pipe(c2p));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(p2c[1]);
        close(c2p[0]);

        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(1);
        }

        int err = set_lock_errno(fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16);
        if (!is_lock_conflict_errno(err)) {
            _exit(2);
        }

        char blocked = 'B';
        write_or_die(c2p[1], &blocked, 1, 5);

        char go = 0;
        if (read(p2c[0], &go, 1) != 1) {
            _exit(3);
        }

        err = set_lock_errno(fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16);
        if (err != 0) {
            _exit(4);
        }

        close(fd);
        close(p2c[0]);
        close(c2p[1]);
        _exit(0);
    }

    close(p2c[0]);
    close(c2p[1]);

    char blocked = 0;
    ASSERT_EQ(1, read(c2p[0], &blocked, 1));
    ASSERT_EQ('B', blocked);

    // Linux 语义：关闭同 inode 的任意 fd，都会释放本进程在该 inode 上的全部 POSIX 锁。
    close(fd2);

    char go = 'G';
    ASSERT_EQ(1, write(p2c[1], &go, 1));

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    close(p2c[1]);
    close(c2p[0]);
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, NegativeLenRangeAndInvalidCase) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);

    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 10, -5));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(1);
        }

        struct flock out = {};
        int err = get_lock_errno(fd, F_WRLCK, SEEK_SET, 5, 5, &out);
        if (err != 0) {
            _exit(2);
        }
        if (out.l_type != F_WRLCK || out.l_start != 5 || out.l_len != 5) {
            _exit(3);
        }

        err = set_lock_errno(fd, F_SETLK, F_WRLCK, SEEK_SET, 2, -5);
        if (err != EINVAL) {
            _exit(4);
        }

        close(fd);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    EXPECT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0));
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, SetlkwDeadlockDetection) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    close(tf.fd);

    int p1_ready[2] = {-1, -1};
    int p2_ready[2] = {-1, -1};
    int p1_go[2] = {-1, -1};
    int p2_go[2] = {-1, -1};
    int p1_res[2] = {-1, -1};
    int p2_res[2] = {-1, -1};
    ASSERT_EQ(0, pipe(p1_ready));
    ASSERT_EQ(0, pipe(p2_ready));
    ASSERT_EQ(0, pipe(p1_go));
    ASSERT_EQ(0, pipe(p2_go));
    ASSERT_EQ(0, pipe(p1_res));
    ASSERT_EQ(0, pipe(p2_res));

    pid_t p1 = fork();
    ASSERT_GE(p1, 0);
    if (p1 == 0) {
        close(p1_ready[0]);
        close(p2_ready[0]);
        close(p2_ready[1]);
        close(p1_go[1]);
        close(p2_go[0]);
        close(p2_go[1]);
        close(p1_res[0]);
        close(p2_res[0]);
        close(p2_res[1]);

        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(1);
        }
        if (set_lock_errno(fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 10) != 0) {
            _exit(2);
        }

        char ready = 'R';
        write_or_die(p1_ready[1], &ready, 1, 3);

        char go = 0;
        if (read(p1_go[0], &go, 1) != 1) {
            _exit(4);
        }

        int err = set_lock_errno(fd, F_SETLKW, F_WRLCK, SEEK_SET, 20, 10);
        write_or_die(p1_res[1], &err, sizeof(err), 5);
        close(fd);
        _exit(0);
    }

    pid_t p2 = fork();
    ASSERT_GE(p2, 0);
    if (p2 == 0) {
        close(p2_ready[0]);
        close(p1_ready[0]);
        close(p1_ready[1]);
        close(p2_go[1]);
        close(p1_go[0]);
        close(p1_go[1]);
        close(p2_res[0]);
        close(p1_res[0]);
        close(p1_res[1]);

        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(1);
        }
        if (set_lock_errno(fd, F_SETLK, F_WRLCK, SEEK_SET, 20, 10) != 0) {
            _exit(2);
        }

        char ready = 'R';
        write_or_die(p2_ready[1], &ready, 1, 3);

        char go = 0;
        if (read(p2_go[0], &go, 1) != 1) {
            _exit(4);
        }

        int err = set_lock_errno(fd, F_SETLKW, F_WRLCK, SEEK_SET, 0, 10);
        write_or_die(p2_res[1], &err, sizeof(err), 5);
        close(fd);
        _exit(0);
    }

    close(p1_ready[1]);
    close(p2_ready[1]);
    close(p1_go[0]);
    close(p2_go[0]);
    close(p1_res[1]);
    close(p2_res[1]);

    char ready = 0;
    ASSERT_EQ(1, read(p1_ready[0], &ready, 1));
    ASSERT_EQ('R', ready);
    ASSERT_EQ(1, read(p2_ready[0], &ready, 1));
    ASSERT_EQ('R', ready);

    char go = 'G';
    ASSERT_EQ(1, write(p1_go[1], &go, 1));
    ASSERT_EQ(1, write(p2_go[1], &go, 1));
    close(p1_go[1]);
    close(p2_go[1]);

    int err1 = 0;
    int err2 = 0;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(err1)), read(p1_res[0], &err1, sizeof(err1)));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(err2)), read(p2_res[0], &err2, sizeof(err2)));

    int st1 = 0;
    int st2 = 0;
    bool done1 = waitpid_with_timeout(p1, &st1, 5000);
    bool done2 = waitpid_with_timeout(p2, &st2, 5000);
    if (!done1) {
        kill(p1, SIGKILL);
        waitpid(p1, &st1, 0);
    }
    if (!done2) {
        kill(p2, SIGKILL);
        waitpid(p2, &st2, 0);
    }

    ASSERT_TRUE(done1 && done2) << "potential deadlock hang: p1_done=" << done1
                                << ", p2_done=" << done2;
    ASSERT_TRUE(WIFEXITED(st1));
    ASSERT_TRUE(WIFEXITED(st2));
    EXPECT_EQ(0, WEXITSTATUS(st1));
    EXPECT_EQ(0, WEXITSTATUS(st2));

    EXPECT_TRUE(err1 == EDEADLK || err2 == EDEADLK)
        << "expected at least one EDEADLK, got err1=" << err1 << ", err2=" << err2;

    close(p1_ready[0]);
    close(p2_ready[0]);
    close(p1_res[0]);
    close(p2_res[0]);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, SetlkwInterruptedBySignal) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    int ready_pipe[2] = {-1, -1};
    int res_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe));
    ASSERT_EQ(0, pipe(res_pipe));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready_pipe[0]);
        close(res_pipe[0]);

        struct sigaction sa = {};
        sa.sa_handler = [](int) {};
        sigemptyset(&sa.sa_mask);
        sa.sa_flags = 0;
        if (sigaction(SIGUSR1, &sa, nullptr) != 0) {
            _exit(1);
        }

        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(2);
        }

        char ready = 'R';
        write_or_die(ready_pipe[1], &ready, 1, 3);

        int err = set_lock_errno(fd, F_SETLKW, F_WRLCK, SEEK_SET, 0, 16);
        write_or_die(res_pipe[1], &err, sizeof(err), 4);

        close(fd);
        close(ready_pipe[1]);
        close(res_pipe[1]);
        _exit(0);
    }

    close(ready_pipe[1]);
    close(res_pipe[1]);

    char ready = 0;
    ASSERT_EQ(1, read(ready_pipe[0], &ready, 1));
    ASSERT_EQ('R', ready);

    usleep(50 * 1000);
    ASSERT_EQ(0, kill(child, SIGUSR1));

    int err = 0;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(err)), read(res_pipe[0], &err, sizeof(err)));
    EXPECT_EQ(EINTR, err);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    EXPECT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0));
    close(ready_pipe[0]);
    close(res_pipe[0]);
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, CloseOtherFdUnblocksSetlkwWaiter) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    int fd2 = open(tf.path.c_str(), O_RDWR);
    ASSERT_GE(fd2, 0);

    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    int child_done[2] = {-1, -1};
    ASSERT_EQ(0, pipe(child_done));
    int flags = fcntl(child_done[0], F_GETFL, 0);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(child_done[0], F_SETFL, flags | O_NONBLOCK));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(child_done[0]);
        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(1);
        }

        int err = set_lock_errno(fd, F_SETLKW, F_WRLCK, SEEK_SET, 0, 16);
        if (err != 0) {
            _exit(2);
        }

        char done = 'D';
        write_or_die(child_done[1], &done, 1, 3);
        close(fd);
        close(child_done[1]);
        _exit(0);
    }

    close(child_done[1]);
    sleep(1);

    char done = 0;
    errno = 0;
    ssize_t n = read(child_done[0], &done, 1);
    EXPECT_EQ(-1, n);
    EXPECT_EQ(EAGAIN, errno);

    // 关闭同 inode 的另一个 fd，按 Linux 语义应释放本进程该 inode 上全部 POSIX 锁并唤醒等待者。
    close(fd2);

    for (;;) {
        n = read(child_done[0], &done, 1);
        if (n == 1) {
            break;
        }
        if (n == -1 && errno == EAGAIN) {
            usleep(10 * 1000);
            continue;
        }
        FAIL() << "unexpected read result n=" << n << ", errno=" << errno;
        break;
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    close(child_done[0]);
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, ForkChildDoesNotInheritParentPosixLock) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        // Linux 语义：fork 后子进程不继承父进程的 POSIX record lock。
        int err = set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16);
        if (!is_lock_conflict_errno(err)) {
            _exit(1);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    EXPECT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0));
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, ForkChildUnlockMustNotReleaseParentPosixLock) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        // 子进程对同区间做 F_UNLCK 不应影响父进程持有的锁。
        int err = set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 16);
        if (err != 0) {
            _exit(1);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));

    pid_t checker = fork();
    ASSERT_GE(checker, 0);
    if (checker == 0) {
        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(2);
        }
        int err = set_lock_errno(fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16);
        close(fd);
        if (!is_lock_conflict_errno(err)) {
            _exit(3);
        }
        _exit(0);
    }

    ASSERT_EQ(checker, waitpid(checker, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    EXPECT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0));
    close(tf.fd);
    unlink(tf.path.c_str());
}

TEST(FcntlLock, ForkChildCloseMustNotReleaseParentPosixLock) {
    auto tf = make_temp_file();
    ASSERT_GE(tf.fd, 0);
    ASSERT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16));

    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(tf.fd);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    ASSERT_EQ(0, WEXITSTATUS(status));

    pid_t checker = fork();
    ASSERT_GE(checker, 0);
    if (checker == 0) {
        int fd = open(tf.path.c_str(), O_RDWR);
        if (fd < 0) {
            _exit(1);
        }
        int err = set_lock_errno(fd, F_SETLK, F_WRLCK, SEEK_SET, 0, 16);
        close(fd);
        if (!is_lock_conflict_errno(err)) {
            _exit(2);
        }
        _exit(0);
    }

    ASSERT_EQ(checker, waitpid(checker, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    EXPECT_EQ(0, set_lock_errno(tf.fd, F_SETLK, F_UNLCK, SEEK_SET, 0, 0));
    close(tf.fd);
    unlink(tf.path.c_str());
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
