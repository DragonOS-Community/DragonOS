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
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
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

void KillAndReap(pid_t child) {
    if (child <= 0) {
        return;
    }
    kill(child, SIGKILL);
    while (waitpid(child, nullptr, 0) < 0 && errno == EINTR) {
    }
}

class ChildProcessGuard {
public:
    explicit ChildProcessGuard(pid_t child) : child_(child) {}
    ChildProcessGuard(const ChildProcessGuard&) = delete;
    ChildProcessGuard& operator=(const ChildProcessGuard&) = delete;

    ~ChildProcessGuard() { KillAndReap(child_); }

    void Release() { child_ = -1; }

private:
    pid_t child_;
};

class SignalMaskGuard {
public:
    explicit SignalMaskGuard(const sigset_t& blocked) : valid_(false) {
        valid_ = sigprocmask(SIG_BLOCK, &blocked, &old_mask_) == 0;
    }
    SignalMaskGuard(const SignalMaskGuard&) = delete;
    SignalMaskGuard& operator=(const SignalMaskGuard&) = delete;

    ~SignalMaskGuard() {
        if (valid_) {
            sigprocmask(SIG_SETMASK, &old_mask_, nullptr);
        }
    }

    bool valid() const { return valid_; }

private:
    sigset_t old_mask_ {};
    bool valid_;
};

struct LastThreadExitArgs {
    int ready_fd;
    pid_t leader_tid;
};

bool PinCurrentTaskToOneAllowedCpu() {
    cpu_set_t available;
    CPU_ZERO(&available);
    if (sched_getaffinity(0, sizeof(available), &available) != 0) {
        return false;
    }

    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &available)) {
            continue;
        }
        cpu_set_t target;
        CPU_ZERO(&target);
        CPU_SET(cpu, &target);
        return sched_setaffinity(0, sizeof(target), &target) == 0;
    }
    errno = EINVAL;
    return false;
}

char ReadProcTaskState(pid_t tgid, pid_t tid) {
    char path[128] {};
    snprintf(path, sizeof(path), "/proc/%d/task/%d/stat", tgid, tid);
    FILE* stat_file = fopen(path, "r");
    if (stat_file == nullptr) {
        return '\0';
    }

    char line[1024] {};
    char state = '\0';
    if (fgets(line, sizeof(line), stat_file) != nullptr) {
        char* comm_end = strrchr(line, ')');
        if (comm_end != nullptr && comm_end[1] == ' ') {
            state = comm_end[2];
        }
    }
    fclose(stat_file);
    return state;
}

void* ExitAfterLeaderThread(void* raw_args) {
    auto* args = static_cast<LastThreadExitArgs*>(raw_args);
    const int ready_fd = args->ready_fd;
    const pid_t leader_tid = args->leader_tid;
    WriteIntOrExit(ready_fd, static_cast<int>(syscall(SYS_gettid)));

    for (int i = 0; i < 5000; ++i) {
        if (ReadProcTaskState(leader_tid, leader_tid) == 'Z') {
            if (getuid() == 0 && syscall(SYS_setuid, 1234) != 0) {
                _exit(124);
            }
            return nullptr;
        }
        SleepForMillis(1);
    }
    _exit(123);
}

void* PauseForeverThread(void*) {
    for (;;) {
        pause();
    }
    return nullptr;
}

void* ReadyPauseThread(void* arg) {
    int ready_fd = *static_cast<int*>(arg);
    WriteByteOrExit(ready_fd, 'R');
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

TEST(ProcessSignalFork, SigchldInfoUsesGroupLeaderWhenLastNonleaderExits) {
    const uid_t leader_uid = getuid();
    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGCHLD);
    SignalMaskGuard mask_guard(blocked);
    ASSERT_TRUE(mask_guard.valid()) << "sigprocmask failed: errno=" << errno << " ("
                                    << strerror(errno) << ")";

    int ready_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << "pipe failed: " << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        close(ready_pipe[0]);
        if (!PinCurrentTaskToOneAllowedCpu()) {
            _exit(120);
        }
        LastThreadExitArgs args {ready_pipe[1], getpid()};
        pthread_t worker {};
        if (pthread_create(&worker, nullptr, ExitAfterLeaderThread, &args) != 0) {
            _exit(121);
        }
        syscall(SYS_exit, 0);
        _exit(122);
    }
    ChildProcessGuard child_guard(child);

    close(ready_pipe[1]);
    int worker_tid = -1;
    ASSERT_TRUE(ReadInt(ready_pipe[0], &worker_tid)) << "worker did not report its tid";
    close(ready_pipe[0]);
    ASSERT_GT(worker_tid, 0);
    ASSERT_NE(worker_tid, child);

    siginfo_t info {};
    timespec timeout {};
    timeout.tv_sec = 5;
    ASSERT_EQ(SIGCHLD, sigtimedwait(&blocked, &info, &timeout))
        << "sigtimedwait failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(child, info.si_pid);
    EXPECT_NE(worker_tid, info.si_pid);
    EXPECT_EQ(leader_uid, info.si_uid);
    EXPECT_EQ(CLD_EXITED, info.si_code);
    EXPECT_EQ(0, info.si_status);

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    child_guard.Release();
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

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

