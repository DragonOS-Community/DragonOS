#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <climits>
#include <cstddef>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <pthread.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SCHED_RESET_ON_FORK
#define SCHED_RESET_ON_FORK 0x40000000
#endif

#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
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

long RawGetPriorityMax(uint64_t policy) {
    return syscall(SYS_sched_get_priority_max, policy);
}

long RawGetPriorityMin(uint64_t policy) {
    return syscall(SYS_sched_get_priority_min, policy);
}

long RawRrGetInterval(pid_t tid, struct timespec* interval) {
    return syscall(SYS_sched_rr_get_interval, tid, interval);
}

bool IsDragonOS() {
    struct utsname info {};
    return uname(&info) == 0 && strstr(info.release, "dragonos") != nullptr;
}

bool ReadAggregateIdleClassTicks(uint64_t* idle_class_ticks) {
    FILE* stat = fopen("/proc/stat", "r");
    if (stat == nullptr) return false;

    char label[8] = {};
    unsigned long long user = 0;
    unsigned long long nice = 0;
    unsigned long long system = 0;
    unsigned long long idle = 0;
    unsigned long long iowait = 0;
    const int fields = fscanf(stat, "%7s %llu %llu %llu %llu %llu", label, &user, &nice,
                              &system, &idle, &iowait);
    const int close_result = fclose(stat);
    if (fields != 6 || close_result != 0 || strcmp(label, "cpu") != 0) return false;

    *idle_class_ticks = static_cast<uint64_t>(idle) + static_cast<uint64_t>(iowait);
    return true;
}

bool ReadSelfSchedulerLabel(char* label, size_t label_size) {
    if (label_size < 16) return false;
    FILE* status = fopen("/proc/self/status", "r");
    if (status == nullptr) return false;

    char line[128] = {};
    bool found = false;
    while (fgets(line, sizeof(line), status) != nullptr) {
        if (strncmp(line, "priority:\t", 10) != 0) continue;
        found = sscanf(line + 10, "%15s", label) == 1;
        break;
    }
    const int close_result = fclose(status);
    return found && close_result == 0;
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

using ScenarioFn = int (*)();

int RunIsolatedScenario(ScenarioFn scenario, int timeout_ms = 5000) {
    pid_t child = fork();
    if (child < 0) return 250;
    if (child == 0) {
        if (setpgid(0, 0) != 0) _exit(251);
        _exit(scenario());
    }

    // The child also calls setpgid, so EACCES/ESRCH here only means it won
    // the race or already exited.
    (void)setpgid(child, child);
    int status = 0;
    const int attempts = timeout_ms / 10;
    for (int attempt = 0; attempt < attempts; ++attempt) {
        pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) {
            if (!WIFEXITED(status)) {
                (void)kill(-child, SIGKILL);
                return 252;
            }
            const int result = WEXITSTATUS(status);
            if (result != 0) (void)kill(-child, SIGKILL);
            return result;
        }
        if (waited < 0 && errno != EINTR) return 253;
        usleep(10 * 1000);
    }

    // Kill every FIFO worker in the isolated group before reaping the
    // coordinator, so a failed scenario cannot starve later test cases.
    (void)kill(-child, SIGKILL);
    (void)kill(child, SIGKILL);
    while (waitpid(child, nullptr, 0) < 0 && errno == EINTR) {
    }
    return 254;
}

bool WaitStopped(pid_t child, int timeout_ms = 2000) {
    int status = 0;
    const int attempts = timeout_ms / 10;
    for (int attempt = 0; attempt < attempts; ++attempt) {
        pid_t waited = waitpid(child, &status, WNOHANG | WUNTRACED);
        if (waited == child) return WIFSTOPPED(status);
        if (waited < 0 && errno != EINTR) return false;
        usleep(10 * 1000);
    }
    return false;
}

bool SetPolicy(pid_t tid, int policy, int priority) {
    RawSchedParam param {priority};
    return RawSetScheduler(tid, policy, &param) == 0;
}

bool ReadPolicyAndPriority(pid_t tid, int policy, int priority) {
    RawSchedParam param {-1};
    return sched_getscheduler(tid) == policy && RawGetParam(tid, &param) == 0 &&
           param.sched_priority == priority;
}

bool PickAllowedCpus(int* first, int* second = nullptr) {
    cpu_set_t allowed;
    CPU_ZERO(&allowed);
    if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0) return false;

    *first = -1;
    if (second != nullptr) *second = -1;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &allowed)) continue;
        if (*first < 0) {
            *first = cpu;
        } else if (second != nullptr) {
            *second = cpu;
            break;
        }
    }
    return *first >= 0;
}

bool PinTask(pid_t tid, int cpu) {
    cpu_set_t mask;
    CPU_ZERO(&mask);
    CPU_SET(cpu, &mask);
    return sched_setaffinity(tid, sizeof(mask), &mask) == 0;
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

bool WaitForBlockedTask(pid_t pid, int timeout_ms = 2000) {
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/%d/stat", pid);
    const int attempts = timeout_ms;
    for (int attempt = 0; attempt < attempts; ++attempt) {
        FILE* stat = fopen(path, "r");
        if (stat != nullptr) {
            char line[512] = {};
            const bool read_ok = fgets(line, sizeof(line), stat) != nullptr;
            fclose(stat);
            if (read_ok) {
                const char* comm_end = strrchr(line, ')');
                if (comm_end != nullptr && comm_end[1] == ' ' &&
                    (comm_end[2] == 'S' || comm_end[2] == 'D')) {
                    return true;
                }
            }
        }
        usleep(1000);
    }
    return false;
}

uint64_t ThreadCpuTimeNs() {
    struct timespec now {};
    if (clock_gettime(CLOCK_THREAD_CPUTIME_ID, &now) != 0) return UINT64_MAX;
    return static_cast<uint64_t>(now.tv_sec) * 1000 * 1000 * 1000 +
           static_cast<uint64_t>(now.tv_nsec);
}

int CurrentCpu() {
    unsigned int cpu = 0;
    return syscall(SYS_getcpu, &cpu, nullptr, nullptr) == 0 ? static_cast<int>(cpu) : -1;
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

TEST(SchedGetScheduler, ProcStatusKeepsLegacyClassLabel) {
    if (!IsDragonOS()) GTEST_SKIP() << "DragonOS-specific proc status field";

    char label[16] = {};
    ASSERT_TRUE(ReadSelfSchedulerLabel(label, sizeof(label)))
        << "failed to read priority field from /proc/self/status";
    EXPECT_STREQ("CFS", label);
}

TEST(SchedRrInterval, CurrentAndErrorsMatchLinux) {
    struct timespec interval {-1, -1};
    EXPECT_EQ(0, RawRrGetInterval(0, &interval)) << strerror(errno);
    EXPECT_GE(interval.tv_sec, 0);
    EXPECT_GE(interval.tv_nsec, 0);
    EXPECT_LT(interval.tv_nsec, 1000 * 1000 * 1000);

    errno = 0;
    EXPECT_EQ(-1, RawRrGetInterval(-1, &interval));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, RawRrGetInterval(INT_MAX, nullptr));
    EXPECT_EQ(ESRCH, errno);
    errno = 0;
    EXPECT_EQ(-1, RawRrGetInterval(0, nullptr));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, RawRrGetInterval(0, reinterpret_cast<struct timespec*>(1)));
    EXPECT_EQ(EFAULT, errno);
}

