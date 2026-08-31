#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <climits>
#include <cstddef>
#include <cstdint>
#include <cstring>
#include <pthread.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <unistd.h>

#ifndef SCHED_RESET_ON_FORK
#define SCHED_RESET_ON_FORK 0x40000000
#endif

namespace {

struct alignas(4) RawSchedParam {
    int32_t sched_priority;
};

struct RawSchedParamWithCanary {
    RawSchedParam param;
    uint8_t canary[60];
};

static_assert(sizeof(RawSchedParam) == 4);
static_assert(offsetof(RawSchedParamWithCanary, canary) == 4);

long RawGetParam(pid_t tid, RawSchedParam* param) {
    return syscall(SYS_sched_getparam, tid, param);
}

long RawSetScheduler(pid_t tid, int policy, const RawSchedParam* param) {
    return syscall(SYS_sched_setscheduler, tid, policy, param);
}

bool IsDragonOS() {
    struct utsname info {};
    return uname(&info) == 0 && strstr(info.release, "dragonos") != nullptr;
}

int WaitForChild(pid_t child) {
    int status = 0;
    for (int attempt = 0; attempt < 500; ++attempt) {
        pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) {
            if (!WIFEXITED(status)) return 255;
            return WEXITSTATUS(status);
        }
        if (waited < 0 && errno != EINTR) return 254;
        usleep(10 * 1000);
    }

    kill(child, SIGKILL);
    while (waitpid(child, nullptr, 0) < 0 && errno == EINTR) {
    }
    return 253;
}

class ChildGuard {
public:
    explicit ChildGuard(pid_t child) : child_(child) {}
    ~ChildGuard() {
        if (child_ <= 0) return;
        kill(child_, SIGKILL);
        while (waitpid(child_, nullptr, 0) < 0 && errno == EINTR) {
        }
    }
    void Release() { child_ = -1; }

private:
    pid_t child_;
};

void WriteByteOrExit(int fd, char value) {
    if (write(fd, &value, 1) != 1) _exit(120);
}

bool ReadByte(int fd) {
    char value = 0;
    return read(fd, &value, 1) == 1;
}

bool ReadByteWithTimeout(int fd, int timeout_ms) {
    struct pollfd poll_fd {fd, POLLIN, 0};
    int result;
    do {
        result = poll(&poll_fd, 1, timeout_ms);
    } while (result < 0 && errno == EINTR);
    return result == 1 && (poll_fd.revents & POLLIN) != 0 && ReadByte(fd);
}

TEST(SchedParamAbi, GetParamWritesExactlyFourBytes) {
    RawSchedParamWithCanary value {};
    value.param.sched_priority = -1;
    memset(value.canary, 0xa5, sizeof(value.canary));

    ASSERT_EQ(0, RawGetParam(0, &value.param)) << strerror(errno);
    EXPECT_EQ(0, value.param.sched_priority);
    for (uint8_t byte : value.canary) EXPECT_EQ(0xa5, byte);
}

TEST(SchedParamAbi, SetParamReadsExactlyFourBytes) {
    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* mapping = mmap(nullptr, static_cast<size_t>(page_size) * 2, PROT_READ | PROT_WRITE,
                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    ASSERT_EQ(0, mprotect(static_cast<char*>(mapping) + page_size, page_size, PROT_NONE));

    auto* param = reinterpret_cast<RawSchedParam*>(static_cast<char*>(mapping) + page_size - 4);
    param->sched_priority = 0;

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        _exit(RawSetScheduler(0, SCHED_OTHER | SCHED_RESET_ON_FORK, param) == 0 ? 0 : errno);
    }
    EXPECT_EQ(0, WaitForChild(child));
    EXPECT_EQ(0, munmap(mapping, static_cast<size_t>(page_size) * 2));
}

TEST(SchedParamAbi, LibcWrapperInterop) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        struct sched_param param {};
        param.sched_priority = 0;
        if (sched_setscheduler(0, SCHED_OTHER | SCHED_RESET_ON_FORK, &param) != 0) _exit(10);
        struct sched_param out {};
        memset(&out, 0xa5, sizeof(out));
        if (sched_getparam(0, &out) != 0 || out.sched_priority != 0) _exit(11);
        if (sched_getscheduler(0) != (SCHED_OTHER | SCHED_RESET_ON_FORK)) _exit(12);
        _exit(0);
    }
    EXPECT_EQ(0, WaitForChild(child));
}