TEST(ProcessSignalFork, ProcessGroupSignalIsDeliveredOncePerThreadGroup) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        if (setsid() < 0) {
            _exit(120);
        }

        const int signal = SIGRTMIN;
        sigset_t blocked;
        sigemptyset(&blocked);
        sigaddset(&blocked, signal);
        if (sigprocmask(SIG_BLOCK, &blocked, nullptr) != 0) {
            _exit(121);
        }

        int ready_pipe[2] = {-1, -1};
        if (pipe(ready_pipe) != 0) {
            _exit(122);
        }
        pthread_t thread {};
        if (pthread_create(&thread, nullptr, ReadyPauseThread, &ready_pipe[1]) != 0
            || !ReadByte(ready_pipe[0])) {
            _exit(123);
        }

        if (kill(0, signal) != 0) {
            _exit(124);
        }

        timespec timeout {};
        timeout.tv_nsec = 10 * 1000 * 1000;
        if (sigtimedwait(&blocked, nullptr, &timeout) != signal) {
            _exit(125);
        }
        errno = 0;
        if (sigtimedwait(&blocked, nullptr, &timeout) != -1 || errno != EAGAIN) {
            _exit(126);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(ProcessSignalFork, ThreadCreationPreservesInheritedGroupAndSingleDelivery) {
    int helper_ready[2] = {-1, -1};
    int child_gate[2] = {-1, -1};
    ASSERT_EQ(0, pipe(helper_ready)) << "pipe failed: " << strerror(errno);
    ASSERT_EQ(0, pipe(child_gate)) << "pipe failed: " << strerror(errno);

    pid_t helper = fork();
    ASSERT_GE(helper, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (helper == 0) {
        if (setpgid(0, 0) != 0) {
            _exit(127);
        }
        sigset_t blocked;
        sigemptyset(&blocked);
        sigaddset(&blocked, SIGRTMIN);
        if (sigprocmask(SIG_BLOCK, &blocked, nullptr) != 0) {
            _exit(128);
        }
        WriteByteOrExit(helper_ready[1], 'R');
        for (;;) {
            pause();
        }
    }
    ChildProcessGuard helper_guard(helper);
    ASSERT_TRUE(ReadByte(helper_ready[0])) << "helper did not become ready";

    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child == 0) {
        if (!ReadByte(child_gate[0])) {
            _exit(129);
        }
        const pid_t process_group = getpgrp();
        const pid_t session = getsid(0);
        if (process_group != helper || session < 0) {
            _exit(130);
        }

        const int signal = SIGRTMIN;
        sigset_t blocked;
        sigemptyset(&blocked);
        sigaddset(&blocked, signal);
        if (sigprocmask(SIG_BLOCK, &blocked, nullptr) != 0) {
            _exit(131);
        }

        int ready_pipe[2] = {-1, -1};
        if (pipe(ready_pipe) != 0) {
            _exit(132);
        }
        pthread_t thread {};
        if (pthread_create(&thread, nullptr, ReadyPauseThread, &ready_pipe[1]) != 0
            || !ReadByte(ready_pipe[0])) {
            _exit(133);
        }

        if (getpgrp() != process_group) {
            _exit(134);
        }
        if (getsid(0) != session) {
            _exit(135);
        }
        if (kill(0, signal) != 0) {
            _exit(136);
        }

        timespec timeout {};
        timeout.tv_nsec = 10 * 1000 * 1000;
        if (sigtimedwait(&blocked, nullptr, &timeout) != signal) {
            _exit(137);
        }
        errno = 0;
        if (sigtimedwait(&blocked, nullptr, &timeout) != -1 || errno != EAGAIN) {
            _exit(138);
        }
        _exit(0);
    }
    ChildProcessGuard child_guard(child);

    ASSERT_EQ(0, setpgid(child, helper))
        << "setpgid failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(1, write(child_gate[1], "G", 1));

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    child_guard.Release();
    ASSERT_EQ(0, kill(helper, SIGKILL));
    int helper_status = 0;
    ASSERT_EQ(helper, waitpid(helper, &helper_status, 0));
    helper_guard.Release();
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
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