TEST(SchedClassAccounting, IdleClassTicksAdvanceWhileCallerSleeps) {
    if (!IsDragonOS()) GTEST_SKIP() << "DragonOS idle-class accounting regression";

    uint64_t before = 0;
    ASSERT_TRUE(ReadAggregateIdleClassTicks(&before))
        << "failed to parse aggregate /proc/stat cpu line";
    ASSERT_EQ(0, usleep(300 * 1000)) << strerror(errno);
    uint64_t after = 0;
    ASSERT_TRUE(ReadAggregateIdleClassTicks(&after))
        << "failed to parse aggregate /proc/stat cpu line";
    EXPECT_GT(after, before) << "idle-class accounting did not advance while the test slept";
}

TEST(SchedPriorityQueries, KnownPolicyMatrixMatchesLinux) {
    struct PolicyRange {
        int policy;
        int maximum;
        int minimum;
    };
    const PolicyRange ranges[] = {
        {SCHED_OTHER, 0, 0},    {SCHED_FIFO, 99, 1}, {SCHED_RR, 99, 1},
        {SCHED_BATCH, 0, 0},    {SCHED_IDLE, 0, 0},  {SCHED_DEADLINE, 0, 0},
    };

    for (const auto& range : ranges) {
        EXPECT_EQ(range.maximum, sched_get_priority_max(range.policy))
            << "policy=" << range.policy << ": " << strerror(errno);
        EXPECT_EQ(range.minimum, sched_get_priority_min(range.policy))
            << "policy=" << range.policy << ": " << strerror(errno);
    }
}

TEST(SchedPriorityQueries, InvalidPoliciesReturnEinval) {
    const int invalid_policies[] = {
        -1, 4, 7, SCHED_OTHER | SCHED_RESET_ON_FORK, SCHED_FIFO | SCHED_RESET_ON_FORK, INT_MAX,
    };

    for (int policy : invalid_policies) {
        errno = 0;
        EXPECT_EQ(-1, sched_get_priority_max(policy)) << "policy=" << policy;
        EXPECT_EQ(EINVAL, errno) << "policy=" << policy;
        errno = 0;
        EXPECT_EQ(-1, sched_get_priority_min(policy)) << "policy=" << policy;
        EXPECT_EQ(EINVAL, errno) << "policy=" << policy;
    }
}

TEST(SchedPriorityQueries, RawSyscallUsesSignedIntAbi) {
    constexpr uint64_t kNonzeroHighBits = uint64_t {1} << 32;

    EXPECT_EQ(99, RawGetPriorityMax(kNonzeroHighBits | SCHED_FIFO));
    EXPECT_EQ(1, RawGetPriorityMin(kNonzeroHighBits | SCHED_FIFO));

    errno = 0;
    EXPECT_EQ(-1, RawGetPriorityMax(kNonzeroHighBits | 4));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, RawGetPriorityMin(kNonzeroHighBits | 4));
    EXPECT_EQ(EINVAL, errno);
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
    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(0, SCHED_RR, &zero));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    RawSchedParam hundred {100};
    EXPECT_EQ(-1, RawSetScheduler(0, SCHED_FIFO, &hundred));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, RawSetScheduler(0, SCHED_RR, &hundred));
    EXPECT_EQ(EINVAL, errno);
}

int FifoRoundTripScenario() {
    if (!SetPolicy(0, SCHED_FIFO, 1)) return 10;
    if (!ReadPolicyAndPriority(0, SCHED_FIFO, 1)) return 11;
    if (!SetPolicy(0, SCHED_FIFO, 99)) return 12;
    if (!ReadPolicyAndPriority(0, SCHED_FIFO, 99)) return 13;
    if (!SetPolicy(0, SCHED_OTHER, 0)) return 14;
    return ReadPolicyAndPriority(0, SCHED_OTHER, 0) ? 0 : 15;
}

TEST(SchedFifoPolicy, FairFifoPriorityRoundTrip) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    EXPECT_EQ(0, RunIsolatedScenario(FifoRoundTripScenario));
}

int RrRoundTripScenario() {
    if (!SetPolicy(0, SCHED_RR, 1)) return 30;
    if (!ReadPolicyAndPriority(0, SCHED_RR, 1)) return 31;
    char label[16] = {};
    if (!ReadSelfSchedulerLabel(label, sizeof(label)) || strcmp(label, "RR") != 0) return 32;
    if (!SetPolicy(0, SCHED_RR, 99)) return 33;
    if (!ReadPolicyAndPriority(0, SCHED_RR, 99)) return 34;
    if (!SetPolicy(0, SCHED_FIFO, 99)) return 35;
    if (!ReadPolicyAndPriority(0, SCHED_FIFO, 99)) return 36;
    if (!SetPolicy(0, SCHED_RR, 99)) return 37;
    if (!ReadPolicyAndPriority(0, SCHED_RR, 99)) return 38;
    if (!SetPolicy(0, SCHED_OTHER, 0)) return 39;
    return ReadPolicyAndPriority(0, SCHED_OTHER, 0) ? 0 : 40;
}

TEST(SchedRrPolicy, FairFifoRrRoundTrip) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(RrRoundTripScenario));
}

int RrIntervalPolicyScenario() {
    struct timespec interval {-1, -1};
    if (!SetPolicy(0, SCHED_RR, 20)) return 50;
    if (RawRrGetInterval(0, &interval) != 0 || interval.tv_sec != 0 ||
        interval.tv_nsec != 100 * 1000 * 1000) {
        return 51;
    }

    if (!SetPolicy(0, SCHED_FIFO, 20)) return 52;
    interval = {-1, -1};
    if (sched_rr_get_interval(0, &interval) != 0 || interval.tv_sec != 0 ||
        interval.tv_nsec != 0) {
        return 53;
    }

    if (!SetPolicy(0, SCHED_OTHER, 0)) return 54;
    interval = {-1, -1};
    if (RawRrGetInterval(0, &interval) != 0 || interval.tv_sec != 0 ||
        interval.tv_nsec != 0) {
        return 55;
    }
    return 0;
}

