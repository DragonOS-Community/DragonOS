#include <gtest/gtest.h>

#ifdef _WIN32

TEST(ProcessSignalFork, PosixForkAndJobControlUnavailableOnWindows) {
    GTEST_SKIP() << "Windows does not provide POSIX fork/SIGSTOP/SIGCONT semantics";
}

#else

#include <errno.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

void SleepForMillis(long millis) {
    timespec ts {};
    ts.tv_sec = millis / 1000;
    ts.tv_nsec = (millis % 1000) * 1000 * 1000;
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
    }
}

bool WaitForExit(pid_t child, int* status, int rounds) {
    for (int i = 0; i < rounds; ++i) {
        pid_t ret = waitpid(child, status, WNOHANG);
        if (ret == child) {
            return true;
        }
        if (ret < 0 && errno != EINTR) {
            return false;
        }
        SleepForMillis(10);
    }
    return false;
}

void WriteByteOrExit(int fd, char value) {
    ssize_t n = write(fd, &value, sizeof(value));
    if (n != static_cast<ssize_t>(sizeof(value))) {
        _exit(120);
    }
}

bool ReadByte(int fd) {
    char value = 0;
    ssize_t n = read(fd, &value, sizeof(value));
    return n == static_cast<ssize_t>(sizeof(value));
}

void WriteIntOrExit(int fd, int value) {
    ssize_t n = write(fd, &value, sizeof(value));
    if (n != static_cast<ssize_t>(sizeof(value))) {
        _exit(120);
    }
}

bool ReadInt(int fd, int* value) {
    ssize_t n = read(fd, value, sizeof(*value));
    return n == static_cast<ssize_t>(sizeof(*value));
}

void* PauseForeverThread(void*) {
    for (;;) {
        pause();
    }
    return nullptr;
}

void RunMultithreadedSignalChild(int ready_fd) {
    pthread_t threads[3] {};
    for (pthread_t& thread : threads) {
        if (pthread_create(&thread, nullptr, PauseForeverThread, nullptr) != 0) {
            _exit(122);
        }
    }

    WriteByteOrExit(ready_fd, 'R');
    for (;;) {
        pause();
    }
}

}  // namespace

TEST(ProcessSignalFork, StopContinueRaceDoesNotLeaveChildStuck) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        for (;;) {
            asm volatile("" ::: "memory");
        }
    }

    for (int i = 0; i < 200; ++i) {
        ASSERT_EQ(0, kill(child, SIGSTOP))
            << "SIGSTOP failed at iteration " << i << ": errno=" << errno << " ("
            << strerror(errno) << ")";
        sched_yield();
        ASSERT_EQ(0, kill(child, SIGCONT))
            << "SIGCONT failed at iteration " << i << ": errno=" << errno << " ("
            << strerror(errno) << ")";

        int event_status = 0;
        while (waitpid(child, &event_status, WUNTRACED | WCONTINUED | WNOHANG) == child) {
        }
    }

    ASSERT_EQ(0, kill(child, SIGCONT))
        << "final SIGCONT failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, kill(child, SIGTERM))
        << "SIGTERM failed: errno=" << errno << " (" << strerror(errno) << ")";

    int status = 0;
    if (!WaitForExit(child, &status, 300)) {
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        FAIL() << "child did not exit after SIGSTOP/SIGCONT race stress";
    }

    ASSERT_TRUE(WIFSIGNALED(status) || WIFEXITED(status)) << "child status=" << status;
    if (WIFSIGNALED(status)) {
        EXPECT_EQ(SIGTERM, WTERMSIG(status));
    } else {
        EXPECT_EQ(0, WEXITSTATUS(status));
    }
}

TEST(ProcessSignalFork, ProcessDirectedSigkillKillsStoppedChildWithPendingStopEvent) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        for (;;) {
            asm volatile("" ::: "memory");
        }
    }

    ASSERT_EQ(0, kill(child, SIGSTOP))
        << "SIGSTOP failed: errno=" << errno << " (" << strerror(errno) << ")";

    siginfo_t info {};
    bool observed_stop = false;
    for (int i = 0; i < 300; ++i) {
        memset(&info, 0, sizeof(info));
        int ret = waitid(P_PID, static_cast<id_t>(child), &info, WSTOPPED | WNOWAIT | WNOHANG);
        ASSERT_EQ(0, ret) << "waitid(WSTOPPED|WNOWAIT) failed: errno=" << errno << " ("
                          << strerror(errno) << ")";
        if (info.si_pid == child && info.si_code == CLD_STOPPED) {
            observed_stop = true;
            break;
        }
        SleepForMillis(10);
    }
    ASSERT_TRUE(observed_stop) << "child did not report a pending stop event";

    ASSERT_EQ(0, kill(child, SIGKILL))
        << "SIGKILL failed: errno=" << errno << " (" << strerror(errno) << ")";

    int status = 0;
    if (!WaitForExit(child, &status, 300)) {
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        FAIL() << "stopped child did not exit after process-directed SIGKILL";
    }

    ASSERT_TRUE(WIFSIGNALED(status)) << "child status=" << status;
    EXPECT_EQ(SIGKILL, WTERMSIG(status));
}

