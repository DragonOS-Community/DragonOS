// Regression coverage for the scheduling fields of /proc/<pid>/stat
// (DragonOS issue #2270).
//
// Linux 6.6 reports 52 fields. Fields 38..41 carry per-task scheduling state:
// the exit signal the task was cloned with, the CPU it last ran on, its user
// visible realtime priority and its policy. DragonOS used to write the CPU
// index into field 38, leave 39..41 at zero and stop after 43 fields, so `ps`,
// `top` and `chrt` read a FIFO task as SCHED_OTHER and misread its placement.
//
// Every expectation below is the Linux ABI value; nothing here copies
// DragonOS's current output as the expected value.

#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <string>
#include <sys/prctl.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

#include <vector>

#ifndef __WCLONE
#define __WCLONE 0x80000000
#endif

namespace {

// Field numbers of /proc/<pid>/stat, 1-based as documented in proc(5).
constexpr int kFieldExitSignal = 38;
constexpr int kFieldProcessor = 39;
constexpr int kFieldRtPriority = 40;
constexpr int kFieldPolicy = 41;

// Linux 6.6 layout: 52 fields in total.
constexpr int kFieldCount = 52;
// Field 4 is the first numeric field; field 3 is the single-character state.
constexpr int kFirstNumericField = 4;

struct RawSchedParam {
    int32_t sched_priority;
};

// Child exit codes of RtPriorityIgnoresNice. `kRtUnsupported` is the only one
// that maps to a skip: it means this environment has no CAP_SYS_NICE, not that
// the kernel rejected a legal request or reported a wrong field.
constexpr int kRtUnsupported = 10;
constexpr int kChildSetupFailed = 11;
constexpr int kChildReadStatFailed = 12;
constexpr int kChildWrongPolicy = 13;
constexpr int kChildWrongRtPriority = 14;
constexpr int kChildWrongPriority = 15;
constexpr int kChildWrongNice = 16;

// Use the raw syscall so the kernel ABI is exercised with exactly one i32,
// matching the legacy sched_setscheduler contract.
long RawSetScheduler(pid_t pid, int policy, int32_t priority) {
    RawSchedParam param {priority};
    return syscall(SYS_sched_setscheduler, pid, policy, &param);
}

pid_t Gettid() { return static_cast<pid_t>(syscall(SYS_gettid)); }

int CurrentCpu() {
    unsigned int cpu = 0;
    return syscall(SYS_getcpu, &cpu, nullptr, nullptr) == 0 ? static_cast<int>(cpu) : -1;
}

struct StatLine {
    // fields[0] is field 4 (`kFirstNumericField`), so field N is
    // `fields[N - kFirstNumericField]`.
    std::vector<long long> fields;
    char state = '\0';
    std::string raw;
};

// Reads one `/proc/<pid>/stat` line.
//
// `comm` is parenthesized and may itself contain spaces and ')', so the fields
// are counted from after the *last* ')' rather than by splitting the whole line
// on whitespace.
bool ReadStatAt(const char* path, StatLine* out) {
    FILE* file = fopen(path, "r");
    if (file == nullptr) return false;

    char line[1024] = {};
    const bool read_ok = fgets(line, sizeof(line), file) != nullptr;
    fclose(file);
    if (!read_ok) return false;

    char* comm_end = strrchr(line, ')');
    if (comm_end == nullptr || comm_end[1] != ' ') return false;

    char* cursor = comm_end + 2;
    if (*cursor == '\0' || *cursor == '\n') return false;
    out->state = *cursor;
    ++cursor;

    out->fields.clear();
    while (true) {
        while (*cursor == ' ') ++cursor;
        if (*cursor == '\0' || *cursor == '\n') break;

        char* next = nullptr;
        errno = 0;
        // Most fields are signed, but field 9 (`flags`) and field 25 (`rsslim`,
        // which is normally RLIM_INFINITY) reach into the unsigned range. Read
        // those unsigned so that no well-formed Linux line is rejected as out
        // of range; the tests only compare small signed fields.
        const long long value = *cursor == '-' ? strtoll(cursor, &next, 10)
                                               : static_cast<long long>(strtoull(cursor, &next, 10));
        if (next == cursor || errno != 0) return false;
        out->fields.push_back(value);
        cursor = next;
    }

    out->raw = line;
    return true;
}

bool ReadStatForThread(pid_t tid, StatLine* out) {
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/self/task/%d/stat", tid);
    return ReadStatAt(path, out);
}

int FieldCount(const StatLine& stat) {
    return static_cast<int>(stat.fields.size()) + kFirstNumericField - 1;
}

long long Field(const StatLine& stat, int number) {
    const int index = number - kFirstNumericField;
    // A missing field would make the cast below an out-of-bounds read, so
    // report it and return a neutral value instead of going out of range.
    if (index < 0 || static_cast<size_t>(index) >= stat.fields.size()) {
        ADD_FAILURE() << "stat has no field " << number << ": " << stat.raw;
        return 0;
    }
    return stat.fields[static_cast<size_t>(index)];
}

// Runs a cleanup action when the enclosing scope ends, including the early
// return of a failed gtest assertion. Without it a worker thread would keep
// running after the test function returned and would dereference a destroyed
// stack object.
template <typename Action>
class ScopeExit {
public:
    explicit ScopeExit(Action action) : action_(action) {}
    ~ScopeExit() { action_(); }

