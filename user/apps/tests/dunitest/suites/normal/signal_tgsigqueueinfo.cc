#include <gtest/gtest.h>

#include <atomic>

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/signalfd.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

long Send(pid_t tgid, pid_t tid, int sig, const void* info) {
    return syscall(SYS_rt_tgsigqueueinfo, tgid, tid, sig, info);
}

siginfo_t Queued(int value = 0) {
    siginfo_t info {};
    info.si_code = SI_QUEUE;
    info.si_pid = 1234;
    info.si_uid = 5678;
    info.si_value.sival_int = value;
    return info;
}

class BlockSignal {
public:
    explicit BlockSignal(int sig) : sig_(sig) {
        sigemptyset(&set_);
        sigaddset(&set_, sig);
        active_ = pthread_sigmask(SIG_BLOCK, &set_, &old_) == 0;
    }
    ~BlockSignal() {
        if (active_) {
            siginfo_t info {};
            timespec zero {};
            while (sigtimedwait(&set_, &info, &zero) == sig_) {}
            pthread_sigmask(SIG_SETMASK, &old_, nullptr);
        }
    }
    bool active() const { return active_; }
    int Wait(siginfo_t* info) const {
        timespec timeout {2, 0};
        // libc may normalize SI_TKILL into SI_USER; inspect the kernel ABI.
        return syscall(SYS_rt_sigtimedwait, &set_, info, &timeout, sizeof(uint64_t));
    }
    int Poll(siginfo_t* info) const {
        timespec zero {};
        return syscall(SYS_rt_sigtimedwait, &set_, info, &zero, sizeof(uint64_t));
    }
    const sigset_t* set() const { return &set_; }
private:
    int sig_;
    bool active_;
    sigset_t set_ {}, old_ {};
};

void ExpectError(pid_t tgid, pid_t tid, int sig, const void* info, int expected) {
    errno = 0;
    EXPECT_EQ(-1, Send(tgid, tid, sig, info));
    EXPECT_EQ(expected, errno) << strerror(errno);
}

int WaitChild(pid_t child) {
    int status = 0;
    for (int attempt = 0; attempt < 500; ++attempt) {
        const pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) {
            return WIFEXITED(status) ? WEXITSTATUS(status) : -1;
        }
        if (result == -1 && errno != EINTR) {
            return -1;
        }
        usleep(10000);
    }
    kill(child, SIGKILL);
    while (waitpid(child, &status, 0) == -1 && errno == EINTR) {}
    return -1;
}

bool ReadReady(int fd) {
    pollfd descriptor {fd, POLLIN, 0};
    char ready = 0;
    return poll(&descriptor, 1, 2000) == 1 && (descriptor.revents & POLLIN) &&
           read(fd, &ready, 1) == 1 && ready == 'R';
}

uid_t OverflowUid() {
    unsigned value = 65534;
    FILE* file = fopen("/proc/sys/kernel/overflowuid", "r");
    if (file) {
        if (fscanf(file, "%u", &value) != 1) {
            value = 65534;
        }
        fclose(file);
    }
    return value;
}

} // namespace

TEST(SignalTgsigqueueinfo, InvalidPointerCopiedBeforeTargetValidation) {
    ExpectError(-1, -1, 0, reinterpret_cast<void*>(1), EFAULT);
    ExpectError(getpid(), syscall(SYS_gettid), 0, nullptr, EFAULT);
}

TEST(SignalTgsigqueueinfo, QueuedMetadataAndSyscallSignoPreserved) {
    BlockSignal block(SIGUSR1);
    ASSERT_TRUE(block.active());
    auto info = Queued(0x1357);
    info.si_signo = SIGUSR2;
    info.si_errno = 42;
    ASSERT_EQ(0, Send(getpid(), syscall(SYS_gettid), SIGUSR1, &info));
    siginfo_t received {};
    ASSERT_EQ(SIGUSR1, block.Wait(&received));
    EXPECT_EQ(SIGUSR1, received.si_signo);
    EXPECT_EQ(SI_QUEUE, received.si_code);
    EXPECT_EQ(42, received.si_errno);
    EXPECT_EQ(1234, received.si_pid);
    EXPECT_EQ(5678U, received.si_uid);
    EXPECT_EQ(0x1357, received.si_value.sival_int);
}

