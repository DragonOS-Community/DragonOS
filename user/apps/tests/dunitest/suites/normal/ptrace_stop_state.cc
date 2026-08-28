// ptrace stop generation / typed reason regression tests.

#include <errno.h>
#include <linux/netlink.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <chrono>
#include <poll.h>
#include <sched.h>
#include <sys/socket.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <cstdio>
#include <cstring>

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
#ifndef PTRACE_SETOPTIONS
#define PTRACE_SETOPTIONS 0x4200
#endif
#ifndef PTRACE_GETSIGINFO
#define PTRACE_GETSIGINFO 0x4202
#endif
#ifndef PTRACE_GETEVENTMSG
#define PTRACE_GETEVENTMSG 0x4201
#endif
#ifndef PTRACE_SETSIGINFO
#define PTRACE_SETSIGINFO 0x4203
#endif
#ifndef PTRACE_O_SUSPEND_SECCOMP
#define PTRACE_O_SUSPEND_SECCOMP (1UL << 21)
#endif
#ifndef PTRACE_O_TRACECLONE
#define PTRACE_O_TRACECLONE (1UL << 3)
#endif
#ifndef PTRACE_EVENT_CLONE
#define PTRACE_EVENT_CLONE 3
#endif
#ifndef SYS_SECCOMP
#define SYS_SECCOMP 1
#endif
#ifndef AUDIT_ARCH_X86_64
#define AUDIT_ARCH_X86_64 0xc000003eU
#endif