    ScopeExit(const ScopeExit&) = delete;
    ScopeExit& operator=(const ScopeExit&) = delete;

private:
    Action action_;
};

template <typename Action>
ScopeExit<Action> OnScopeExit(Action action) {
    return ScopeExit<Action>(action);
}

// A worker thread that names itself and optionally adopts a realtime policy,
// publishes its tid and then sleeps until the test releases it.
struct WorkerState {
    const char* name = nullptr;
    int policy = SCHED_OTHER;
    int priority = 0;
    std::atomic<pid_t> tid {-1};
    // 0 while the requested policy was applied; otherwise the errno of the
    // failed sched_setscheduler(), which lets the test tell "no CAP_SYS_NICE"
    // (skip) from "the kernel rejected a legal request" (fail).
    std::atomic<int> policy_errno {0};
    std::atomic<bool> stop {false};
};

void* WorkerMain(void* arg) {
    WorkerState* state = static_cast<WorkerState*>(arg);

    if (state->name != nullptr) {
        prctl(PR_SET_NAME, state->name, 0, 0, 0);
    }
    if (state->policy != SCHED_OTHER &&
        RawSetScheduler(0, state->policy, state->priority) != 0) {
        state->policy_errno.store(errno);
    }
    state->tid.store(Gettid());

    while (!state->stop.load(std::memory_order_relaxed)) {
        usleep(1000);
    }
    return nullptr;
}

// Waits until `state.tid` is published, i.e. the worker finished setting its
// name/policy.
bool WaitForWorker(WorkerState* state, int timeout_ms = 5000) {
    for (int attempt = 0; attempt < timeout_ms; ++attempt) {
        if (state->tid.load() > 0) return true;
        usleep(1000);
    }
    return false;
}

// A worker thread pinned to one CPU, spinning so that its current CPU is
// observable from the test process.
struct CpuSpinState {
    int cpu = -1;
    std::atomic<int> observed {-1};
    std::atomic<pid_t> tid {-1};
    std::atomic<bool> stop {false};
};

void* CpuSpinMain(void* arg) {
    CpuSpinState* state = static_cast<CpuSpinState*>(arg);

    cpu_set_t mask;
    CPU_ZERO(&mask);
    CPU_SET(state->cpu, &mask);
    if (sched_setaffinity(0, sizeof(mask), &mask) != 0) return nullptr;

    state->tid.store(Gettid());
    while (!state->stop.load(std::memory_order_relaxed)) {
        state->observed.store(CurrentCpu(), std::memory_order_relaxed);
    }
    return nullptr;
}

// Returns the first two CPUs this process is allowed to run on.
bool PickTwoAllowedCpus(int* first, int* second) {
    cpu_set_t allowed;
    CPU_ZERO(&allowed);
    if (sched_getaffinity(0, sizeof(allowed), &allowed) != 0) return false;

    *first = -1;
    *second = -1;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (!CPU_ISSET(cpu, &allowed)) continue;
        if (*first < 0) {
            *first = cpu;
        } else {
            *second = cpu;
            break;
        }
    }
    return *first >= 0 && *second >= 0;
}

}  // namespace