TEST(SignalTgsigqueueinfo, NonPositiveIdsRejected) {
    auto info = Queued();
    ExpectError(0, syscall(SYS_gettid), 0, &info, EINVAL);
    ExpectError(-1, syscall(SYS_gettid), 0, &info, EINVAL);
    ExpectError(getpid(), 0, 0, &info, EINVAL);
    ExpectError(getpid(), -1, 0, &info, EINVAL);
}

TEST(SignalTgsigqueueinfo, ForgedCodeRejectedBeforeMissingTarget) {
    siginfo_t info {};
    for (int code : {SI_USER, SI_KERNEL, SI_TKILL}) {
        info.si_code = code;
        ExpectError(getpid(), 0x3fffffff, 0, &info, EPERM);
    }
}

TEST(SignalTgsigqueueinfo, MissingTargetAndWrongGroupPrecedeInvalidSignal) {
    auto info = Queued();
    ExpectError(getpid(), 0x3fffffff, -1, &info, ESRCH);
    ExpectError(getpid() + 1, syscall(SYS_gettid), -1, &info, ESRCH);
    ExpectError(getpid(), syscall(SYS_gettid), -1, &info, EINVAL);
    ExpectError(getpid(), syscall(SYS_gettid), 65, &info, EINVAL);
}

TEST(SignalTgsigqueueinfo, SelfAllowsPositiveAndTkillCodes) {
    BlockSignal block(SIGUSR1);
    ASSERT_TRUE(block.active());
    for (int code : {SI_USER, SI_KERNEL, SI_TKILL}) {
        auto info = Queued(73);
        info.si_code = code;
        ASSERT_EQ(0, Send(getpid(), syscall(SYS_gettid), SIGUSR1, &info));
        siginfo_t received {};
        ASSERT_EQ(SIGUSR1, block.Wait(&received));
        EXPECT_EQ(code, received.si_code);
    }
}