TEST(SchedRrInterval, PolicyIntervalsMatchLinux) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(RrIntervalPolicyScenario));
}

int FifoForkScenario() {
    if (!SetPolicy(0, SCHED_FIFO, 20)) return 20;
    pid_t inherited = fork();
    if (inherited < 0) return 21;
    if (inherited == 0) {
        _exit(ReadPolicyAndPriority(0, SCHED_FIFO, 20) ? 0 : 22);
    }
    if (WaitForChild(inherited) != 0) return 23;

    if (!SetPolicy(0, SCHED_FIFO | SCHED_RESET_ON_FORK, 20)) return 24;
    pid_t reset = fork();
    if (reset < 0) return 25;
    if (reset == 0) {
        _exit(ReadPolicyAndPriority(0, SCHED_OTHER, 0) ? 0 : 26);
    }
    if (WaitForChild(reset) != 0) return 27;
    if (!ReadPolicyAndPriority(0, SCHED_FIFO | SCHED_RESET_ON_FORK, 20)) return 28;
    return SetPolicy(0, SCHED_OTHER, 0) ? 0 : 29;
}

TEST(SchedFifoReset, ForkInheritanceAndResetOnFork) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    EXPECT_EQ(0, RunIsolatedScenario(FifoForkScenario));
}

int RrForkScenario() {
    if (!SetPolicy(0, SCHED_RR, 20)) return 40;
    pid_t inherited = fork();
    if (inherited < 0) return 41;
    if (inherited == 0) {
        _exit(ReadPolicyAndPriority(0, SCHED_RR, 20) ? 0 : 42);
    }
    if (WaitForChild(inherited) != 0) return 43;

    if (!SetPolicy(0, SCHED_RR | SCHED_RESET_ON_FORK, 20)) return 44;
    pid_t reset = fork();
    if (reset < 0) return 45;
    if (reset == 0) {
        _exit(ReadPolicyAndPriority(0, SCHED_OTHER, 0) ? 0 : 46);
    }
    if (WaitForChild(reset) != 0) return 47;
    if (!ReadPolicyAndPriority(0, SCHED_RR | SCHED_RESET_ON_FORK, 20)) return 48;
    return SetPolicy(0, SCHED_OTHER, 0) ? 0 : 49;
}

TEST(SchedRrReset, ForkInheritanceAndResetOnFork) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(RrForkScenario));
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
    struct timespec interval {-1, -1};
    EXPECT_EQ(0, RawRrGetInterval(tid, &interval)) << strerror(errno);
    EXPECT_GE(interval.tv_sec, 0);
    EXPECT_GE(interval.tv_nsec, 0);
    EXPECT_LT(interval.tv_nsec, 1000 * 1000 * 1000);
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

bool WaitAtomicValue(const std::atomic<int>& value, int expected, int timeout_ms = 2000) {
    const int attempts = timeout_ms / 10;
    for (int attempt = 0; attempt < attempts; ++attempt) {
        if (value.load(std::memory_order_acquire) == expected) return true;
        usleep(10 * 1000);
    }
    return false;
}

struct RlimitThreadState {
    std::atomic<int> phase {0};
    int first_result = -1;
    int second_result = -1;
    int third_result = -1;
    int race_cpu = -1;
};

constexpr int kRlimitRaceRounds = 1024;

void* RlimitWorker(void* arg) {
    auto* state = static_cast<RlimitThreadState*>(arg);
    if (syscall(SYS_setresuid, 1001, 1001, 1001) != 0) {
        state->phase.store(90 + errno, std::memory_order_release);
        return nullptr;
    }
    state->phase.store(1, std::memory_order_release);

    while (state->phase.load(std::memory_order_acquire) != 2) sched_yield();
    struct rlimit limit {};
    if (getrlimit(RLIMIT_RTPRIO, &limit) != 0 || limit.rlim_cur != 10) {
        state->first_result = 80;
    } else if (!SetPolicy(0, SCHED_FIFO, 10)) {
        state->first_result = errno;
    } else {
        state->first_result = 0;
    }
    state->phase.store(3, std::memory_order_release);

    while (state->phase.load(std::memory_order_acquire) != 4) sched_yield();
    limit = {};
    if (getrlimit(RLIMIT_RTPRIO, &limit) != 0 || limit.rlim_cur != 0) {
        state->second_result = 81;
    } else if (!SetPolicy(0, SCHED_FIFO, 1)) {
        // Lowering an existing FIFO priority remains permitted after the
        // soft limit is reduced to zero.
        state->second_result = 82 + errno;
    } else if (!SetPolicy(0, SCHED_OTHER, 0)) {
        state->second_result = 83 + errno;
    } else {
        state->second_result = 0;
    }
    state->phase.store(5, std::memory_order_release);

    while (state->phase.load(std::memory_order_acquire) != 6) sched_yield();
    errno = 0;
    if (!SetPolicy(0, SCHED_FIFO, 1) && errno == EPERM) {
        state->third_result = 0;
    } else {
        const int saved_errno = errno;
        (void)SetPolicy(0, SCHED_OTHER, 0);
        state->third_result = 84 + saved_errno;
    }
    state->phase.store(7, std::memory_order_release);

    while (state->phase.load(std::memory_order_acquire) != 8) sched_yield();
    if (state->race_cpu < 0) {
        state->phase.store(9, std::memory_order_release);
        return nullptr;
    }
    if (!PinTask(0, state->race_cpu)) {
        state->phase.store(-1, std::memory_order_release);
        return nullptr;
    }
    state->phase.store(9, std::memory_order_release);
    for (int round = 0; round < kRlimitRaceRounds; ++round) {
        const int start_phase = 10 + round * 2;
        while (state->phase.load(std::memory_order_acquire) != start_phase) sched_yield();
        struct rlimit stale_high {10, 10};
        // Depending on serialization order this either succeeds before the
        // privileged lowering writer, or fails with EPERM after it. It must
        // never restore the group's hard limit after that writer completes.
        (void)setrlimit(RLIMIT_RTPRIO, &stale_high);
        state->phase.store(start_phase + 1, std::memory_order_release);
    }
    return nullptr;
}