// The line must have the full Linux 6.6 layout on both the process and the
// thread path, and it must stay a single well-formed line.
TEST(ProcStatSchedFields, LayoutMatchesLinux) {
    StatLine process {};
    ASSERT_TRUE(ReadStatAt("/proc/self/stat", &process));
    EXPECT_EQ(kFieldCount, FieldCount(process));

    StatLine thread {};
    ASSERT_TRUE(ReadStatForThread(Gettid(), &thread));
    EXPECT_EQ(kFieldCount, FieldCount(thread));

    const StatLine* lines[] = {&process, &thread};
    for (const StatLine* stat : lines) {
        const std::string& raw = stat->raw;
        ASSERT_FALSE(raw.empty());
        EXPECT_EQ('\n', raw[raw.size() - 1]) << "stat line must end with a newline";
        EXPECT_EQ(raw.find('\n'), raw.size() - 1) << "stat must be a single line";
        EXPECT_EQ(std::string::npos, raw.find("  "))
            << "stat fields must be separated by single spaces: " << raw;
        EXPECT_EQ(std::string::npos, raw.find(" \n"))
            << "stat must not end with a trailing space: " << raw;
    }
}

// Linux emits 0 for these fields unconditionally, which makes them position
// anchors: inserting or dropping any earlier field moves them and fails here.
TEST(ProcStatSchedFields, ConstantFieldsStayZero) {
    StatLine stat {};
    ASSERT_TRUE(ReadStatAt("/proc/self/stat", &stat));

    EXPECT_EQ(0, Field(stat, 21)) << "field 21 (itrealvalue) is always 0 in Linux";
    EXPECT_EQ(0, Field(stat, 36)) << "field 36 (nswap) is always 0 in Linux";
    EXPECT_EQ(0, Field(stat, 37)) << "field 37 (cnswap) is always 0 in Linux";
}

// Field 38 is `task_struct::exit_signal`: the signal delivered to the parent on
// exit, -1 for a thread group member, and unchanged once the task has exited.
TEST(ProcStatSchedFields, ExitSignalFollowsCloneFlags) {
    // The thread group leader was created by fork/vfork, i.e. with SIGCHLD.
    StatLine leader {};
    ASSERT_TRUE(ReadStatAt("/proc/self/stat", &leader));
    EXPECT_EQ(SIGCHLD, Field(leader, kFieldExitSignal));

    // A non-leader thread is created with CLONE_THREAD and carries -1. The
    // caller's own tid *is* this thread group's leader, so a worker thread is
    // needed to observe the thread case.
    WorkerState worker {};
    pthread_t thread_id;
    const int create_error = pthread_create(&thread_id, nullptr, WorkerMain, &worker);
    ASSERT_EQ(0, create_error) << strerror(create_error);
    auto join_worker = OnScopeExit([&] {
        worker.stop.store(true);
        pthread_join(thread_id, nullptr);
    });

    ASSERT_TRUE(WaitForWorker(&worker)) << "worker did not publish its tid";

    StatLine thread {};
    ASSERT_TRUE(ReadStatForThread(worker.tid.load(), &thread));
    EXPECT_EQ(-1, Field(thread, kFieldExitSignal));

    // A clone child carries the signal encoded in the low byte of `flags`. The
    // notification is ignored here so that the test process is not terminated
    // by its own child; the disposition must stay ignored until the child is
    // reaped, hence the scope guard instead of a plain restore.
    struct sigaction ignore {};
    struct sigaction previous {};
    ignore.sa_handler = SIG_IGN;
    sigemptyset(&ignore.sa_mask);
    ASSERT_EQ(0, sigaction(SIGUSR1, &ignore, &previous)) << strerror(errno);
    auto restore_sigusr1 = OnScopeExit([&] { sigaction(SIGUSR1, &previous, nullptr); });

    const long child = syscall(SYS_clone, SIGUSR1, nullptr, nullptr, nullptr, 0);
    ASSERT_GE(child, 0) << "clone(SIGUSR1) failed: " << strerror(errno);
    if (child == 0) {
        _exit(0);
    }

    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/%ld/stat", child);
    StatLine zombie {};
    bool zombie_visible = false;
    for (int attempt = 0; attempt < 5000; ++attempt) {
        if (ReadStatAt(path, &zombie) && zombie.state == 'Z') {
            zombie_visible = true;
            break;
        }
        usleep(1000);
    }
    EXPECT_TRUE(zombie_visible) << "clone child did not become a visible zombie";
    if (zombie_visible) {
        EXPECT_EQ(SIGUSR1, Field(zombie, kFieldExitSignal))
            << "an exited task must keep the exit signal it was cloned with";
    }

    // The child is only waitable as a clone child.
    EXPECT_GT(waitpid(static_cast<pid_t>(child), nullptr, __WCLONE), 0) << strerror(errno);
}