TEST(SignalTgsigqueueinfo, SignalZeroDoesNotEnqueue) {
    BlockSignal block(SIGUSR1);
    ASSERT_TRUE(block.active());
    auto info = Queued(99);
    info.si_signo = SIGUSR1;
    ASSERT_EQ(0, Send(getpid(), syscall(SYS_gettid), 0, &info));
    siginfo_t received {};
    errno = 0;
    EXPECT_EQ(-1, block.Poll(&received));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(SignalTgsigqueueinfo, ProtectedAndCrossPageUserCopy) {
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    void* mapping = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* boundary = static_cast<unsigned char*>(mapping) + page;
    auto info = Queued(0x2468);
    memcpy(boundary - 48, &info, 48);
    ASSERT_EQ(0, mprotect(boundary, page, PROT_NONE));
    BlockSignal block(SIGUSR1);
    ASSERT_TRUE(block.active());
    ExpectError(getpid(), syscall(SYS_gettid), 0, boundary, EFAULT);
    ExpectError(-1, -1, 0, boundary - 47, EFAULT);
    EXPECT_EQ(0, Send(getpid(), syscall(SYS_gettid), SIGUSR1, boundary - 48));
    siginfo_t received {};
    ASSERT_EQ(SIGUSR1, block.Wait(&received));
    EXPECT_EQ(0x2468, received.si_value.sival_int);
    EXPECT_EQ(0, munmap(mapping, page * 2));
}

TEST(SignalTgsigqueueinfo, UnknownExpansionRequiresReadableZeroTail) {
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    void* mapping = mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* boundary = static_cast<unsigned char*>(mapping) + page;
    siginfo_t info {};
    info.si_code = 7; // Unknown SIGUSR1 layout: expansion bytes must be zero.
    memcpy(boundary - 48, &info, sizeof(info));
    ASSERT_EQ(0, mprotect(boundary, page, PROT_NONE));
    ExpectError(-1, -1, SIGUSR1, boundary - 48, EFAULT);
    ASSERT_EQ(0, mprotect(boundary, page, PROT_READ | PROT_WRITE));
    boundary[0] = 1;
    ExpectError(-1, -1, SIGUSR1, boundary - 48, E2BIG);
    boundary[0] = 0;
    ExpectError(-1, -1, SIGUSR1, boundary - 48, EINVAL);
    EXPECT_EQ(0, munmap(mapping, page * 2));
}

TEST(SignalTgsigqueueinfo, UnknownLayoutPreservesOpaqueKernelPrefix) {
    BlockSignal block(SIGUSR1);
    ASSERT_TRUE(block.active());
    for (int code : {7, -99}) {
        siginfo_t info {};
        info.si_signo = SIGUSR2;
        info.si_errno = 41;
        info.si_code = code;
        auto* bytes = reinterpret_cast<unsigned char*>(&info);
        for (size_t offset = 12; offset < 48; ++offset) {
            bytes[offset] = static_cast<unsigned char>(offset + 19);
        }
        // The ABI accepts unknown layouts only when the 80-byte expansion is zero.
        ASSERT_EQ(0, Send(getpid(), syscall(SYS_gettid), SIGUSR1, &info));
        siginfo_t received {};
        ASSERT_EQ(SIGUSR1, block.Wait(&received));
        info.si_signo = SIGUSR1;
        EXPECT_EQ(0, memcmp(&info, &received, 48)) << "si_code=" << code;
        siginfo_t zero {};
        EXPECT_EQ(0, memcmp(reinterpret_cast<unsigned char*>(&received) + 48,
                            reinterpret_cast<unsigned char*>(&zero) + 48, 80));
    }
}

TEST(SignalTgsigqueueinfo, RealtimeSignalsPreserveFifo) {
    const int sig = SIGRTMIN;
    BlockSignal block(sig);
    ASSERT_TRUE(block.active());
    for (int value = 1; value <= 4; ++value) {
        auto info = Queued(value);
        ASSERT_EQ(0, Send(getpid(), syscall(SYS_gettid), sig, &info));
    }
    for (int value = 1; value <= 4; ++value) {
        siginfo_t received {};
        ASSERT_EQ(sig, block.Wait(&received));
        EXPECT_EQ(value, received.si_value.sival_int);
    }
    siginfo_t received {};
    EXPECT_EQ(-1, block.Poll(&received));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(SignalTgsigqueueinfo, StandardSignalsCoalesceAndKeepFirstInfo) {
    BlockSignal block(SIGUSR1);
    ASSERT_TRUE(block.active());
    for (int value : {11, 22}) {
        auto info = Queued(value);
        ASSERT_EQ(0, Send(getpid(), syscall(SYS_gettid), SIGUSR1, &info));
    }
    siginfo_t received {};
    ASSERT_EQ(SIGUSR1, block.Wait(&received));
    EXPECT_EQ(11, received.si_value.sival_int);
    EXPECT_EQ(-1, block.Poll(&received));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(SignalTgsigqueueinfo, SignalfdReceivesThreadQueuedInfo) {
    BlockSignal block(SIGUSR1);
    ASSERT_TRUE(block.active());
    const int fd = signalfd(-1, block.set(), SFD_CLOEXEC | SFD_NONBLOCK);
    ASSERT_GE(fd, 0) << strerror(errno);
    auto info = Queued(0x579);
    info.si_errno = 31;
    EXPECT_EQ(0, Send(getpid(), syscall(SYS_gettid), SIGUSR1, &info));
    signalfd_siginfo received {};
    EXPECT_EQ(static_cast<ssize_t>(sizeof(received)), read(fd, &received, sizeof(received)));
    EXPECT_EQ(static_cast<unsigned>(SIGUSR1), received.ssi_signo);
    EXPECT_EQ(SI_QUEUE, received.ssi_code);
    EXPECT_EQ(31, received.ssi_errno);
    EXPECT_EQ(1234U, received.ssi_pid);
    EXPECT_EQ(5678U, received.ssi_uid);
    EXPECT_EQ(0x579, received.ssi_int);
    EXPECT_EQ(-1, read(fd, &received, sizeof(received)));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, close(fd));
}

TEST(SignalTgsigqueueinfo, SignalfdDecodesUnknownLayoutFallbacks) {
    BlockSignal block(SIGSYS);
    ASSERT_TRUE(block.active());
    const int fd = signalfd(-1, block.set(), SFD_CLOEXEC | SFD_NONBLOCK);
    ASSERT_GE(fd, 0) << strerror(errno);
    for (int code : {3, 7, -99}) {
        siginfo_t info {};
        info.si_code = code;
        info.si_errno = 27;
        if (code == 3) {
            // Beyond NSIGSYS, but within NSIGPOLL: Linux chooses SIL_POLL.
            info.si_band = 0x345;
            info.si_fd = 37;
        } else {
            info.si_pid = 1234;
            info.si_uid = 5678;
            info.si_value.sival_int = 0x2468;
        }
        EXPECT_EQ(0, Send(getpid(), syscall(SYS_gettid), SIGSYS, &info));
        signalfd_siginfo received {};
        EXPECT_EQ(static_cast<ssize_t>(sizeof(received)), read(fd, &received, sizeof(received)));
        EXPECT_EQ(static_cast<unsigned>(SIGSYS), received.ssi_signo);
        EXPECT_EQ(code, received.ssi_code);
        EXPECT_EQ(27, received.ssi_errno);
        if (code == 3) {
            EXPECT_EQ(0x345U, received.ssi_band);
            EXPECT_EQ(37, received.ssi_fd);
        } else {
            EXPECT_EQ(1234U, received.ssi_pid);
            EXPECT_EQ(5678U, received.ssi_uid);
            if (code < 0) {
                EXPECT_EQ(0x2468, received.ssi_int);
                EXPECT_EQ(0x2468U, received.ssi_ptr);
            } else {
                // Positive unknown code selects SIL_KILL, not SIL_RT.
                EXPECT_EQ(0, received.ssi_int);
                EXPECT_EQ(0U, received.ssi_ptr);
            }
        }
    }
    signalfd_siginfo received {};
    EXPECT_EQ(-1, read(fd, &received, sizeof(received)));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0, close(fd));
}

namespace {
struct ThreadWait {
    int ready_fd;
    pid_t tid = 0;
    int received_signal = 0;
    siginfo_t received {};
};

void* WaitInSibling(void* arg) {
    auto* state = static_cast<ThreadWait*>(arg);
    state->tid = syscall(SYS_gettid);
    // The parent blocks SIGUSR1 before pthread_create, so the worker inherits it.
    if (write(state->ready_fd, &state->tid, sizeof(state->tid)) != sizeof(state->tid)) {
        return nullptr;
    }
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGUSR1);
    timespec timeout {2, 0};
    state->received_signal = sigtimedwait(&set, &state->received, &timeout);
    return nullptr;
}
} // namespace