TEST(SchedGetParam, CurrentAndErrorsMatchLinux) {
    RawSchedParam param {-1};
    EXPECT_EQ(0, RawGetParam(0, &param));
    EXPECT_EQ(0, param.sched_priority);

    errno = 0;
    EXPECT_EQ(-1, RawGetParam(-1, &param));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, RawGetParam(0, nullptr));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, RawGetParam(INT_MAX, &param));
    EXPECT_EQ(ESRCH, errno);
    errno = 0;
    EXPECT_EQ(-1, RawGetParam(0, reinterpret_cast<RawSchedParam*>(1)));
    EXPECT_EQ(EFAULT, errno);
}

TEST(SchedGetScheduler, CurrentAndErrorsMatchLinux) {
    EXPECT_EQ(SCHED_OTHER, sched_getscheduler(0));
    errno = 0;
    EXPECT_EQ(-1, sched_getscheduler(-1));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, sched_getscheduler(INT_MAX));
    EXPECT_EQ(ESRCH, errno);
}

TEST(SchedSetScheduler, ErrorOrderingAndPolicyMatrix) {
    RawSchedParam zero {0};
    RawSchedParam one {1};
    RawSchedParam negative {-1};
    RawSchedParam maximum {INT_MAX};
    const auto* bad = reinterpret_cast<const RawSchedParam*>(1);

    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(0, -1, bad));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(-1, SCHED_OTHER, bad));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(INT_MAX, SCHED_OTHER, bad));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(INT_MAX, 0x20000000, &zero));
    EXPECT_EQ(ESRCH, errno);

    for (const RawSchedParam* invalid : {&negative, &one, &maximum}) {
        errno = 0;
        EXPECT_EQ(-1, RawSetScheduler(0, SCHED_OTHER, invalid));
        EXPECT_EQ(EINVAL, errno);
    }
    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(0, 0x20000000, &zero));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(0, SCHED_FIFO, &zero));
    EXPECT_EQ(EINVAL, errno);

    // On Linux a valid FIFO request may really enter RT scheduling. Only the
    // DragonOS guest exercises its documented staging boundary.
    if (IsDragonOS()) {
        errno = 0;
        EXPECT_EQ(-1, RawSetScheduler(0, SCHED_FIFO, &one));
        EXPECT_EQ(EOPNOTSUPP, errno);
    }
}

TEST(SchedSetScheduler, SelfResetFlagRoundTripAndFork) {
    pid_t probe = fork();
    ASSERT_GE(probe, 0) << strerror(errno);
    if (probe == 0) {
        RawSchedParam zero {0};
        if (RawSetScheduler(0, SCHED_OTHER | SCHED_RESET_ON_FORK, &zero) != 0) _exit(10);
        if (sched_getscheduler(0) != (SCHED_OTHER | SCHED_RESET_ON_FORK)) _exit(11);
        pid_t child = fork();
        if (child < 0) _exit(12);
        if (child == 0) _exit(sched_getscheduler(0) == SCHED_OTHER ? 0 : 13);
        if (WaitForChild(child) != 0) _exit(14);
        if (sched_getscheduler(0) != (SCHED_OTHER | SCHED_RESET_ON_FORK)) _exit(15);
        _exit(0);
    }
    EXPECT_EQ(0, WaitForChild(probe));
}

struct WorkerState {
    std::atomic<int> tid {0};
    std::atomic<int> proceed {0};
    int result = -1;
};

void* NestedWorker(void* arg) {
    auto* result = static_cast<int*>(arg);
    *result = sched_getscheduler(0) == SCHED_OTHER ? 0 : 1;
    return nullptr;
}

void* FlagWorker(void* arg) {
    auto* state = static_cast<WorkerState*>(arg);
    state->tid.store(static_cast<int>(syscall(SYS_gettid)), std::memory_order_release);
    while (state->proceed.load(std::memory_order_acquire) == 0) sched_yield();
    if (sched_getscheduler(0) != (SCHED_OTHER | SCHED_RESET_ON_FORK)) {
        state->result = 1;
        return nullptr;
    }
    int nested_result = -1;
    pthread_t nested;
    if (pthread_create(&nested, nullptr, NestedWorker, &nested_result) != 0) {
        state->result = 2;
        return nullptr;
    }
    if (pthread_join(nested, nullptr) != 0 || nested_result != 0) {
        state->result = 3;
        return nullptr;
    }
    state->result = 0;
    return nullptr;
}