namespace {

constexpr int kPtraceEventStop = 128;
constexpr int kWall = 0x40000000;
constexpr int kThreadGroupMembers = 3;

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

bool RemainsUnreadableUntil(int fd, std::chrono::milliseconds duration) {
    const auto deadline = std::chrono::steady_clock::now() + duration;
    for (;;) {
        const auto remaining = std::chrono::duration_cast<std::chrono::milliseconds>(
            deadline - std::chrono::steady_clock::now());
        if (remaining.count() <= 0) return true;
        pollfd pfd = {.fd = fd, .events = POLLIN, .revents = 0};
        const int result = poll(&pfd, 1, static_cast<int>(remaining.count()));
        if (result > 0) return false;
        if (result == 0) return true;
        if (errno != EINTR) return false;
    }
}

bool ReadExactUntil(int fd, void* buffer, size_t length,
                    std::chrono::milliseconds timeout =
                        std::chrono::seconds(2)) {
    auto* next = static_cast<char*>(buffer);
    size_t remaining = length;
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (remaining > 0) {
        const auto wait = std::chrono::duration_cast<std::chrono::milliseconds>(
            deadline - std::chrono::steady_clock::now());
        if (wait.count() <= 0) {
            errno = ETIMEDOUT;
            return false;
        }
        pollfd pfd = {.fd = fd, .events = POLLIN, .revents = 0};
        const int polled = poll(&pfd, 1, static_cast<int>(wait.count()));
        if (polled == 0) {
            errno = ETIMEDOUT;
            return false;
        }
        if (polled < 0) {
            if (errno == EINTR) continue;
            return false;
        }
        const ssize_t count = read(fd, next, remaining);
        if (count > 0) {
            next += count;
            remaining -= static_cast<size_t>(count);
            continue;
        }
        if (count < 0 && errno == EINTR) continue;
        if (count == 0) errno = EPIPE;
        return false;
    }
    return true;
}

bool WriteExact(int fd, const void* buffer, size_t length) {
    const auto* next = static_cast<const char*>(buffer);
    size_t remaining = length;
    while (remaining > 0) {
        const ssize_t count = write(fd, next, remaining);
        if (count > 0) {
            next += count;
            remaining -= static_cast<size_t>(count);
            continue;
        }
        if (count < 0 && errno == EINTR) continue;
        return false;
    }
    return true;
}

bool WaitForTaskState(pid_t pid, const char* accepted,
                      std::chrono::milliseconds timeout =
                          std::chrono::seconds(2)) {
    char path[64] = {};
    std::snprintf(path, sizeof(path), "/proc/%d/stat", pid);
    const auto deadline = std::chrono::steady_clock::now() + timeout;
    while (std::chrono::steady_clock::now() < deadline) {
        FILE* stat = std::fopen(path, "r");
        if (stat != nullptr) {
            char line[512] = {};
            const bool read_ok = std::fgets(line, sizeof(line), stat) != nullptr;
            std::fclose(stat);
            if (read_ok) {
                const char* comm_end = std::strrchr(line, ')');
                if (comm_end != nullptr && comm_end[1] == ' ' &&
                    std::strchr(accepted, comm_end[2]) != nullptr) {
                    return true;
                }
            }
        }
        sched_yield();
    }
    errno = ETIMEDOUT;
    return false;
}

bool WaitForSleepingProcess(pid_t pid,
                            std::chrono::milliseconds timeout =
                                std::chrono::seconds(2)) {
    return WaitForTaskState(pid, "SD", timeout);
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

struct ThreadRecord {
    pid_t tid;
    int member;
};

struct ControlledThreadArgs {
    int member;
    int ready_fd;
    int command_fd;
};

void ControlledMemberLoop(const ControlledThreadArgs& args) {
    const ThreadRecord record = {
        .tid = static_cast<pid_t>(syscall(SYS_gettid)),
        .member = args.member,
    };
    if (!WriteExact(args.ready_fd, &record, sizeof(record))) _exit(90);
    for (;;) {
        char command = 0;
        ssize_t count;
        do {
            count = read(args.command_fd, &command, 1);
        } while (count < 0 && errno == EINTR);
        if (count != 1 || command != 'R') _exit(91);
        if (!WriteExact(args.ready_fd, &record, sizeof(record))) _exit(92);
    }
}

void* ControlledWorker(void* opaque) {
    ControlledMemberLoop(*static_cast<ControlledThreadArgs*>(opaque));
    return nullptr;
}

struct LateCloneArgs {
    int ready_fd;
    int command_fd;
    int ran_fd;
};

void* LateClonedWorker(void* opaque) {
    const int ran_fd = *static_cast<int*>(opaque);
    const char marker = 'R';
    if (!WriteExact(ran_fd, &marker, 1)) _exit(95);
    for (;;) pause();
}

void* LateCloneCreator(void* opaque) {
    auto* args = static_cast<LateCloneArgs*>(opaque);
    const pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
    if (!WriteExact(args->ready_fd, &tid, sizeof(tid))) _exit(96);

    char command = 0;
    if (read(args->command_fd, &command, 1) != 1 || command != 'C') _exit(97);
    pthread_t late = {};
    if (pthread_create(&late, nullptr, LateClonedWorker, &args->ran_fd) != 0)
        _exit(98);
    for (;;) pause();
}

class ThreadGroupGuard {
public:
    explicit ThreadGroupGuard(pid_t leader) : leader_(leader) {}
    ThreadGroupGuard(const ThreadGroupGuard&) = delete;
    ThreadGroupGuard& operator=(const ThreadGroupGuard&) = delete;

    ~ThreadGroupGuard() {
        if (leader_ <= 0) return;
        kill(leader_, SIGKILL);
        int status = 0;
        if (late_ > 0) WaitPidUntil(late_, &status, kWall);
        if (creator_ > 0) WaitPidUntil(creator_, &status, kWall);
        WaitPidUntil(leader_, &status, 0);
    }

    void SetCreator(pid_t creator) { creator_ = creator; }
    void SetLate(pid_t late) { late_ = late; }

private:
    pid_t leader_;
    pid_t creator_ = -1;
    pid_t late_ = -1;
};

class ControlledThreadGroup {
public:
    ControlledThreadGroup() = default;
    ControlledThreadGroup(const ControlledThreadGroup&) = delete;
    ControlledThreadGroup& operator=(const ControlledThreadGroup&) = delete;

    ~ControlledThreadGroup() {
        if (leader_ > 0) {
            kill(leader_, SIGKILL);
            int status = 0;
            for (int member = 1; member < kThreadGroupMembers; ++member) {
                if (tids_[member] > 0)
                    WaitPidUntil(tids_[member], &status, kWall);
            }
            WaitPidUntil(leader_, &status, 0);
        }
        if (ready_fd_ >= 0) close(ready_fd_);
        for (int member = 0; member < kThreadGroupMembers; ++member) {
            if (command_fds_[member] >= 0) close(command_fds_[member]);
        }
    }

    bool Start() {
        int ready[2] = {-1, -1};
        if (pipe(ready) != 0) return false;
        int commands[kThreadGroupMembers][2] = {};
        for (int member = 0; member < kThreadGroupMembers; ++member) {
            if (pipe(commands[member]) != 0) {
                close(ready[0]);
                close(ready[1]);
                for (int opened = 0; opened < member; ++opened) {
                    close(commands[opened][0]);
                    close(commands[opened][1]);
                }
                return false;
            }
        }

        leader_ = fork();
        if (leader_ < 0) {
            close(ready[0]);
            close(ready[1]);
            for (int member = 0; member < kThreadGroupMembers; ++member) {
                close(commands[member][0]);
                close(commands[member][1]);
            }
            return false;
        }
        if (leader_ == 0) {
            close(ready[0]);
            for (int member = 0; member < kThreadGroupMembers; ++member)
                close(commands[member][1]);

            ControlledThreadArgs args[kThreadGroupMembers] = {};
            pthread_t workers[kThreadGroupMembers - 1] = {};
            for (int member = 0; member < kThreadGroupMembers; ++member) {
                args[member] = {
                    .member = member,
                    .ready_fd = ready[1],
                    .command_fd = commands[member][0],
                };
            }
            for (int member = 1; member < kThreadGroupMembers; ++member) {
                if (pthread_create(&workers[member - 1], nullptr,
                                   ControlledWorker, &args[member]) != 0) {
                    _exit(93);
                }
            }
            ControlledMemberLoop(args[0]);
        }

        close(ready[1]);
        ready_fd_ = ready[0];
        for (int member = 0; member < kThreadGroupMembers; ++member) {
            close(commands[member][0]);
            command_fds_[member] = commands[member][1];
        }

        for (int received = 0; received < kThreadGroupMembers; ++received) {
            ThreadRecord record = {};
            if (!ReadExactUntil(ready_fd_, &record, sizeof(record))) return false;
            if (record.member < 0 || record.member >= kThreadGroupMembers ||
                tids_[record.member] != -1) {
                errno = EPROTO;
                return false;
            }
            tids_[record.member] = record.tid;
        }
        if (tids_[0] != leader_) {
            errno = EPROTO;
            return false;
        }
        return true;
    }

    pid_t leader() const { return leader_; }
    pid_t worker(int index) const { return tids_[index + 1]; }

    bool QueueExecutionProbe(int member) const {
        if (member < 0 || member >= kThreadGroupMembers) return false;
        const char command = 'R';
        return WriteExact(command_fds_[member], &command, 1);
    }

    bool QueueAllExecutionProbes() const {
        for (int member = 0; member < kThreadGroupMembers; ++member) {
            if (!QueueExecutionProbe(member)) return false;
        }
        return true;
    }

    bool WaitForExecutionProbe(int expected_member) const {
        ThreadRecord record = {};
        return ReadExactUntil(ready_fd_, &record, sizeof(record)) &&
               record.member == expected_member &&
               record.tid == tids_[expected_member];
    }

    bool WaitForAllExecutionProbes() const {
        bool acknowledged[kThreadGroupMembers] = {};
        for (int received = 0; received < kThreadGroupMembers; ++received) {
            ThreadRecord record = {};
            if (!ReadExactUntil(ready_fd_, &record, sizeof(record))) return false;
            if (record.member < 0 || record.member >= kThreadGroupMembers ||
                record.tid != tids_[record.member] || acknowledged[record.member]) {
                errno = EPROTO;
                return false;
            }
            acknowledged[record.member] = true;
        }
        return true;
    }

    bool NoExecutionProbeCompletes(std::chrono::milliseconds duration) const {
        return RemainsUnreadableUntil(ready_fd_, duration);
    }

    bool WaitUntilResumed() const {
        return QueueAllExecutionProbes() && WaitForAllExecutionProbes();
    }

private:
    pid_t leader_ = -1;
    pid_t tids_[kThreadGroupMembers] = {-1, -1, -1};
    int ready_fd_ = -1;
    int command_fds_[kThreadGroupMembers] = {-1, -1, -1};
};

void ExpectEventStop(int status, int signal) {
    ASSERT_TRUE(WIFSTOPPED(status));
    EXPECT_EQ(signal, WSTOPSIG(status));
    EXPECT_EQ(kPtraceEventStop, status >> 16);
}

void DetachSeizedRunning(pid_t tid) {
    ASSERT_EQ(0, ptrace_call(PTRACE_INTERRUPT, tid, 0, 0));
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(tid, &status, kWall));
    ExpectEventStop(status, SIGTRAP);
    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, tid, 0, 0));
}