TEST(SignalTgsigqueueinfo, SiblingCannotForgeButReceivesItsOwnPrivateQueue) {
    BlockSignal block(SIGUSR1);
    ASSERT_TRUE(block.active());
    int ready[2];
    ASSERT_EQ(0, pipe(ready));
    ThreadWait state {ready[1]};
    pthread_t worker;
    ASSERT_EQ(0, pthread_create(&worker, nullptr, WaitInSibling, &state));
    pid_t tid = 0;
    pollfd ready_poll {ready[0], POLLIN, 0};
    if (poll(&ready_poll, 1, 2000) == 1 && (ready_poll.revents & POLLIN)) {
        EXPECT_EQ(static_cast<ssize_t>(sizeof(tid)), read(ready[0], &tid, sizeof(tid)));
    } else {
        ADD_FAILURE() << "worker readiness deadline expired";
    }
    for (int code : {SI_USER, SI_KERNEL, SI_TKILL}) {
        auto info = Queued();
        info.si_code = code;
        ExpectError(getpid(), tid, SIGUSR1, &info, EPERM);
    }
    auto info = Queued(0x789);
    EXPECT_EQ(0, Send(getpid(), tid, SIGUSR1, &info));
    EXPECT_EQ(0, pthread_join(worker, nullptr));
    EXPECT_EQ(SIGUSR1, state.received_signal);
    EXPECT_EQ(SI_QUEUE, state.received.si_code);
    EXPECT_EQ(0x789, state.received.si_value.sival_int);
    siginfo_t received {};
    EXPECT_EQ(-1, block.Poll(&received));
    EXPECT_EQ(EAGAIN, errno);
    close(ready[0]);
    close(ready[1]);
}