// Field 39 is `task_cpu(task)`: the CPU the task last ran on. The worker is
// pinned to a second CPU and reports its own CPU with getcpu(), which witnesses
// that the reported processor is the real placement.
TEST(ProcStatSchedFields, ProcessorFollowsTaskCpu) {
    int first = -1;
    int second = -1;
    if (!PickTwoAllowedCpus(&first, &second)) {
        GTEST_SKIP() << "requires at least two CPUs in the affinity mask";
    }

    CpuSpinState state {};
    state.cpu = second;
    pthread_t thread;
    const int create_error = pthread_create(&thread, nullptr, CpuSpinMain, &state);
    ASSERT_EQ(0, create_error) << strerror(create_error);
    auto join_worker = OnScopeExit([&] {
        state.stop.store(true);
        pthread_join(thread, nullptr);
    });

    bool observed = false;
    for (int attempt = 0; attempt < 5000; ++attempt) {
        if (state.tid.load() > 0 && state.observed.load() == second) {
            observed = true;
            break;
        }
        usleep(1000);
    }

    if (observed) {
        StatLine stat {};
        ASSERT_TRUE(ReadStatForThread(state.tid.load(), &stat));
        EXPECT_EQ(second, Field(stat, kFieldProcessor))
            << "field 39 must report the CPU the task was placed on";
    }

    // A worker that never published its tid returned early because
    // sched_setaffinity() failed; say so instead of reporting a timeout.
    ASSERT_GT(state.tid.load(), 0) << "worker could not pin itself to CPU " << second;
    EXPECT_TRUE(observed) << "worker never reported running on CPU " << second;
}

