// ptrace stop generation / typed reason regression tests.

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <chrono>
#include <poll.h>
#include <sched.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <gtest/gtest.h>

#ifndef PTRACE_SEIZE
#define PTRACE_SEIZE 0x4206
#endif
#ifndef PTRACE_ATTACH
#define PTRACE_ATTACH 16
#endif
#ifndef PTRACE_DETACH
#define PTRACE_DETACH 17
#endif
#ifndef PTRACE_CONT
#define PTRACE_CONT 7
#endif
#ifndef PTRACE_INTERRUPT
#define PTRACE_INTERRUPT 0x4207
#endif
#ifndef PTRACE_LISTEN
#define PTRACE_LISTEN 0x4208
#endif
#ifndef PTRACE_GETSIGINFO
#define PTRACE_GETSIGINFO 0x4202
#endif
#ifndef PTRACE_SETSIGINFO
#define PTRACE_SETSIGINFO 0x4203
#endif

namespace {

constexpr int kPtraceEventStop = 128;

long ptrace_call(long request, pid_t pid, unsigned long addr,
                 unsigned long data) {
    return syscall(SYS_ptrace, request, pid, addr, data);
}

bool WaitPidUntil(pid_t child, int* status, int options,
                  std::chrono::milliseconds timeout = std::chrono::seconds(2)) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    for (;;) {
        const pid_t result = waitpid(child, status, options | WNOHANG);
        if (result == child) {
            return true;
        }
        if (result < 0 && errno != EINTR) {
            return false;
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            errno = ETIMEDOUT;
            return false;
        }
        sched_yield();
    }
}

bool WaitIdUntil(pid_t child, siginfo_t* info, int options,
                 std::chrono::milliseconds timeout = std::chrono::seconds(2)) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    for (;;) {
        *info = {};
        errno = 0;
        const int result = waitid(P_PID, child, info, options | WNOHANG);
        if (result == 0 && info->si_pid == child) {
            return true;
        }
        if (result < 0 && errno != EINTR) {
            return false;
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            errno = ETIMEDOUT;
            return false;
        }
        sched_yield();
    }
}

bool ReadByteUntil(int fd, char* byte,
                   std::chrono::milliseconds timeout = std::chrono::seconds(2)) {
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    for (;;) {
        const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
            deadline - std::chrono::steady_clock::now());
        if (remaining.count() <= 0) {
            errno = ETIMEDOUT;
            return false;
        }
        pollfd pfd = {.fd = fd, .events = POLLIN, .revents = 0};
        const int result = poll(&pfd, 1, static_cast<int>(remaining.count()));
        if (result > 0) return read(fd, byte, 1) == 1;
        if (result == 0) {
            errno = ETIMEDOUT;
            return false;
        }
        if (errno != EINTR) return false;
    }
}

class ChildGuard {
public:
    explicit ChildGuard(pid_t child) : child_(child) {}
    ChildGuard(const ChildGuard&) = delete;
    ChildGuard& operator=(const ChildGuard&) = delete;

    ~ChildGuard() {
        if (child_ > 0) {
            kill(child_, SIGKILL);
            int status = 0;
            WaitPidUntil(child_, &status, 0);
        }
    }

    void Release() { child_ = -1; }

private:
    pid_t child_;
};

void ExitOnSigusr2(int signal, siginfo_t* info, void*) {
    _exit(signal == SIGUSR2 && info != nullptr && info->si_signo == SIGUSR2 &&
                  info->si_code == SI_TKILL
              ? 0
              : 61);
}

TEST(PtraceStopState, WnowaitPreservesMutableSignalInjection) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        struct sigaction action = {};
        action.sa_sigaction = ExitOnSigusr2;
        action.sa_flags = SA_SIGINFO;
        sigemptyset(&action.sa_mask);
        if (sigaction(SIGUSR2, &action, nullptr) != 0 ||
            ptrace_call(0 /* PTRACE_TRACEME */, 0, 0, 0) != 0) {
            _exit(62);
        }
        raise(SIGUSR1);
        _exit(63);
    }
    ChildGuard guard(child);

    siginfo_t waited = {};
    ASSERT_TRUE(WaitIdUntil(child, &waited, WSTOPPED | WNOWAIT));
    ASSERT_EQ(child, waited.si_pid);
    ASSERT_EQ(CLD_TRAPPED, waited.si_code);
    ASSERT_EQ(SIGUSR1, waited.si_status);

    ASSERT_TRUE(WaitIdUntil(child, &waited, WSTOPPED));
    ASSERT_EQ(child, waited.si_pid);
    ASSERT_EQ(CLD_TRAPPED, waited.si_code);
    ASSERT_EQ(SIGUSR1, waited.si_status);

    int status = 0;
    ASSERT_EQ(0, waitpid(child, &status, WNOHANG));

    siginfo_t injected = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&injected)));
    ASSERT_EQ(SIGUSR1, injected.si_signo);
    ASSERT_EQ(SI_TKILL, injected.si_code);
    injected.si_signo = SIGUSR2;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&injected)));
    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, child, 0, SIGUSR2));

    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    guard.Release();
}