int ThreadGroupRlimitScenario() {
    struct rlimit high {10, 10};
    if (setrlimit(RLIMIT_RTPRIO, &high) != 0) return 10 + errno;

    pid_t child = fork();
    if (child < 0) return 11;
    if (child == 0) {
        struct rlimit child_limit {0, 10};
        _exit(setrlimit(RLIMIT_RTPRIO, &child_limit) == 0 ? 0 : 12 + errno);
    }
    if (WaitForChild(child) != 0) return 13;
    struct rlimit parent_limit {};
    if (getrlimit(RLIMIT_RTPRIO, &parent_limit) != 0 || parent_limit.rlim_cur != 10) return 14;

    RlimitThreadState state;
    pthread_t worker;
    if (pthread_create(&worker, nullptr, RlimitWorker, &state) != 0) return 20;
    if (!WaitAtomicValue(state.phase, 1)) return 21;

    state.phase.store(2, std::memory_order_release);
    if (!WaitAtomicValue(state.phase, 3) || state.first_result != 0) return 30 + state.first_result;

    struct rlimit low {0, 10};
    if (setrlimit(RLIMIT_RTPRIO, &low) != 0) return 40 + errno;
    state.phase.store(4, std::memory_order_release);
    if (!WaitAtomicValue(state.phase, 5) || state.second_result != 0) {
        return 50 + state.second_result;
    }
    state.phase.store(6, std::memory_order_release);
    if (!WaitAtomicValue(state.phase, 7) || state.third_result != 0) return 60 + state.third_result;

    int controller_cpu = -1;
    int worker_cpu = -1;
    if (!PickAllowedCpus(&controller_cpu, &worker_cpu)) return 70;
    state.race_cpu = worker_cpu;
    state.phase.store(8, std::memory_order_release);
    if (!WaitAtomicValue(state.phase, 9)) return 71;
    if (worker_cpu >= 0) {
        if (!PinTask(0, controller_cpu)) return 72;
        for (int round = 0; round < kRlimitRaceRounds; ++round) {
            if (setrlimit(RLIMIT_RTPRIO, &high) != 0) return 73;
            const int start_phase = 10 + round * 2;
            state.phase.store(start_phase, std::memory_order_release);
            struct rlimit zero {0, 0};
            if (setrlimit(RLIMIT_RTPRIO, &zero) != 0) return 74;
            while (state.phase.load(std::memory_order_acquire) != start_phase + 1) {
                if (state.phase.load(std::memory_order_relaxed) < 0) return 75;
                __atomic_signal_fence(__ATOMIC_SEQ_CST);
            }
            struct rlimit observed {};
            if (getrlimit(RLIMIT_RTPRIO, &observed) != 0 || observed.rlim_cur != 0 ||
                observed.rlim_max != 0) {
                return 76;
            }
        }
    }
    return pthread_join(worker, nullptr) == 0 ? 0 : 77;
}

int RunRlimitMismatchCase(rlim_t target_limit, rlim_t caller_limit, int priority,
                          int expected_errno) {
    int ready[2];
    int hold[2];
    if (pipe(ready) != 0 || pipe(hold) != 0) return 100;

    pid_t target = fork();
    if (target < 0) return 101;
    if (target == 0) {
        close(ready[0]);
        close(hold[1]);
        struct rlimit limit {target_limit, target_limit};
        if (setrlimit(RLIMIT_RTPRIO, &limit) != 0) _exit(10 + errno);
        if (syscall(SYS_setresuid, 1001, 1001, 1001) != 0) _exit(20 + errno);
        WriteByteOrExit(ready[1], 'r');
        _exit(ReadByte(hold[0]) ? 0 : 30);
    }
    close(ready[1]);
    close(hold[0]);
    if (!ReadByteWithTimeout(ready[0], 2000)) {
        kill(target, SIGKILL);
        WaitForChild(target);
        return 102;
    }
    close(ready[0]);

    pid_t caller = fork();
    if (caller < 0) return 103;
    if (caller == 0) {
        struct rlimit limit {caller_limit, caller_limit};
        if (setrlimit(RLIMIT_RTPRIO, &limit) != 0) _exit(40 + errno);
        if (syscall(SYS_setresuid, 1001, 1001, 1001) != 0) _exit(50 + errno);
        errno = 0;
        const bool changed = SetPolicy(target, SCHED_FIFO, priority);
        if (expected_errno == 0) {
            if (!changed || !ReadPolicyAndPriority(target, SCHED_FIFO, priority)) _exit(60 + errno);
            if (!SetPolicy(target, SCHED_OTHER, 0)) _exit(70 + errno);
            _exit(0);
        }
        _exit(!changed && errno == expected_errno ? 0 : 80 + errno);
    }

    int result = WaitForChild(caller);
    const bool released = write(hold[1], "x", 1) == 1;
    close(hold[1]);
    const int target_result = WaitForChild(target);
    return result == 0 && released && target_result == 0 ? 0 : 110 + result;
}

int RlimitTargetVsCallerScenario() {
    int result = RunRlimitMismatchCase(10, 0, 10, 0);
    if (result != 0) return result;
    result = RunRlimitMismatchCase(0, 10, 1, EPERM);
    if (result != 0) return result;
    return RunRlimitMismatchCase(10, 0, 11, EPERM);
}

int NestedUserNamespaceRlimitScenario() {
    struct rlimit initial {};
    if (getrlimit(RLIMIT_RTPRIO, &initial) != 0 || initial.rlim_max != 0) return 120;
    if (unshare(CLONE_NEWUSER) != 0) return 121 + errno;

    struct rlimit raised {1, 1};
    errno = 0;
    if (setrlimit(RLIMIT_RTPRIO, &raised) == 0 || errno != EPERM) return 130 + errno;

    errno = 0;
    if (SetPolicy(0, SCHED_FIFO, 1) || errno != EPERM) {
        (void)SetPolicy(0, SCHED_OTHER, 0);
        return 140 + errno;
    }
    return 0;
}

TEST(SchedFifoRlimit, ThreadGroupSharesRaisedAndLoweredLimit) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    EXPECT_EQ(0, RunIsolatedScenario(ThreadGroupRlimitScenario));
}

TEST(SchedFifoRlimit, AuthorizationUsesTargetNotCallerLimit) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    EXPECT_EQ(0, RunIsolatedScenario(RlimitTargetVsCallerScenario));
}

TEST(SchedFifoRlimit, NestedUserNamespaceCannotRaiseHardLimit) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    EXPECT_EQ(0, RunIsolatedScenario(NestedUserNamespaceRlimitScenario));
}