TEST(ProcessSignalFork, ProcessDirectedSigkillKillsMultithreadedChild) {
    int ready_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << "pipe failed: " << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        close(ready_pipe[0]);
        RunMultithreadedSignalChild(ready_pipe[1]);
    }

    close(ready_pipe[1]);
    ASSERT_TRUE(ReadByte(ready_pipe[0])) << "child did not report pthread readiness";
    close(ready_pipe[0]);

    ASSERT_EQ(0, kill(child, SIGKILL))
        << "SIGKILL failed: errno=" << errno << " (" << strerror(errno) << ")";

    int status = 0;
    if (!WaitForExit(child, &status, 300)) {
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        FAIL() << "multithreaded child did not exit after process-directed SIGKILL";
    }

    ASSERT_TRUE(WIFSIGNALED(status)) << "child status=" << status;
    EXPECT_EQ(SIGKILL, WTERMSIG(status));
}

TEST(ProcessSignalFork, ProcessDirectedSigtermPreservesExitSignalForMultithreadedChild) {
    int ready_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << "pipe failed: " << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        close(ready_pipe[0]);
        RunMultithreadedSignalChild(ready_pipe[1]);
    }

    close(ready_pipe[1]);
    ASSERT_TRUE(ReadByte(ready_pipe[0])) << "child did not report pthread readiness";
    close(ready_pipe[0]);

    ASSERT_EQ(0, kill(child, SIGTERM))
        << "SIGTERM failed: errno=" << errno << " (" << strerror(errno) << ")";

    int status = 0;
    if (!WaitForExit(child, &status, 300)) {
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        FAIL() << "multithreaded child did not exit after process-directed SIGTERM";
    }

    ASSERT_TRUE(WIFSIGNALED(status)) << "child status=" << status;
    EXPECT_EQ(SIGTERM, WTERMSIG(status));
}

TEST(ProcessSignalFork, AncestorPidNamespaceCanKillPidNamespaceInit) {
    int pid_pipe[2] = {-1, -1};
    int status_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pid_pipe)) << "pid pipe failed: " << strerror(errno);
    ASSERT_EQ(0, pipe(status_pipe)) << "status pipe failed: " << strerror(errno);

    pid_t supervisor = fork();
    ASSERT_GE(supervisor, 0) << "fork supervisor failed: errno=" << errno << " ("
                             << strerror(errno) << ")";

    if (supervisor == 0) {
        close(pid_pipe[0]);
        close(status_pipe[0]);

        if (unshare(CLONE_NEWPID) != 0) {
            WriteIntOrExit(status_pipe[1], errno == EPERM ? -EPERM : -errno);
            _exit(0);
        }

        pid_t init = fork();
        if (init < 0) {
            WriteIntOrExit(status_pipe[1], -errno);
            _exit(0);
        }

        if (init == 0) {
            for (;;) {
                pause();
            }
        }

        WriteIntOrExit(pid_pipe[1], static_cast<int>(init));

        int status = 0;
        if (waitpid(init, &status, 0) != init) {
            WriteIntOrExit(status_pipe[1], -errno);
            _exit(0);
        }

        WriteIntOrExit(status_pipe[1], status);
        _exit(0);
    }

    close(pid_pipe[1]);
    close(status_pipe[1]);

    int init_pid = -1;
    int reported_status = 0;
    ASSERT_TRUE(ReadInt(pid_pipe[0], &init_pid) || ReadInt(status_pipe[0], &reported_status))
        << "supervisor did not report pid namespace init pid or setup status";
    close(pid_pipe[0]);

    if (init_pid < 0) {
        ASSERT_LT(reported_status, 0);
        int err = -reported_status;
        int supervisor_status = 0;
        waitpid(supervisor, &supervisor_status, 0);
        if (err == EPERM || err == EINVAL || err == ENOSYS) {
            GTEST_SKIP() << "PID namespace unshare unsupported in this environment: errno=" << err;
        }
        FAIL() << "supervisor setup failed: errno=" << err << " (" << strerror(err) << ")";
    }

    ASSERT_EQ(0, kill(static_cast<pid_t>(init_pid), SIGKILL))
        << "SIGKILL to pid namespace init from ancestor failed: errno=" << errno << " ("
        << strerror(errno) << ")";

    ASSERT_TRUE(ReadInt(status_pipe[0], &reported_status))
        << "supervisor did not report pid namespace init wait status";
    close(status_pipe[0]);

    int supervisor_status = 0;
    ASSERT_EQ(supervisor, waitpid(supervisor, &supervisor_status, 0));
    ASSERT_TRUE(WIFEXITED(supervisor_status));
    ASSERT_EQ(0, WEXITSTATUS(supervisor_status));

    ASSERT_TRUE(WIFSIGNALED(reported_status)) << "init status=" << reported_status;
    EXPECT_EQ(SIGKILL, WTERMSIG(reported_status));
}

#endif

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