TEST(PtraceStopState, MutableSiginfoAndStopGenerationFollowLinux) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        for (;;) {
            pause();
        }
    }
    ChildGuard guard(child);

    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, child, 0, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_INTERRUPT, child, 0, 0));

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP | (kPtraceEventStop << 8), status >> 8);

    siginfo_t info = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&info)));
    // Linux 6.6 PTRACE_LISTEN intentionally checks mutable last_siginfo.
    const int event_stop_code = info.si_code;
    info.si_code = 0;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&info)));
    errno = 0;
    ASSERT_EQ(-1, ptrace_call(PTRACE_LISTEN, child, 0, 0));
    ASSERT_EQ(EIO, errno);

    // Restore the event-stop code. LISTEN must then consume this same stop.
    info.si_code = event_stop_code;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&info)));
    ASSERT_EQ(0, ptrace_call(PTRACE_LISTEN, child, 0, 0));

    // Queue the next stop while the previous stop waiter is still completing.
    // The old generation must not clear the newly published event-stop.
    ASSERT_EQ(0, ptrace_call(PTRACE_INTERRUPT, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP | (kPtraceEventStop << 8), status >> 8);

    // SIGKILL is never suppressible by a ptrace stop and needs no CONT.
    ASSERT_EQ(0, kill(child, SIGKILL));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGKILL, WTERMSIG(status));
    guard.Release();
}

TEST(PtraceStopState, AttachToGroupStopUsesTraceeWaiter) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        for (;;) {
            pause();
        }
    }
    ChildGuard guard(child);

    ASSERT_EQ(0, kill(child, SIGSTOP));
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status, WUNTRACED));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));

    ASSERT_EQ(0, ptrace_call(PTRACE_ATTACH, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));

    siginfo_t info = {};
    errno = 0;
    ASSERT_EQ(-1, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                              reinterpret_cast<unsigned long>(&info)));
    ASSERT_EQ(EINVAL, errno);

    // A group-stop resume has an actual ptrace_stop waiter, but Linux ignores
    // resume data for do_jobctl_trap rather than queueing another delivery-stop.
    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, child, 0, 0));
    ASSERT_EQ(0, waitpid(child, &status, WUNTRACED | WNOHANG));
    ASSERT_EQ(0, kill(child, SIGCONT));
    ASSERT_TRUE(WaitPidUntil(child, &status, WCONTINUED));
    ASSERT_TRUE(WIFCONTINUED(status));
    ASSERT_EQ(0, kill(child, SIGKILL));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGKILL, WTERMSIG(status));
    guard.Release();
}

TEST(PtraceStopState, InterruptCannotLoseRunnableToPipeSleepTransition) {
    constexpr int kRounds = 32;
    int ready[2] = {};
    int release[2] = {};
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));

    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        for (int round = 0; round < kRounds; ++round) {
            const char byte = 1;
            if (write(ready[1], &byte, 1) != 1) _exit(70);
            char token = 0;
            if (read(release[0], &token, 1) != 1) _exit(71);
        }
        _exit(0);
    }
    ChildGuard guard(child);
    close(ready[1]);
    close(release[0]);
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, child, 0, 0));

    for (int round = 0; round < kRounds; ++round) {
        char byte = 0;
        ASSERT_TRUE(ReadByteUntil(ready[0], &byte));
        // The child is runnable immediately after publishing ready and is
        // about to enter an interruptible pipe read. EVENT_STOP must win both
        // sides of that Runnable -> Blocked transition.
        ASSERT_EQ(0, ptrace_call(PTRACE_INTERRUPT, child, 0, 0));
        int status = 0;
        ASSERT_TRUE(WaitPidUntil(child, &status, 0));
        ASSERT_TRUE(WIFSTOPPED(status));
        ASSERT_EQ(SIGTRAP | (kPtraceEventStop << 8), status >> 8);
        ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
        ASSERT_EQ(1, write(release[1], &byte, 1));
    }

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    guard.Release();
    close(ready[0]);
    close(release[1]);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