int RrRlimitScenario() {
    struct rlimit high {10, 10};
    if (setrlimit(RLIMIT_RTPRIO, &high) != 0) return 10 + errno;
    if (syscall(SYS_setresuid, 1001, 1001, 1001) != 0) return 20 + errno;

    if (!SetPolicy(0, SCHED_RR, 10)) return 30 + errno;
    errno = 0;
    if (SetPolicy(0, SCHED_RR, 11) || errno != EPERM) return 40 + errno;

    struct rlimit low {0, 10};
    if (setrlimit(RLIMIT_RTPRIO, &low) != 0) return 50 + errno;
    if (!SetPolicy(0, SCHED_RR, 1)) return 60 + errno;

    errno = 0;
    if (SetPolicy(0, SCHED_FIFO, 1) || errno != EPERM) return 70 + errno;
    if (!ReadPolicyAndPriority(0, SCHED_RR, 1)) return 80;
    if (!SetPolicy(0, SCHED_OTHER, 0)) return 81 + errno;

    errno = 0;
    if (SetPolicy(0, SCHED_RR, 1) || errno != EPERM) {
        (void)SetPolicy(0, SCHED_OTHER, 0);
        return 90 + errno;
    }
    return 0;
}

TEST(SchedRrRlimit, PriorityAndExactPolicyRules) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(RrRlimitScenario));
}

struct SharedFifoState {
    int sequence_index;
    char sequence[4];
    int throttle_stage;
    int remote_runner_started;
    int remote_candidate_ran;
    int remote_runner_release;
    int stress_started;
    int stress_release;
};

SharedFifoState* NewSharedFifoState() {
    void* mapping = mmap(nullptr, sizeof(SharedFifoState), PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) return nullptr;
    memset(mapping, 0, sizeof(SharedFifoState));
    return static_cast<SharedFifoState*>(mapping);
}

void RecordEvent(SharedFifoState* state, char event) {
    int position = __atomic_fetch_add(&state->sequence_index, 1, __ATOMIC_SEQ_CST);
    if (position >= 0 && position < static_cast<int>(sizeof(state->sequence))) {
        state->sequence[position] = event;
    }
}

struct SharedRtState {
    int sequence_index;
    char sequence[4];
    int first_done;
    int peer_ran_early;
    int started_on_cpu1;
    int seen_cpu0;
};