volatile sig_atomic_t g_sigchld_handler_calls = 0;

void CountSigchld(int) {
    ++g_sigchld_handler_calls;
}

class ScopedBlockedSigchldHandler {
public:
    bool Install() {
        struct sigaction action = {};
        action.sa_handler = CountSigchld;
        action.sa_flags = SA_NOCLDSTOP;
        sigemptyset(&action.sa_mask);
        if (sigaction(SIGCHLD, &action, &old_action_) != 0) return false;
        action_installed_ = true;

        sigset_t blocked = {};
        sigemptyset(&blocked);
        sigaddset(&blocked, SIGCHLD);
        if (sigprocmask(SIG_BLOCK, &blocked, &old_mask_) != 0) return false;
        mask_installed_ = true;
        return true;
    }

    ~ScopedBlockedSigchldHandler() {
        // Restore the action before unblocking so an intentionally pending exit
        // notification cannot be delivered to this test's stop-only handler.
        if (action_installed_) sigaction(SIGCHLD, &old_action_, nullptr);
        if (mask_installed_) sigprocmask(SIG_SETMASK, &old_mask_, nullptr);
    }

private:
    struct sigaction old_action_ = {};
    sigset_t old_mask_ = {};
    bool action_installed_ = false;
    bool mask_installed_ = false;
};