TEST(SignalTgsigqueueinfo, SignalZeroRequiresCredentialPermission) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires a privileged parent to establish distinct UIDs";
    }
    const pid_t parent = getpid();
    const pid_t tid = syscall(SYS_gettid);
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(4);
        if (setresuid(65534, 65534, 65534) != 0) {
            _exit(77);
        }
        auto info = Queued();
        errno = 0;
        _exit(Send(parent, tid, 0, &info) == -1 && errno == EPERM ? 0 : 1);
    }
    const int status = WaitChild(child);
    if (status == 77) {
        GTEST_SKIP() << "setresuid not permitted in this environment";
    }
    EXPECT_EQ(0, status);
}

TEST(SignalTgsigqueueinfo, PidNamespaceUsesCallerVisibleTid) {
    const pid_t supervisor = fork();
    ASSERT_GE(supervisor, 0);
    if (supervisor == 0) {
        alarm(4);
        if (unshare(CLONE_NEWUSER | CLONE_NEWPID) != 0) {
            _exit(77);
        }
        const pid_t init = fork();
        if (init < 0) {
            _exit(2);
        }
        if (init == 0) {
            alarm(3);
            if (getpid() != 1 || syscall(SYS_gettid) != 1) {
                _exit(3);
            }
            BlockSignal block(SIGUSR1);
            if (!block.active()) {
                _exit(4);
            }
            auto info = Queued(0x345);
            // Metadata is supplied by the caller, not translated or synthesized.
            info.si_pid = 9876;
            if (Send(1, 1, SIGUSR1, &info) != 0) {
                _exit(5);
            }
            siginfo_t received {};
            if (block.Wait(&received) != SIGUSR1 || received.si_code != SI_QUEUE ||
                received.si_pid != 9876 || received.si_value.sival_int != 0x345) {
                _exit(6);
            }
            errno = 0;
            _exit(Send(2, 1, 0, &info) == -1 && errno == ESRCH ? 0 : 7);
        }
        _exit(WaitChild(init) == 0 ? 0 : 8);
    }
    const int status = WaitChild(supervisor);
    if (status == 77) {
        GTEST_SKIP() << "user/PID namespaces unavailable in this environment";
    }
    EXPECT_EQ(0, status);
}

TEST(SignalTgsigqueueinfo, PositiveChildLayoutPreservesMetadata) {
    BlockSignal block(SIGCHLD);
    ASSERT_TRUE(block.active());
    siginfo_t info {};
    info.si_signo = SIGUSR1;
    info.si_errno = 29;
    info.si_code = CLD_EXITED;
    info.si_pid = 1234;
    info.si_uid = 5678;
    info.si_status = 19;
    info.si_utime = 111;
    info.si_stime = 222;
    ASSERT_EQ(0, Send(getpid(), syscall(SYS_gettid), SIGCHLD, &info));
    siginfo_t received {};
    ASSERT_EQ(SIGCHLD, block.Wait(&received));
    EXPECT_EQ(SIGCHLD, received.si_signo);
    EXPECT_EQ(CLD_EXITED, received.si_code);
    EXPECT_EQ(29, received.si_errno);
    EXPECT_EQ(1234, received.si_pid);
    EXPECT_EQ(5678U, received.si_uid);
    EXPECT_EQ(19, received.si_status);
    EXPECT_EQ(111, received.si_utime);
    EXPECT_EQ(222, received.si_stime);
}