SharedRtState* NewSharedRtState() {
    void* mapping = mmap(nullptr, sizeof(SharedRtState), PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    if (mapping == MAP_FAILED) return nullptr;
    memset(mapping, 0, sizeof(SharedRtState));
    return static_cast<SharedRtState*>(mapping);
}

void RecordEvent(SharedRtState* state, char event) {
    int position = __atomic_fetch_add(&state->sequence_index, 1, __ATOMIC_SEQ_CST);
    if (position >= 0 && position < static_cast<int>(sizeof(state->sequence))) {
        state->sequence[position] = event;
    }
}

bool SequenceIs(const SharedRtState* state, const char* expected, int count) {
    if (__atomic_load_n(&state->sequence_index, __ATOMIC_SEQ_CST) != count) return false;
    return memcmp(state->sequence, expected, static_cast<size_t>(count)) == 0;
}

int OrderedRotationScenario(bool remote, bool mixed_fifo_peer) {
    int controller_cpu = -1;
    int worker_cpu = -1;
    if (!PickAllowedCpus(&controller_cpu, remote ? &worker_cpu : nullptr)) return 10;
    if (remote && worker_cpu < 0) return 11;
    if (!PinTask(0, controller_cpu)) return 12;
    if (!remote) worker_cpu = controller_cpu;

    SharedRtState* state = NewSharedRtState();
    if (state == nullptr) return 13;

    pid_t first = fork();
    if (first < 0) return 14;
    if (first == 0) {
        raise(SIGSTOP);
        RecordEvent(state, 'A');
        while (__atomic_load_n(&state->sequence_index, __ATOMIC_ACQUIRE) < 2) {
            __atomic_signal_fence(__ATOMIC_SEQ_CST);
        }
        RecordEvent(state, 'A');
        _exit(0);
    }
    ChildGuard first_guard(first);
    if (!WaitStopped(first) || !PinTask(first, worker_cpu) ||
        !SetPolicy(first, SCHED_RR, 20)) {
        return 15;
    }

    pid_t second = fork();
    if (second < 0) return 16;
    if (second == 0) {
        raise(SIGSTOP);
        RecordEvent(state, 'B');
        if (mixed_fifo_peer && sched_yield() != 0) _exit(20);
        while (__atomic_load_n(&state->sequence_index, __ATOMIC_ACQUIRE) < 3) {
            __atomic_signal_fence(__ATOMIC_SEQ_CST);
        }
        _exit(0);
    }
    ChildGuard second_guard(second);
    const int second_policy = mixed_fifo_peer ? SCHED_FIFO : SCHED_RR;
    if (!WaitStopped(second) || !PinTask(second, worker_cpu) ||
        !SetPolicy(second, second_policy, 20)) {
        return 17;
    }

    if (!remote && !SetPolicy(0, SCHED_FIFO, 50)) return 18;
    if (kill(first, SIGCONT) != 0 || kill(second, SIGCONT) != 0) {
        if (!remote) (void)SetPolicy(0, SCHED_OTHER, 0);
        return 19;
    }
    if (!remote && !SetPolicy(0, SCHED_OTHER, 0)) return 21;

    const int first_result = WaitForChild(first);
    first_guard.Release();
    const int second_result = WaitForChild(second);
    second_guard.Release();
    const bool ordered = SequenceIs(state, "ABA", 3);
    munmap(state, sizeof(*state));
    return first_result == 0 && second_result == 0 && ordered ? 0 : 22;
}

int RrRotationScenario() {
    return OrderedRotationScenario(false, false);
}

int MixedRotationScenario() {
    return OrderedRotationScenario(false, true);
}

int RemoteRrRotationScenario() {
    return OrderedRotationScenario(true, false);
}

int NoPrematurePeerScenario(int first_policy, int first_priority, int second_policy,
                            int second_priority) {
    int cpu = -1;
    if (!PickAllowedCpus(&cpu) || !PinTask(0, cpu)) return 30;
    SharedRtState* state = NewSharedRtState();
    if (state == nullptr) return 31;

    pid_t first = fork();
    if (first < 0) return 32;
    if (first == 0) {
        raise(SIGSTOP);
        const uint64_t start = ThreadCpuTimeNs();
        if (start == UINT64_MAX) _exit(20);
        for (;;) {
            const uint64_t now = ThreadCpuTimeNs();
            if (now == UINT64_MAX) _exit(21);
            if (__atomic_load_n(&state->peer_ran_early, __ATOMIC_ACQUIRE) != 0) _exit(22);
            if (now - start >= 300 * 1000 * 1000ULL) break;
        }
        __atomic_store_n(&state->first_done, 1, __ATOMIC_RELEASE);
        _exit(0);
    }
    ChildGuard first_guard(first);
    if (!WaitStopped(first) || !PinTask(first, cpu) ||
        !SetPolicy(first, first_policy, first_priority)) {
        return 33;
    }

    pid_t second = fork();
    if (second < 0) return 34;
    if (second == 0) {
        raise(SIGSTOP);
        if (__atomic_load_n(&state->first_done, __ATOMIC_ACQUIRE) == 0) {
            __atomic_store_n(&state->peer_ran_early, 1, __ATOMIC_RELEASE);
        }
        _exit(0);
    }
    ChildGuard second_guard(second);
    if (!WaitStopped(second) || !PinTask(second, cpu) ||
        !SetPolicy(second, second_policy, second_priority)) {
        return 35;
    }

    if (!SetPolicy(0, SCHED_FIFO, 50)) return 36;
    if (kill(first, SIGCONT) != 0 || kill(second, SIGCONT) != 0) {
        (void)SetPolicy(0, SCHED_OTHER, 0);
        return 37;
    }
    if (!SetPolicy(0, SCHED_OTHER, 0)) return 38;

    const int first_result = WaitForChild(first);
    first_guard.Release();
    const int second_result = WaitForChild(second);
    second_guard.Release();
    const bool correct = __atomic_load_n(&state->first_done, __ATOMIC_ACQUIRE) == 1 &&
                         __atomic_load_n(&state->peer_ran_early, __ATOMIC_ACQUIRE) == 0;
    munmap(state, sizeof(*state));
    return first_result == 0 && second_result == 0 && correct ? 0 : 39;
}

int RrPriorityIsolationScenario() {
    return NoPrematurePeerScenario(SCHED_RR, 20, SCHED_RR, 10);
}

int FifoNoTimesliceScenario() {
    return NoPrematurePeerScenario(SCHED_FIFO, 20, SCHED_FIFO, 20);
}

int RrBlockedWakeTailScenario() {
    int cpu = -1;
    if (!PickAllowedCpus(&cpu) || !PinTask(0, cpu)) return 40;
    SharedRtState* state = NewSharedRtState();
    if (state == nullptr) return 41;
    int ready[2];
    int wake[2];
    if (pipe(ready) != 0 || pipe(wake) != 0) return 42;

    pid_t blocked = fork();
    if (blocked < 0) return 43;
    if (blocked == 0) {
        close(ready[0]);
        close(wake[1]);
        WriteByteOrExit(ready[1], 'r');
        close(ready[1]);
        if (!ReadByte(wake[0])) _exit(20);
        RecordEvent(state, 'B');
        _exit(0);
    }
    ChildGuard blocked_guard(blocked);
    close(ready[1]);
    close(wake[0]);
    if (!ReadByteWithTimeout(ready[0], 2000) || !WaitForBlockedTask(blocked) ||
        !PinTask(blocked, cpu) || !SetPolicy(blocked, SCHED_RR, 20)) {
        return 44;
    }
    close(ready[0]);

    pid_t first = fork();
    if (first < 0) return 45;
    if (first == 0) {
        raise(SIGSTOP);
        RecordEvent(state, 'A');
        if (sched_yield() != 0) _exit(21);
        _exit(0);
    }
    ChildGuard first_guard(first);
    if (!WaitStopped(first) || !PinTask(first, cpu) || !SetPolicy(first, SCHED_FIFO, 20)) {
        return 46;
    }

    if (!SetPolicy(0, SCHED_FIFO, 50)) return 47;
    if (kill(first, SIGCONT) != 0 || write(wake[1], "x", 1) != 1) {
        (void)SetPolicy(0, SCHED_OTHER, 0);
        return 48;
    }
    close(wake[1]);
    if (!SetPolicy(0, SCHED_OTHER, 0)) return 49;

    const int first_result = WaitForChild(first);
    first_guard.Release();
    const int blocked_result = WaitForChild(blocked);
    blocked_guard.Release();
    const bool ordered = SequenceIs(state, "AB", 2);
    munmap(state, sizeof(*state));
    return first_result == 0 && blocked_result == 0 && ordered ? 0 : 50;
}

int ResetOnlyKeepsQueueOrderScenario() {
    int cpu = -1;
    if (!PickAllowedCpus(&cpu) || !PinTask(0, cpu)) return 51;
    SharedRtState* state = NewSharedRtState();
    if (state == nullptr) return 52;

    pid_t first = fork();
    if (first < 0) return 53;
    if (first == 0) {
        raise(SIGSTOP);
        RecordEvent(state, 'A');
        if (sched_yield() != 0) _exit(20);
        _exit(0);
    }
    ChildGuard first_guard(first);
    if (!WaitStopped(first) || !PinTask(first, cpu) || !SetPolicy(first, SCHED_FIFO, 20)) {
        return 54;
    }

    pid_t second = fork();
    if (second < 0) return 55;
    if (second == 0) {
        raise(SIGSTOP);
        RecordEvent(state, 'B');
        _exit(0);
    }
    ChildGuard second_guard(second);
    if (!WaitStopped(second) || !PinTask(second, cpu) || !SetPolicy(second, SCHED_FIFO, 20)) {
        return 56;
    }

    if (!SetPolicy(0, SCHED_FIFO, 50)) return 57;
    if (kill(first, SIGCONT) != 0 || kill(second, SIGCONT) != 0 ||
        !SetPolicy(first, SCHED_FIFO | SCHED_RESET_ON_FORK, 20)) {
        (void)SetPolicy(0, SCHED_OTHER, 0);
        return 58;
    }
    if (!SetPolicy(0, SCHED_OTHER, 0)) return 59;

    const int first_result = WaitForChild(first);
    first_guard.Release();
    const int second_result = WaitForChild(second);
    second_guard.Release();
    const bool ordered = SequenceIs(state, "AB", 2);
    munmap(state, sizeof(*state));
    return first_result == 0 && second_result == 0 && ordered ? 0 : 60;
}

int RrAffinityMigrationScenario() {
    int controller_cpu = -1;
    int worker_cpu = -1;
    if (!PickAllowedCpus(&controller_cpu, &worker_cpu) || worker_cpu < 0 ||
        !PinTask(0, controller_cpu)) {
        return 61;
    }
    SharedRtState* state = NewSharedRtState();
    if (state == nullptr) return 62;

    pid_t worker = fork();
    if (worker < 0) return 63;
    if (worker == 0) {
        raise(SIGSTOP);
        if (CurrentCpu() != worker_cpu) _exit(20);
        __atomic_store_n(&state->started_on_cpu1, 1, __ATOMIC_RELEASE);
        for (;;) {
            const int cpu = CurrentCpu();
            if (cpu < 0) _exit(21);
            if (cpu == controller_cpu) {
                __atomic_store_n(&state->seen_cpu0, 1, __ATOMIC_RELEASE);
                _exit(0);
            }
        }
    }
    ChildGuard worker_guard(worker);
    if (!WaitStopped(worker) || !PinTask(worker, worker_cpu) ||
        !SetPolicy(worker, SCHED_RR, 20) || kill(worker, SIGCONT) != 0) {
        return 64;
    }

    bool started = false;
    for (int attempt = 0; attempt < 200; ++attempt) {
        if (__atomic_load_n(&state->started_on_cpu1, __ATOMIC_ACQUIRE) != 0) {
            started = true;
            break;
        }
        usleep(10 * 1000);
    }
    if (!started || !PinTask(worker, controller_cpu)) return 65;

    const int worker_result = WaitForChild(worker);
    worker_guard.Release();
    const bool migrated = __atomic_load_n(&state->seen_cpu0, __ATOMIC_ACQUIRE) == 1;
    munmap(state, sizeof(*state));
    return worker_result == 0 && migrated ? 0 : 66;
}

int FifoYieldScenario() {
    int cpu = -1;
    if (!PickAllowedCpus(&cpu) || !PinTask(0, cpu)) return 10;
    SharedFifoState* state = NewSharedFifoState();
    if (state == nullptr) return 11;

    pid_t first = fork();
    if (first < 0) return 12;
    if (first == 0) {
        raise(SIGSTOP);
        RecordEvent(state, 'A');
        if (sched_yield() != 0) _exit(20);
        RecordEvent(state, 'A');
        _exit(0);
    }
    if (!WaitStopped(first) || !PinTask(first, cpu) || !SetPolicy(first, SCHED_FIFO, 20)) {
        kill(first, SIGKILL);
        return 13;
    }

    pid_t second = fork();
    if (second < 0) {
        kill(first, SIGKILL);
        return 14;
    }
    if (second == 0) {
        raise(SIGSTOP);
        RecordEvent(state, 'B');
        _exit(0);
    }
    if (!WaitStopped(second) || !PinTask(second, cpu) || !SetPolicy(second, SCHED_FIFO, 20)) {
        kill(first, SIGKILL);
        kill(second, SIGKILL);
        return 15;
    }

    if (!SetPolicy(0, SCHED_FIFO, 50)) return 16;
    if (kill(first, SIGCONT) != 0 || kill(second, SIGCONT) != 0) {
        (void)SetPolicy(0, SCHED_OTHER, 0);
        return 17;
    }
    if (!SetPolicy(0, SCHED_OTHER, 0)) return 18;

    int first_result = WaitForChild(first);
    int second_result = WaitForChild(second);
    bool ordered = __atomic_load_n(&state->sequence_index, __ATOMIC_SEQ_CST) == 3 &&
                   state->sequence[0] == 'A' && state->sequence[1] == 'B' &&
                   state->sequence[2] == 'A';
    munmap(state, sizeof(*state));
    return first_result == 0 && second_result == 0 && ordered ? 0 : 19;
}

int RealtimeThrottlingScenario(int policy) {
    int cpu = -1;
    if (!PickAllowedCpus(&cpu) || !PinTask(0, cpu)) return 30;
    SharedFifoState* state = NewSharedFifoState();
    if (state == nullptr) return 31;

    pid_t runner = fork();
    if (runner < 0) return 32;
    if (runner == 0) {
        raise(SIGSTOP);
        __atomic_store_n(&state->throttle_stage, 1, __ATOMIC_RELEASE);
        while (__atomic_load_n(&state->throttle_stage, __ATOMIC_ACQUIRE) == 1) {
            __atomic_signal_fence(__ATOMIC_SEQ_CST);
        }
        if (__atomic_load_n(&state->throttle_stage, __ATOMIC_ACQUIRE) != 2) _exit(121);
        __atomic_store_n(&state->throttle_stage, 3, __ATOMIC_RELEASE);
        _exit(0);
    }
    if (!WaitStopped(runner) || !PinTask(runner, cpu) || !SetPolicy(runner, policy, 20)) {
        kill(runner, SIGKILL);
        return 33;
    }

    pid_t observer = fork();
    if (observer < 0) {
        kill(runner, SIGKILL);
        return 34;
    }
    if (observer == 0) {
        raise(SIGSTOP);
        int expected = 1;
        if (!__atomic_compare_exchange_n(&state->throttle_stage, &expected, 2, false,
                                         __ATOMIC_ACQ_REL, __ATOMIC_ACQUIRE)) {
            __atomic_store_n(&state->throttle_stage, -1, __ATOMIC_RELEASE);
            _exit(122);
        }
        while (__atomic_load_n(&state->throttle_stage, __ATOMIC_ACQUIRE) != 3) {
            __atomic_signal_fence(__ATOMIC_SEQ_CST);
        }
        _exit(0);
    }
    if (!WaitStopped(observer) || !PinTask(observer, cpu)) {
        kill(runner, SIGKILL);
        kill(observer, SIGKILL);
        return 35;
    }

    if (!SetPolicy(0, SCHED_FIFO, 50)) return 36;
    // Queue Fair first, then RT, while the higher-priority coordinator keeps
    // both from running. After the coordinator demotes, only RT throttling can
    // make the observer run; period replenishment must then resume the runner.
    if (kill(observer, SIGCONT) != 0 || kill(runner, SIGCONT) != 0) {
        (void)SetPolicy(0, SCHED_OTHER, 0);
        return 37;
    }
    if (!SetPolicy(0, SCHED_OTHER, 0)) return 38;

    int runner_result = WaitForChild(runner);
    int observer_result = WaitForChild(observer);
    bool completed = __atomic_load_n(&state->throttle_stage, __ATOMIC_ACQUIRE) == 3;
    munmap(state, sizeof(*state));
    return runner_result == 0 && observer_result == 0 && completed ? 0 : 39;
}

int FifoThrottlingScenario() {
    return RealtimeThrottlingScenario(SCHED_FIFO);
}

int RrThrottlingScenario() {
    return RealtimeThrottlingScenario(SCHED_RR);
}

int FifoRemoteScenario() {
    int controller_cpu = -1;
    int worker_cpu = -1;
    if (!PickAllowedCpus(&controller_cpu, &worker_cpu) || worker_cpu < 0 ||
        !PinTask(0, controller_cpu)) {
        return 40;
    }
    SharedFifoState* state = NewSharedFifoState();
    if (state == nullptr) return 41;

    pid_t runner = fork();
    if (runner < 0) return 42;
    if (runner == 0) {
        raise(SIGSTOP);
        __atomic_store_n(&state->remote_runner_started, 1, __ATOMIC_RELEASE);
        while (__atomic_load_n(&state->remote_runner_release, __ATOMIC_ACQUIRE) == 0) {
            __atomic_signal_fence(__ATOMIC_SEQ_CST);
        }
        _exit(0);
    }
    if (!WaitStopped(runner) || !PinTask(runner, worker_cpu) ||
        !SetPolicy(runner, SCHED_FIFO, 10)) {
        kill(runner, SIGKILL);
        return 43;
    }

    pid_t candidate = fork();
    if (candidate < 0) return 44;
    if (candidate == 0) {
        raise(SIGSTOP);
        __atomic_store_n(&state->remote_candidate_ran, 1, __ATOMIC_RELEASE);
        _exit(0);
    }
    if (!WaitStopped(candidate) || !PinTask(candidate, worker_cpu)) {
        kill(runner, SIGKILL);
        kill(candidate, SIGKILL);
        return 45;
    }

    if (kill(runner, SIGCONT) != 0) return 46;
    bool runner_started = false;
    for (int attempt = 0; attempt < 200; ++attempt) {
        if (__atomic_load_n(&state->remote_runner_started, __ATOMIC_ACQUIRE) == 1) {
            runner_started = true;
            break;
        }
        usleep(10 * 1000);
    }
    if (!runner_started || kill(candidate, SIGCONT) != 0) return 47;
    // SIGCONT makes the Fair candidate runnable behind the busy FIFO runner.
    // The remote policy transaction below must be the event that preempts CPU1.
    if (__atomic_load_n(&state->remote_candidate_ran, __ATOMIC_ACQUIRE) != 0 ||
        !ReadPolicyAndPriority(candidate, SCHED_OTHER, 0) ||
        !SetPolicy(candidate, SCHED_FIFO, 20)) {
        return 48;
    }

    bool preempted = false;
    for (int attempt = 0; attempt < 200; ++attempt) {
        if (__atomic_load_n(&state->remote_candidate_ran, __ATOMIC_ACQUIRE) == 1) {
            preempted = true;
            break;
        }
        usleep(10 * 1000);
    }
    __atomic_store_n(&state->remote_runner_release, 1, __ATOMIC_RELEASE);
    int candidate_result = WaitForChild(candidate);
    int runner_result = WaitForChild(runner);
    if (!preempted || candidate_result != 0 || runner_result != 0) return 49;

    pid_t stress = fork();
    if (stress < 0) return 50;
    if (stress == 0) {
        if (!PinTask(0, worker_cpu)) _exit(51);
        __atomic_store_n(&state->stress_started, 1, __ATOMIC_RELEASE);
        while (__atomic_load_n(&state->stress_release, __ATOMIC_ACQUIRE) == 0) {
            __atomic_signal_fence(__ATOMIC_SEQ_CST);
        }
        _exit(0);
    }
    bool stress_started = false;
    for (int attempt = 0; attempt < 200; ++attempt) {
        if (__atomic_load_n(&state->stress_started, __ATOMIC_ACQUIRE) == 1) {
            stress_started = true;
            break;
        }
        usleep(10 * 1000);
    }
    if (!stress_started) return 52;

    for (int round = 0; round < 64; ++round) {
        const int priority = (round & 1) == 0 ? 1 : 99;
        if (!SetPolicy(stress, SCHED_FIFO, priority) ||
            !ReadPolicyAndPriority(stress, SCHED_FIFO, priority) ||
            !SetPolicy(stress, SCHED_OTHER, 0) ||
            !ReadPolicyAndPriority(stress, SCHED_OTHER, 0)) {
            return 53;
        }
    }
    __atomic_store_n(&state->stress_release, 1, __ATOMIC_RELEASE);
    int stress_result = WaitForChild(stress);
    munmap(state, sizeof(*state));
    return stress_result == 0 ? 0 : 54;
}

TEST(SchedFifoBehavior, SamePriorityYieldMovesCurrentToTail) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    EXPECT_EQ(0, RunIsolatedScenario(FifoYieldScenario));
}