void ExitOnSigusr2(int signal, siginfo_t* info, void*) {
    _exit(signal == SIGUSR2 && info != nullptr && info->si_signo == SIGUSR2 &&
                  info->si_code == SI_TKILL
              ? 0
              : 61);
}

TEST(PtraceStopState, SaNocldstopSuppressesTrapSignalButWaitStillObservesStop) {
    g_sigchld_handler_calls = 0;
    ScopedBlockedSigchldHandler signal_guard;
    ASSERT_TRUE(signal_guard.Install());

    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(0 /* PTRACE_TRACEME */, 0, 0, 0) != 0) _exit(50);
        raise(SIGSTOP);
        _exit(0);
    }
    ChildGuard child_guard(child);

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));

    sigset_t pending = {};
    ASSERT_EQ(0, sigpending(&pending));
    EXPECT_EQ(0, sigismember(&pending, SIGCHLD));
    EXPECT_EQ(0, g_sigchld_handler_calls);

    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    child_guard.Release();
}

TEST(PtraceStopState, UnsupportedSuspendSeccompUsesEinvalAtBothEntrypoints) {
    int ready[2] = {};
    ASSERT_EQ(0, pipe(ready));
    const pid_t seize_child = fork();
    ASSERT_GE(seize_child, 0);
    if (seize_child == 0) {
        close(ready[0]);
        const char marker = 'R';
        if (write(ready[1], &marker, 1) != 1) _exit(51);
        for (;;) pause();
    }
    ChildGuard seize_guard(seize_child);
    close(ready[1]);
    char marker = 0;
    ASSERT_TRUE(ReadByteUntil(ready[0], &marker));
    ASSERT_EQ('R', marker);

    errno = 0;
    EXPECT_EQ(-1, ptrace_call(PTRACE_SEIZE, seize_child, 0,
                              PTRACE_O_SUSPEND_SECCOMP));
    EXPECT_EQ(EINVAL, errno);
    close(ready[0]);

    const pid_t options_child = fork();
    ASSERT_GE(options_child, 0);
    if (options_child == 0) {
        if (ptrace_call(0 /* PTRACE_TRACEME */, 0, 0, 0) != 0) _exit(52);
        raise(SIGSTOP);
        _exit(0);
    }
    ChildGuard options_guard(options_child);
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(options_child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));

    errno = 0;
    EXPECT_EQ(-1, ptrace_call(PTRACE_SETOPTIONS, options_child, 0,
                              PTRACE_O_SUSPEND_SECCOMP));
    EXPECT_EQ(EINVAL, errno);

    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, options_child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(options_child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    options_guard.Release();
}

TEST(PtraceStopState, GroupStopStopsMixedLiveSiblingsAndReportsEachTracee) {
    ControlledThreadGroup group;
    ASSERT_TRUE(group.Start()) << strerror(errno);
    ASSERT_TRUE(group.WaitUntilResumed());

    const pid_t first = group.worker(0);
    const pid_t second = group.worker(1);
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, first, 0, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, second, 0, 0));

    // Targeting a known seized TID makes the signal-delivery-stop explicit;
    // reinjection then starts the process-wide job-control stop.
    ASSERT_EQ(0, syscall(SYS_tgkill, group.leader(), first, SIGSTOP));
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(first, &status, kWall));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, status >> 16);
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, first, 0, SIGSTOP));

    ASSERT_TRUE(WaitPidUntil(first, &status, kWall));
    ExpectEventStop(status, SIGSTOP);
    ASSERT_TRUE(WaitPidUntil(second, &status, kWall));
    ExpectEventStop(status, SIGSTOP);

    // The natural parent report is published only after every live member has
    // participated. The leader is untraced; both workers are ptrace-stopped.
    ASSERT_TRUE(WaitPidUntil(group.leader(), &status, WUNTRACED));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_TRUE(group.QueueAllExecutionProbes());
    EXPECT_TRUE(
        group.NoExecutionProbeCompletes(std::chrono::milliseconds(100)));

    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, first, 0, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, second, 0, 0));
    ASSERT_EQ(0, kill(group.leader(), SIGCONT));
    EXPECT_TRUE(group.WaitForAllExecutionProbes());
}