// Fields 40/41 (and 18, the encoded priority) follow the scheduling policy,
// and field 19 keeps the nice value across every policy change.
TEST(ProcStatSchedFields, RtFieldsFollowPolicy) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to set a realtime policy";
    }

    // This test rewrites the policy and the nice value of the test process
    // itself, so snapshot them first and restore them on every exit path,
    // including the early return of a failed assertion: leaving the process in
    // SCHED_FIFO would change how the remaining tests of this binary are
    // scheduled and would make RtFieldsArePerThread see a realtime leader.
    const int original_policy = sched_getscheduler(0);
    ASSERT_GE(original_policy, 0) << strerror(errno);
    struct sched_param original_param {};
    ASSERT_EQ(0, sched_getparam(0, &original_param)) << strerror(errno);
    errno = 0;
    const int original_nice = getpriority(PRIO_PROCESS, 0);
    ASSERT_EQ(0, errno) << strerror(errno);
    auto restore_scheduling = OnScopeExit([&] {
        RawSetScheduler(0, original_policy, original_param.sched_priority);
        setpriority(PRIO_PROCESS, 0, original_nice);
    });

    // Entering a realtime policy doubles as the capability probe: raising the
    // scheduling priority and lowering the nice value both need CAP_SYS_NICE,
    // so a skip here is the only case where the nice change below may fail.
    if (RawSetScheduler(0, SCHED_FIFO, 1) != 0) {
        const int error = errno;
        ASSERT_EQ(EPERM, error)
            << "sched_setscheduler(SCHED_FIFO, 1) failed with " << strerror(error)
            << "; only EPERM means this environment lacks CAP_SYS_NICE";
        GTEST_SKIP() << "requires CAP_SYS_NICE: " << strerror(error);
    }

    // Give the realtime task a non-zero nice value: the legacy
    // sched_setscheduler() must carry it across every further policy change, so
    // the expected value of field 19 is observable instead of assuming the
    // ambient nice happens to be 0. Linux keeps updating `static_prio` for
    // realtime tasks as well (`set_user_nice()` only returns early for
    // SCHED_DEADLINE), so a nice change must not move field 18.
    const int kNice = -3;
    ASSERT_EQ(0, setpriority(PRIO_PROCESS, 0, kNice)) << strerror(errno);

    StatLine fifo {};
    ASSERT_TRUE(ReadStatAt("/proc/self/stat", &fifo));
    EXPECT_EQ(SCHED_FIFO, Field(fifo, kFieldPolicy));
    EXPECT_EQ(1, Field(fifo, kFieldRtPriority));
    // Linux `task_prio()` is `-1 - rt_priority` for a realtime task, and the
    // nice value is untouched by the policy change.
    EXPECT_EQ(-2, Field(fifo, 18));
    EXPECT_EQ(kNice, Field(fifo, 19));

    ASSERT_EQ(0, RawSetScheduler(0, SCHED_RR, 2)) << strerror(errno);
    StatLine rr {};
    ASSERT_TRUE(ReadStatAt("/proc/self/stat", &rr));
    EXPECT_EQ(SCHED_RR, Field(rr, kFieldPolicy));
    EXPECT_EQ(2, Field(rr, kFieldRtPriority));
    EXPECT_EQ(-3, Field(rr, 18));
    EXPECT_EQ(kNice, Field(rr, 19));

    ASSERT_EQ(0, RawSetScheduler(0, SCHED_OTHER, 0)) << strerror(errno);
    StatLine other {};
    ASSERT_TRUE(ReadStatAt("/proc/self/stat", &other));
    EXPECT_EQ(SCHED_OTHER, Field(other, kFieldPolicy));
    EXPECT_EQ(0, Field(other, kFieldRtPriority));
    // Going back to a fair policy must keep the nice value too:
    // `_sched_setscheduler()` builds its attribute as
    // `.sched_nice = PRIO_TO_NICE(p->static_prio)`, i.e. it reuses the fair
    // priority the task had before it became realtime (verified on Linux 6.6:
    // nice -5 + FIFO 1 -> SCHED_OTHER still reports field 19 == -5).
    EXPECT_EQ(kNice, Field(other, 19));
    EXPECT_EQ(kNice + 20, Field(other, 18));
}