TEST(SchedFifoBehavior, RuntimeThrottlingLetsFairRunAndRecovers) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    EXPECT_EQ(0, RunIsolatedScenario(FifoThrottlingScenario));
}

TEST(SchedRrBehavior, SamePriorityQuantumRotates) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(RrRotationScenario));
}

TEST(SchedRrBehavior, DifferentPriorityDoesNotRotate) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(RrPriorityIsolationScenario));
}

TEST(SchedRrBehavior, FifoDoesNotAcquireTimeslicing) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(FifoNoTimesliceScenario));
}

TEST(SchedRrBehavior, FifoAndRrSharePriorityQueue) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(MixedRotationScenario));
}

TEST(SchedRrBehavior, BlockedWakeupJoinsPriorityTail) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(RrBlockedWakeTailScenario));
}

TEST(SchedPolicyChange, ResetOnlyDoesNotMoveQueuedTask) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    EXPECT_EQ(0, RunIsolatedScenario(ResetOnlyKeepsQueueOrderScenario));
}

TEST(SchedRrBehavior, RuntimeThrottlingLetsFairRunAndRecovers) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    EXPECT_EQ(0, RunIsolatedScenario(RrThrottlingScenario));
}

TEST(SchedFifoSmp, RemotePreemptionAndRunningTargetStress) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace FIFO support";
    int first = -1;
    int second = -1;
    ASSERT_TRUE(PickAllowedCpus(&first, &second));
    if (second < 0) GTEST_SKIP() << "requires at least two allowed CPUs";
    EXPECT_EQ(0, RunIsolatedScenario(FifoRemoteScenario));
}

TEST(SchedRrSmp, RemoteRunqueueRotatesSamePriorityTasks) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    int first = -1;
    int second = -1;
    ASSERT_TRUE(PickAllowedCpus(&first, &second));
    if (second < 0) GTEST_SKIP() << "requires at least two allowed CPUs";
    EXPECT_EQ(0, RunIsolatedScenario(RemoteRrRotationScenario));
}

TEST(SchedRrSmp, RunningTaskMigratesToAllowedCpu) {
    if (!IsDragonOS()) GTEST_SKIP() << "requires DragonOS userspace RR support";
    int first = -1;
    int second = -1;
    ASSERT_TRUE(PickAllowedCpus(&first, &second));
    if (second < 0) GTEST_SKIP() << "requires at least two allowed CPUs";
    EXPECT_EQ(0, RunIsolatedScenario(RrAffinityMigrationScenario));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