TEST(PtraceStopState, SigcontCancelsIncompleteGroupStopWithoutLateStop) {
    ControlledThreadGroup group;
    ASSERT_TRUE(group.Start()) << strerror(errno);
    ASSERT_TRUE(group.WaitUntilResumed());

    const pid_t participant = group.worker(0);
    const pid_t held = group.worker(1);
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, participant, 0, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, held, 0, 0));

    // Keep one required participant in an earlier INTERRUPT stop so that the
    // new group-stop cannot reach group_stop_count == 0.
    ASSERT_EQ(0, ptrace_call(PTRACE_INTERRUPT, held, 0, 0));
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(held, &status, kWall));
    ExpectEventStop(status, SIGTRAP);

    ASSERT_EQ(0,
              syscall(SYS_tgkill, group.leader(), participant, SIGSTOP));
    ASSERT_TRUE(WaitPidUntil(participant, &status, kWall));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, status >> 16);
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, participant, 0, SIGSTOP));
    ASSERT_TRUE(WaitPidUntil(participant, &status, kWall));
    ExpectEventStop(status, SIGSTOP);

    errno = 0;
    ASSERT_EQ(0,
              waitpid(group.leader(), &status, WUNTRACED | WNOHANG));

    ASSERT_EQ(0, kill(group.leader(), SIGCONT));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, participant, 0, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, held, 0, 0));

    // Linux notifies seized tasks that SIGCONT changed job-control state.
    // These are fresh SIGTRAP notification events; neither may be a stale
    // SIGSTOP event from the cancelled group-stop generation.
    ASSERT_TRUE(WaitPidUntil(participant, &status, kWall));
    ExpectEventStop(status, SIGTRAP);
    ASSERT_TRUE(WaitPidUntil(held, &status, kWall));
    ExpectEventStop(status, SIGTRAP);
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, participant, 0, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, held, 0, 0));
    EXPECT_TRUE(group.WaitUntilResumed());

    EXPECT_EQ(0, waitpid(participant, &status, kWall | WNOHANG));
    EXPECT_EQ(0, waitpid(held, &status, kWall | WNOHANG));

    DetachSeizedRunning(participant);
    DetachSeizedRunning(held);
}

TEST(PtraceStopState, DetachPendingParticipantCompletesAsUntracedStop) {
    ControlledThreadGroup group;
    ASSERT_TRUE(group.Start()) << strerror(errno);
    ASSERT_TRUE(group.WaitUntilResumed());

    const pid_t participant = group.worker(0);
    const pid_t pending = group.worker(1);
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, participant, 0, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, pending, 0, 0));

    ASSERT_EQ(0, ptrace_call(PTRACE_INTERRUPT, pending, 0, 0));
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(pending, &status, kWall));
    ExpectEventStop(status, SIGTRAP);

    ASSERT_EQ(0,
              syscall(SYS_tgkill, group.leader(), participant, SIGSTOP));
    ASSERT_TRUE(WaitPidUntil(participant, &status, kWall));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, participant, 0, SIGSTOP));
    ASSERT_TRUE(WaitPidUntil(participant, &status, kWall));
    ExpectEventStop(status, SIGSTOP);
    ASSERT_EQ(0,
              waitpid(group.leader(), &status, WUNTRACED | WNOHANG));

    // Unlinking the last outstanding participant must not let it run through
    // an active group-stop. It completes the transaction as an untraced task.
    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, pending, 0, 0));
    ASSERT_TRUE(WaitPidUntil(group.leader(), &status, WUNTRACED));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_TRUE(group.QueueExecutionProbe(2));
    EXPECT_TRUE(
        group.NoExecutionProbeCompletes(std::chrono::milliseconds(100)));

    ASSERT_EQ(0, kill(group.leader(), SIGCONT));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, participant, 0, 0));
    ASSERT_TRUE(WaitPidUntil(participant, &status, kWall));
    ExpectEventStop(status, SIGTRAP);
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, participant, 0, 0));
    EXPECT_TRUE(group.WaitForExecutionProbe(2));
    EXPECT_TRUE(group.WaitUntilResumed());

    DetachSeizedRunning(participant);
}