TEST(SchedSetScheduler, OtherTidAndNestedCloneReset) {
    WorkerState state;
    pthread_t worker;
    ASSERT_EQ(0, pthread_create(&worker, nullptr, FlagWorker, &state));
    for (int attempt = 0;
         attempt < 500 && state.tid.load(std::memory_order_acquire) == 0; ++attempt) {
        usleep(10 * 1000);
    }
    if (state.tid.load(std::memory_order_acquire) == 0) {
        state.proceed.store(1, std::memory_order_release);
        pthread_join(worker, nullptr);
        FAIL() << "worker did not publish its TID within 5 seconds";
    }
    const pid_t tid = state.tid.load(std::memory_order_acquire);
    RawSchedParam zero {0};
    EXPECT_EQ(0, RawSetScheduler(tid, SCHED_OTHER | SCHED_RESET_ON_FORK, &zero)) << strerror(errno);
    EXPECT_EQ(SCHED_OTHER | SCHED_RESET_ON_FORK, sched_getscheduler(tid));
    state.proceed.store(1, std::memory_order_release);
    ASSERT_EQ(0, pthread_join(worker, nullptr));
    EXPECT_EQ(0, state.result);

    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(tid, SCHED_OTHER | SCHED_RESET_ON_FORK, &zero));
    EXPECT_EQ(ESRCH, errno);
}

int RunCredentialCaller(pid_t target, uid_t ruid, uid_t euid, bool set_flag,
                        int expected_errno, bool expect_get_success) {
    pid_t caller = fork();
    if (caller < 0) return 200;
    if (caller == 0) {
        if (setresuid(ruid, euid, euid) != 0) _exit(100 + errno);
        if (expect_get_success) {
            if (sched_getscheduler(target) < 0) _exit(20);
            RawSchedParam query {-1};
            if (RawGetParam(target, &query) != 0 || query.sched_priority != 0) _exit(21);
        }
        RawSchedParam zero {0};
        errno = 0;
        long result = RawSetScheduler(
            target, set_flag ? SCHED_OTHER | SCHED_RESET_ON_FORK : SCHED_OTHER, &zero);
        if (expected_errno == 0) _exit(result == 0 ? 0 : 30 + errno);
        _exit(result == -1 && errno == expected_errno ? 0 : 60 + errno);
    }
    return WaitForChild(caller);
}

TEST(SchedPermission, OwnerCapabilityAndProtectedClearMatrix) {
    if (geteuid() != 0) GTEST_SKIP() << "requires root to construct distinct credentials";

    int hold[2];
    int ready[2];
    ASSERT_EQ(0, pipe(hold));
    ASSERT_EQ(0, pipe(ready));
    pid_t target = fork();
    ASSERT_GE(target, 0) << strerror(errno);
    if (target == 0) {
        close(hold[1]);
        close(ready[0]);
        if (setresuid(1001, 1001, 1001) != 0) _exit(100 + errno);
        WriteByteOrExit(ready[1], 'r');
        close(ready[1]);
        _exit(ReadByte(hold[0]) ? 0 : 121);
    }
    ChildGuard target_guard(target);
    close(hold[0]);
    close(ready[1]);
    ASSERT_TRUE(ReadByteWithTimeout(ready[0], 5000))
        << "target did not publish credential readiness within 5 seconds";
    close(ready[0]);

    // Cross-owner queries are unrestricted, while setters require owner or
    // CAP_SYS_NICE. Matching only current real UID must not authorize.
    EXPECT_EQ(0, RunCredentialCaller(target, 1002, 1002, true, EPERM, true));
    EXPECT_EQ(0, RunCredentialCaller(target, 1001, 1002, true, EPERM, true));

    // Same effective UID may set the flag, but may not clear the protected flag.
    EXPECT_EQ(0, RunCredentialCaller(target, 1001, 1001, true, 0, true));
    EXPECT_EQ(SCHED_OTHER | SCHED_RESET_ON_FORK, sched_getscheduler(target));
    EXPECT_EQ(0, RunCredentialCaller(target, 1001, 1001, false, EPERM, true));
    EXPECT_EQ(SCHED_OTHER | SCHED_RESET_ON_FORK, sched_getscheduler(target));

    // The root parent has CAP_SYS_NICE in the initial namespace and may clear.
    RawSchedParam zero {0};
    EXPECT_EQ(0, RawSetScheduler(target, SCHED_OTHER, &zero)) << strerror(errno);
    EXPECT_EQ(SCHED_OTHER, sched_getscheduler(target));

    EXPECT_EQ(1, write(hold[1], "x", 1));
    close(hold[1]);
    EXPECT_EQ(0, WaitForChild(target));
    target_guard.Release();
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