TEST(SignalTgsigqueueinfo, QueuedUidRemappedIntoTargetUserNamespace) {
    const uid_t overflow = OverflowUid();
    int ready[2];
    ASSERT_EQ(0, pipe(ready));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(4);
        close(ready[0]);
        if (unshare(CLONE_NEWUSER) != 0) {
            _exit(77);
        }
        BlockSignal block(SIGUSR1);
        if (!block.active() || write(ready[1], "R", 1) != 1) {
            _exit(2);
        }
        siginfo_t received {};
        _exit(block.Wait(&received) == SIGUSR1 && received.si_code == SI_QUEUE &&
                      received.si_pid == 1234 && received.si_uid == overflow &&
                      received.si_value.sival_int == 0xabc
                  ? 0 : 3);
    }
    close(ready[1]);
    const bool is_ready = ReadReady(ready[0]);
    close(ready[0]);
    if (is_ready) {
        auto info = Queued(0xabc);
        EXPECT_EQ(0, Send(child, child, SIGUSR1, &info));
    }
    const int status = WaitChild(child);
    if (status == 77) {
        GTEST_SKIP() << "user namespaces unavailable in this environment";
    }
    EXPECT_TRUE(is_ready);
    EXPECT_EQ(0, status);
}

TEST(SignalTgsigqueueinfo, ChildUserNamespaceCapabilityDoesNotAuthorizeAncestor) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires privilege to establish a different target UID";
    }
    BlockSignal pipe_signal(SIGPIPE);
    ASSERT_TRUE(pipe_signal.active());
    int ready[2];
    int release[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(release));
    const pid_t target = fork();
    ASSERT_GE(target, 0);
    if (target == 0) {
        alarm(4);
        close(ready[0]);
        close(release[1]);
        if (setresuid(65534, 65534, 65534) != 0) {
            _exit(77);
        }
        if (write(ready[1], "R", 1) != 1) {
            _exit(2);
        }
        char command;
        _exit(read(release[0], &command, 1) == 1 ? 0 : 3);
    }
    close(ready[1]);
    close(release[0]);
    const bool is_ready = ReadReady(ready[0]);
    close(ready[0]);
    int sender_status = -1;
    if (is_ready) {
        const pid_t sender = fork();
        if (sender == 0) {
            alarm(3);
            if (unshare(CLONE_NEWUSER) != 0) {
                _exit(77);
            }
            auto info = Queued();
            // Sender owns capabilities only in its new user namespace; target
            // remains in the ancestor namespace and has a different global UID.
            errno = 0;
            _exit(Send(target, target, 0, &info) == -1 && errno == EPERM ? 0 : 1);
        }
        if (sender > 0) {
            sender_status = WaitChild(sender);
        } else {
            ADD_FAILURE() << "fork sender: " << strerror(errno);
        }
        EXPECT_EQ(1, write(release[1], "X", 1));
    }
    close(release[1]);
    const int target_status = WaitChild(target);
    if (target_status == 77 || sender_status == 77) {
        GTEST_SKIP() << "UID transition or user namespace unavailable";
    }
    EXPECT_TRUE(is_ready);
    EXPECT_EQ(0, sender_status);
    EXPECT_EQ(0, target_status);
}