void VerifyCloneThreadJoinsCompletedGroupStop(bool seized) {
    int ready[2] = {-1, -1};
    int command[2] = {-1, -1};
    int ran[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(command));
    ASSERT_EQ(0, pipe(ran));

    const pid_t leader = fork();
    ASSERT_GE(leader, 0);
    if (leader == 0) {
        close(ready[0]);
        close(command[1]);
        close(ran[0]);
        LateCloneArgs args = {
            .ready_fd = ready[1],
            .command_fd = command[0],
            .ran_fd = ran[1],
        };
        pthread_t creator = {};
        if (pthread_create(&creator, nullptr, LateCloneCreator, &args) != 0)
            _exit(99);
        for (;;) pause();
    }
    ThreadGroupGuard guard(leader);
    close(ready[1]);
    close(command[0]);
    close(ran[1]);

    pid_t creator = -1;
    ASSERT_TRUE(ReadExactUntil(ready[0], &creator, sizeof(creator)));
    guard.SetCreator(creator);
    close(ready[0]);

    int status = 0;
    if (seized) {
        ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, creator, 0, PTRACE_O_TRACECLONE));
    } else {
        ASSERT_EQ(0, ptrace_call(PTRACE_ATTACH, creator, 0, 0));
        ASSERT_TRUE(WaitPidUntil(creator, &status, kWall));
        ASSERT_TRUE(WIFSTOPPED(status));
        ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
        ASSERT_EQ(0,
                  ptrace_call(PTRACE_SETOPTIONS, creator, 0, PTRACE_O_TRACECLONE));
        ASSERT_EQ(0, ptrace_call(PTRACE_CONT, creator, 0, 0));
    }
    ASSERT_EQ(0, syscall(SYS_tgkill, leader, creator, SIGSTOP));
    ASSERT_TRUE(WaitPidUntil(creator, &status, kWall));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, status >> 16);
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, creator, 0, SIGSTOP));
    ASSERT_TRUE(WaitPidUntil(creator, &status, kWall));
    if (seized) {
        ExpectEventStop(status, SIGSTOP);
    } else {
        ASSERT_TRUE(WIFSTOPPED(status));
        ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
        ASSERT_EQ(0, status >> 16);
    }
    ASSERT_TRUE(WaitPidUntil(leader, &status, WUNTRACED));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));

    // Let exactly the traced creator run inside the completed group-stop. Its
    // CLONE_THREAD child must join that stop before executing user code.
    const char clone_command = 'C';
    ASSERT_TRUE(WriteExact(command[1], &clone_command, 1));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, creator, 0, 0));
    ASSERT_TRUE(WaitPidUntil(creator, &status, kWall));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP, WSTOPSIG(status));
    ASSERT_EQ(PTRACE_EVENT_CLONE, status >> 16);

    unsigned long late_message = 0;
    ASSERT_EQ(0, ptrace_call(PTRACE_GETEVENTMSG, creator, 0,
                             reinterpret_cast<unsigned long>(&late_message)));
    const pid_t late = static_cast<pid_t>(late_message);
    ASSERT_GT(late, 0);
    guard.SetLate(late);

    // The clone event is the publication barrier for the new TID. Racing an
    // INTERRUPT before its first wait must coalesce with, not replace, the
    // already-required completed-group stop.
    if (seized) {
        ASSERT_EQ(0, ptrace_call(PTRACE_INTERRUPT, late, 0, 0));
    }
    ASSERT_TRUE(WaitPidUntil(late, &status, kWall));
    if (seized) {
        ExpectEventStop(status, SIGSTOP);
    } else {
        ASSERT_TRUE(WIFSTOPPED(status));
        ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
        ASSERT_EQ(0, status >> 16);
    }
    char marker = 0;
    pollfd ran_poll = {.fd = ran[0], .events = POLLIN, .revents = 0};
    EXPECT_EQ(0, poll(&ran_poll, 1, 0));

    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, late, 0, 0));
    ASSERT_EQ(0, ptrace_call(PTRACE_DETACH, creator, 0, 0));
    EXPECT_TRUE(RemainsUnreadableUntil(ran[0], std::chrono::milliseconds(100)));
    ASSERT_EQ(0, kill(leader, SIGCONT));
    ASSERT_TRUE(ReadByteUntil(ran[0], &marker));
    EXPECT_EQ('R', marker);

    close(command[1]);
    close(ran[0]);
}

TEST(PtraceStopState, SeizedCloneThreadJoinsCompletedGroupStopBeforeRunning) {
    VerifyCloneThreadJoinsCompletedGroupStop(true);
}

TEST(PtraceStopState, AttachedCloneThreadJoinsCompletedGroupStopBeforeRunning) {
    VerifyCloneThreadJoinsCompletedGroupStop(false);
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

TEST(PtraceStopState, SetsiginfoPreservesSigsysPayload) {
#if !defined(__x86_64__)
    GTEST_SKIP() << "seccomp audit architecture is x86_64-specific";
#else
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(0 /* PTRACE_TRACEME */, 0, 0, 0) != 0) {
            _exit(64);
        }
        raise(SIGSTOP);
        raise(SIGSYS);
        _exit(0);
    }
    ChildGuard guard(child);

    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));

    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSYS, WSTOPSIG(status));

    siginfo_t replacement = {};
    replacement.si_signo = SIGSYS;
    replacement.si_errno = 0x1234;
    replacement.si_code = SYS_SECCOMP;
    replacement.si_call_addr = reinterpret_cast<void*>(0x12345678UL);
    replacement.si_syscall = SYS_getpid;
    replacement.si_arch = AUDIT_ARCH_X86_64;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&replacement)));

    siginfo_t observed = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&observed)));
    EXPECT_EQ(SIGSYS, observed.si_signo);
    EXPECT_EQ(0x1234, observed.si_errno);
    EXPECT_EQ(SYS_SECCOMP, observed.si_code);
    EXPECT_EQ(replacement.si_call_addr, observed.si_call_addr);
    EXPECT_EQ(SYS_getpid, observed.si_syscall);
    EXPECT_EQ(AUDIT_ARCH_X86_64, observed.si_arch);

    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    guard.Release();