// A realtime task keeps its effective priority when its nice value changes, so
// field 40 and field 18 must not move, while field 19 must report the nice.
TEST(ProcStatSchedFields, RtPriorityIgnoresNice) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to set a realtime policy";
    }

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        // Run in a child so that the test process keeps its own nice value.
        if (RawSetScheduler(0, SCHED_FIFO, 1) != 0) {
            _exit(errno == EPERM ? kRtUnsupported : kChildSetupFailed);
        }
        if (setpriority(PRIO_PROCESS, 0, -5) != 0) _exit(kChildSetupFailed);

        // gtest failures are lost in a forked child, so every check below is a
        // plain comparison with its own exit code. Keep the expected values
        // non-zero: a missing field is reported as 0 by Field(), and a
        // zero-valued expectation would silently accept that.
        StatLine stat {};
        if (!ReadStatAt("/proc/self/stat", &stat)) _exit(kChildReadStatFailed);
        if (Field(stat, kFieldPolicy) != SCHED_FIFO) _exit(kChildWrongPolicy);
        if (Field(stat, kFieldRtPriority) != 1) _exit(kChildWrongRtPriority);
        if (Field(stat, 18) != -2) _exit(kChildWrongPriority);
        if (Field(stat, 19) != -5) _exit(kChildWrongNice);
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "child did not exit normally: " << status;

    const int code = WEXITSTATUS(status);
    if (code == kRtUnsupported) {
        GTEST_SKIP() << "requires CAP_SYS_NICE to set SCHED_FIFO";
    }
    EXPECT_EQ(0, code) << "child observed a wrong stat field (step " << code << ")";
}

// Fields 38..41 are per task, so the thread path must describe the thread and
// not its thread group leader.
TEST(ProcStatSchedFields, RtFieldsArePerThread) {
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to set a realtime policy";
    }

    WorkerState worker {};
    worker.policy = SCHED_FIFO;
    worker.priority = 3;

    pthread_t thread;
    const int create_error = pthread_create(&thread, nullptr, WorkerMain, &worker);
    ASSERT_EQ(0, create_error) << strerror(create_error);
    auto join_worker = OnScopeExit([&] {
        worker.stop.store(true);
        pthread_join(thread, nullptr);
    });

    ASSERT_TRUE(WaitForWorker(&worker)) << "worker did not publish its tid";

    const int worker_error = worker.policy_errno.load();
    if (worker_error != 0) {
        ASSERT_EQ(EPERM, worker_error)
            << "sched_setscheduler(SCHED_FIFO, 3) in a thread failed with "
            << strerror(worker_error) << "; only EPERM means this environment lacks CAP_SYS_NICE";
        GTEST_SKIP() << "requires CAP_SYS_NICE: " << strerror(worker_error);
    }

    StatLine thread_stat {};
    ASSERT_TRUE(ReadStatForThread(worker.tid.load(), &thread_stat));
    EXPECT_EQ(SCHED_FIFO, Field(thread_stat, kFieldPolicy));
    EXPECT_EQ(3, Field(thread_stat, kFieldRtPriority));
    EXPECT_EQ(-4, Field(thread_stat, 18));

    StatLine leader_stat {};
    ASSERT_TRUE(ReadStatAt("/proc/self/stat", &leader_stat));
    EXPECT_EQ(SCHED_OTHER, Field(leader_stat, kFieldPolicy));
    EXPECT_EQ(0, Field(leader_stat, kFieldRtPriority));
}

// A command name containing spaces must not shift the scheduling fields.
TEST(ProcStatSchedFields, CommWithSpacesKeepsPositions) {
    WorkerState worker {};
    worker.name = "ab cd ef";

    pthread_t thread;
    const int create_error = pthread_create(&thread, nullptr, WorkerMain, &worker);
    ASSERT_EQ(0, create_error) << strerror(create_error);
    auto join_worker = OnScopeExit([&] {
        worker.stop.store(true);
        pthread_join(thread, nullptr);
    });

    ASSERT_TRUE(WaitForWorker(&worker)) << "worker did not publish its tid";

    StatLine stat {};
    ASSERT_TRUE(ReadStatForThread(worker.tid.load(), &stat));
    EXPECT_NE(std::string::npos, stat.raw.find("(ab cd ef)")) << stat.raw;
    EXPECT_EQ(kFieldCount, FieldCount(stat));
    EXPECT_EQ(-1, Field(stat, kFieldExitSignal));
    EXPECT_EQ(SCHED_OTHER, Field(stat, kFieldPolicy));
    EXPECT_EQ(0, Field(stat, kFieldRtPriority));
    EXPECT_GE(Field(stat, kFieldProcessor), 0);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