namespace {
struct RealtimeWait {
    int ready_fd;
    std::atomic<pid_t> tid {0};
    int consumed = 0;
    int unexpected_value = 0;
    int wait_result = 0;
    int wait_errno = 0;
    int final_result = 0;
    int final_errno = 0;
};

void* ConsumeRealtime(void* arg) {
    auto* state = static_cast<RealtimeWait*>(arg);
    state->tid.store(syscall(SYS_gettid), std::memory_order_relaxed);
    if (write(state->ready_fd, "R", 1) != 1) {
        return nullptr;
    }
    sigset_t set;
    sigemptyset(&set);
    sigaddset(&set, SIGRTMIN);
    for (int value = 1; value <= 300; ++value) {
        siginfo_t info {};
        timespec timeout {2, 0};
        errno = 0;
        const int result = syscall(SYS_rt_sigtimedwait, &set, &info, &timeout, sizeof(uint64_t));
        if (result != SIGRTMIN) {
            state->wait_result = result;
            state->wait_errno = errno;
            return nullptr;
        }
        ++state->consumed;
        if (info.si_code != SI_QUEUE || info.si_value.sival_int != value) {
            state->unexpected_value = value;
            return nullptr;
        }
    }
    siginfo_t info {};
    timespec zero {};
    errno = 0;
    state->final_result = syscall(SYS_rt_sigtimedwait, &set, &info, &zero, sizeof(uint64_t));
    state->final_errno = errno;
    return nullptr;
}
} // namespace

TEST(SignalTgsigqueueinfo, ConcurrentRealtimeConsumptionLeavesNoPhantomPendingBit) {
    BlockSignal block(SIGRTMIN);
    ASSERT_TRUE(block.active());
    int ready[2];
    ASSERT_EQ(0, pipe(ready));
    RealtimeWait state {ready[1]};
    pthread_t worker;
    ASSERT_EQ(0, pthread_create(&worker, nullptr, ConsumeRealtime, &state));
    const bool is_ready = ReadReady(ready[0]);
    if (is_ready) {
        for (int value = 1; value <= 300; ++value) {
            auto info = Queued(value);
            if (Send(getpid(), state.tid.load(std::memory_order_relaxed), SIGRTMIN, &info) != 0) {
                ADD_FAILURE() << "send " << value << ": " << strerror(errno);
                break;
            }
            // Encourage enqueue/dequeue overlap, not merely a prefilled queue.
            if (value % 7 == 0) {
                sched_yield();
            }
        }
    }
    EXPECT_EQ(0, pthread_join(worker, nullptr));
    close(ready[0]);
    close(ready[1]);
    EXPECT_TRUE(is_ready);
    EXPECT_EQ(300, state.consumed);
    EXPECT_EQ(0, state.unexpected_value);
    EXPECT_EQ(0, state.wait_result) << "worker stopped after " << state.consumed
                                  << " signals; wait errno=" << state.wait_errno;
    EXPECT_EQ(0, state.wait_errno) << strerror(state.wait_errno);
    EXPECT_EQ(-1, state.final_result);
    EXPECT_EQ(EAGAIN, state.final_errno);
    siginfo_t received {};
    EXPECT_EQ(-1, block.Poll(&received));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(SignalTgsigqueueinfo, KernelChildNotificationKeepsGlobalUidProvenance) {
    BlockSignal block(SIGCHLD);
    ASSERT_TRUE(block.active());
    const uid_t original_uid = getuid();
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(3);
        // The new namespace has no UID map, but the kernel credential remains
        // the original global UID. Parent must not receive an overflow UID.
        _exit(unshare(CLONE_NEWUSER) == 0 ? 23 : 77);
    }
    siginfo_t received {};
    const int signal = block.Wait(&received);
    const int status = WaitChild(child);
    if (status == 77) {
        GTEST_SKIP() << "user namespaces unavailable in this environment";
    }
    EXPECT_EQ(23, status);
    EXPECT_EQ(SIGCHLD, signal);
    EXPECT_EQ(CLD_EXITED, received.si_code);
    EXPECT_EQ(child, received.si_pid);
    EXPECT_EQ(original_uid, received.si_uid);
    EXPECT_EQ(23, received.si_status);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