#endif
}

TEST(PtraceStopState, SetsiginfoPreservesAllLinuxFaultUnionLayouts) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(0 /* PTRACE_TRACEME */, 0, 0, 0) != 0) _exit(72);
        raise(SIGUSR1);
        _exit(73);
    }
    ChildGuard guard(child);
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));

    auto set_u64 = [](siginfo_t* info, size_t offset, uint64_t value) {
        memcpy(reinterpret_cast<unsigned char*>(info) + offset, &value,
               sizeof(value));
    };
    auto set_u32 = [](siginfo_t* info, size_t offset, uint32_t value) {
        memcpy(reinterpret_cast<unsigned char*>(info) + offset, &value,
               sizeof(value));
    };
    auto get_u64 = [](const siginfo_t* info, size_t offset) {
        uint64_t value = 0;
        memcpy(&value, reinterpret_cast<const unsigned char*>(info) + offset,
               sizeof(value));
        return value;
    };
    auto get_u32 = [](const siginfo_t* info, size_t offset) {
        uint32_t value = 0;
        memcpy(&value, reinterpret_cast<const unsigned char*>(info) + offset,
               sizeof(value));
        return value;
    };
    auto check_roundtrip = [&](siginfo_t input, siginfo_t* output) {
        ASSERT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                                 reinterpret_cast<unsigned long>(&input)));
        memset(output, 0, sizeof(*output));
        ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                                 reinterpret_cast<unsigned long>(output)));
        EXPECT_EQ(input.si_signo, output->si_signo);
        EXPECT_EQ(input.si_code, output->si_code);
    };

    // x86_64 siginfo_t: preamble 16 bytes, si_addr at 16, reason union at 24.
    siginfo_t mce = {};
    mce.si_signo = SIGBUS;
    mce.si_code = 4;  // BUS_MCEERR_AR
    set_u64(&mce, 16, 0x1111222233334444ULL);
    set_u32(&mce, 24, 13);
    siginfo_t output = {};
    check_roundtrip(mce, &output);
    EXPECT_EQ(0x1111222233334444ULL, get_u64(&output, 16));
    EXPECT_EQ(13U, get_u32(&output, 24));

    siginfo_t bounds = {};
    bounds.si_signo = SIGSEGV;
    bounds.si_code = 3;  // SEGV_BNDERR
    set_u64(&bounds, 16, 0x2222333344445555ULL);
    set_u64(&bounds, 32, 0x1000);
    set_u64(&bounds, 40, 0x2000);
    check_roundtrip(bounds, &output);
    EXPECT_EQ(0x2222333344445555ULL, get_u64(&output, 16));
    EXPECT_EQ(0x1000U, get_u64(&output, 32));
    EXPECT_EQ(0x2000U, get_u64(&output, 40));

    siginfo_t pkey = {};
    pkey.si_signo = SIGSEGV;
    pkey.si_code = 4;  // SEGV_PKUERR
    set_u64(&pkey, 16, 0x3333444455556666ULL);
    set_u32(&pkey, 32, 7);
    check_roundtrip(pkey, &output);
    EXPECT_EQ(0x3333444455556666ULL, get_u64(&output, 16));
    EXPECT_EQ(7U, get_u32(&output, 32));

    siginfo_t perf = {};
    perf.si_signo = SIGTRAP;
    perf.si_code = 6;  // TRAP_PERF
    set_u64(&perf, 16, 0x4444555566667777ULL);
    set_u64(&perf, 24, 0xabcddcba11223344ULL);
    set_u32(&perf, 32, 9);
    set_u32(&perf, 36, 3);
    check_roundtrip(perf, &output);
    EXPECT_EQ(0x4444555566667777ULL, get_u64(&output, 16));
    EXPECT_EQ(0xabcddcba11223344ULL, get_u64(&output, 24));
    EXPECT_EQ(9U, get_u32(&output, 32));
    EXPECT_EQ(3U, get_u32(&output, 36));

    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(73, WEXITSTATUS(status));
    guard.Release();
}

TEST(PtraceStopState, SetsiginfoUsesPollFallbackAndRejectsUnknownExpansion) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (ptrace_call(0 /* PTRACE_TRACEME */, 0, 0, 0) != 0) _exit(74);
        raise(SIGUSR1);
        _exit(75);
    }
    ChildGuard guard(child);
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));

    siginfo_t poll_info = {};
    poll_info.si_signo = SIGSYS;
    poll_info.si_code = 3;  // outside NSIGSYS, inside NSIGPOLL
    poll_info.si_band = 0x12345678;
    poll_info.si_fd = 42;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&poll_info)));
    siginfo_t output = {};
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&output)));
    EXPECT_EQ(SIGSYS, output.si_signo);
    EXPECT_EQ(3, output.si_code);
    EXPECT_EQ(0x12345678, output.si_band);
    EXPECT_EQ(42, output.si_fd);

    // Linux accepts the siginfo payload wholesale; signo zero must not be fed
    // into a signal-mask shift while selecting the positive-code layout.
    poll_info.si_signo = 0;
    poll_info.si_code = 1;
    poll_info.si_band = 0x55667788;
    poll_info.si_fd = 24;
    ASSERT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&poll_info)));
    memset(&output, 0, sizeof(output));
    ASSERT_EQ(0, ptrace_call(PTRACE_GETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&output)));
    EXPECT_EQ(0, output.si_signo);
    EXPECT_EQ(1, output.si_code);
    EXPECT_EQ(0x55667788, output.si_band);
    EXPECT_EQ(24, output.si_fd);

    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* mapping = mmap(nullptr, static_cast<size_t>(page_size) * 2,
                         PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1,
                         0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* boundary_info = reinterpret_cast<siginfo_t*>(
        static_cast<unsigned char*>(mapping) + page_size - 48);
    memcpy(boundary_info, &poll_info, 48);
    ASSERT_EQ(0, mprotect(static_cast<unsigned char*>(mapping) + page_size,
                          page_size, PROT_NONE));
    EXPECT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(boundary_info)));
    EXPECT_EQ(0, munmap(mapping, static_cast<size_t>(page_size) * 2));

    siginfo_t unknown = {};
    unknown.si_signo = SIGSYS;
    unknown.si_code = 7;
    reinterpret_cast<unsigned char*>(&unknown)[48] = 1;
    errno = 0;
    EXPECT_EQ(-1, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                              reinterpret_cast<unsigned long>(&unknown)));
    EXPECT_EQ(E2BIG, errno);
    reinterpret_cast<unsigned char*>(&unknown)[48] = 0;
    EXPECT_EQ(0, ptrace_call(PTRACE_SETSIGINFO, child, 0,
                             reinterpret_cast<unsigned long>(&unknown)));

    ASSERT_EQ(0, ptrace_call(PTRACE_CONT, child, 0, 0));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(75, WEXITSTATUS(status));
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

TEST(PtraceStopState, InterruptEscapesBlockingNetlinkReceive) {
    int ready[2] = {};
    int release[2] = {};
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));

    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]);
        close(release[1]);
        const int netlink = socket(AF_NETLINK, SOCK_RAW, NETLINK_ROUTE);
        if (netlink < 0) _exit(80);
        sockaddr_nl address = {};
        address.nl_family = AF_NETLINK;
        if (bind(netlink, reinterpret_cast<sockaddr*>(&address),
                 sizeof(address)) != 0) {
            _exit(81);
        }
        const char marker = 'R';
        if (write(ready[1], &marker, 1) != 1) _exit(82);
        char token = 0;
        if (read(release[0], &token, 1) != 1) _exit(83);
        char buffer[64] = {};
        (void)recv(netlink, buffer, sizeof(buffer), 0);
        _exit(84);
    }
    ChildGuard guard(child);
    close(ready[1]);
    close(release[0]);

    char marker = 0;
    ASSERT_TRUE(ReadByteUntil(ready[0], &marker));
    ASSERT_EQ('R', marker);
    ASSERT_EQ(0, ptrace_call(PTRACE_SEIZE, child, 0, 0));
    const char go = 'G';
    ASSERT_EQ(1, write(release[1], &go, 1));
    ASSERT_TRUE(WaitForSleepingProcess(child))
        << "tracee did not block in netlink recv: errno=" << errno;

    ASSERT_EQ(0, ptrace_call(PTRACE_INTERRUPT, child, 0, 0));
    int status = 0;
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGTRAP | (kPtraceEventStop << 8), status >> 8);

    ASSERT_EQ(0, kill(child, SIGKILL));
    ASSERT_TRUE(WaitPidUntil(child, &status, 0));
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGKILL, WTERMSIG(status));
    guard.Release();
    close(ready[0]);
    close(release[1]);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
