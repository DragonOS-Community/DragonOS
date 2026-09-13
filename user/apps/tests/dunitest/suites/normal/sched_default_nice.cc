// Regression coverage for the default nice value and the internal priority
// space (DragonOS issue #2268).
//
// Linux defines the user visible nice range as [-20, 19], which makes
// NICE_WIDTH == 40 and DEFAULT_PRIO == 120. A task that has never been given
// an explicit nice value must therefore report nice 0 in
// /proc/<pid>/stat field 19, and its user mode CPU time must be accounted to
// the `user` column of /proc/stat rather than to `nice`.
//
// Field 18 is the other half of the same ABI: it is `task_prio()`, the
// priority offset into the realtime range, so it reports 20 for a fair task at
// nice 0 rather than the raw internal priority.
//
// Every expectation below is the Linux ABI value; nothing here copies
// DragonOS's current output as the expected value.

#include <gtest/gtest.h>

#include <cerrno>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <pthread.h>
#include <sched.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#ifndef SCHED_RESET_ON_FORK
#define SCHED_RESET_ON_FORK 0x40000000
#endif

namespace {

constexpr long kNiceWidth = 40;
constexpr long kMinNice = -20;
constexpr long kMaxNice = 19;
constexpr long kMaxRtPrio = 100;
constexpr long kDefaultPrio = 120;

struct RawSchedParam {
    int32_t sched_priority;
};

// Use the raw syscall so the kernel ABI is exercised with exactly one i32,
// matching the legacy sched_setscheduler contract.
long RawSetScheduler(pid_t pid, int policy, int32_t priority) {
    RawSchedParam param {priority};
    return syscall(SYS_sched_setscheduler, pid, policy, &param);
}

// Field numbers of /proc/<pid>/stat, 1-based as documented in proc(5).
constexpr int kFieldPriority = 18;
constexpr int kFieldNice = 19;

// Reads the `priority` and `nice` fields of /proc/<pid>/stat.
//
// `comm` is parenthesized and may itself contain spaces and ')', so the field
// index is counted from after the *last* ')' rather than by splitting the
// whole line on whitespace.
bool ReadStatPriorityAndNiceAt(const char* path, long* priority, long* nice) {
    FILE* file = fopen(path, "r");
    if (file == nullptr) return false;

    char line[1024] = {};
    const bool read_ok = fgets(line, sizeof(line), file) != nullptr;
    fclose(file);
    if (!read_ok) return false;

    char* comm_end = strrchr(line, ')');
    if (comm_end == nullptr) return false;

    char* cursor = comm_end + 1;
    while (*cursor == ' ') ++cursor;

    // Field 3 is the single-character process state; skip it so that the
    // remaining tokens are all numeric.
    if (*cursor == '\0' || *cursor == '\n') return false;
    ++cursor;

    // Fields 4..19 inclusive: 16 numeric fields.
    constexpr int kFirstNumericField = 4;
    constexpr int kNumericFieldCount = kFieldNice - kFirstNumericField + 1;
    long values[kNumericFieldCount] = {};

    for (int i = 0; i < kNumericFieldCount; ++i) {
        while (*cursor == ' ') ++cursor;
        if (*cursor == '\0' || *cursor == '\n') return false;

        char* next = nullptr;
        errno = 0;
        const long value = strtol(cursor, &next, 10);
        if (next == cursor || errno != 0) return false;
        values[i] = value;
        cursor = next;
    }

    *priority = values[kFieldPriority - kFirstNumericField];
    *nice = values[kFieldNice - kFirstNumericField];
    return true;
}

bool ReadStatPriorityAndNice(pid_t pid, long* priority, long* nice) {
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/%d/stat", pid);
    return ReadStatPriorityAndNiceAt(path, priority, nice);
}

// Reads the first `/proc/stat` line ("cpu ..."), returning the `user` and
// `nice` columns plus the raw line for diagnostics.
bool ReadAggregateUserNice(uint64_t* user, uint64_t* nice, char* raw, size_t raw_size) {
    FILE* file = fopen("/proc/stat", "r");
    if (file == nullptr) return false;

    if (raw != nullptr && raw_size > 0) {
        raw[0] = '\0';
        if (fgets(raw, static_cast<int>(raw_size), file) == nullptr) {
            fclose(file);
            return false;
        }
        rewind(file);
    }

    char label[8] = {};
    unsigned long long user_ticks = 0;
    unsigned long long nice_ticks = 0;
    const int fields = fscanf(file, "%7s %llu %llu", label, &user_ticks, &nice_ticks);
    fclose(file);

    if (fields != 3 || strcmp(label, "cpu") != 0) return false;
    *user = user_ticks;
    *nice = nice_ticks;
    return true;
}

// Asserts that a freshly created task reports the Linux default nice value.
// Must be called from the test's main thread: fatal assertions are not safe to
// raise from a helper thread.
void ExpectDefaultNice(pid_t pid, const char* what) {
    long priority = 0;
    long nice = 0;
    ASSERT_TRUE(ReadStatPriorityAndNice(pid, &priority, &nice))
        << "failed to read the priority/nice fields of " << what;

    EXPECT_GE(nice, kMinNice) << what << " reports a nice value below Linux MIN_NICE";
    EXPECT_LE(nice, kMaxNice) << what << " reports a nice value above Linux MAX_NICE";
    EXPECT_EQ(0, nice) << what
                       << " must start at the Linux default nice 0 (NICE_WIDTH=" << kNiceWidth
                       << ", DEFAULT_PRIO=120); got nice=" << nice
                       << " (stat field " << kFieldNice << "=" << nice
                       << ", field " << kFieldPriority << "=" << priority << ")";

    // Linux `task_prio()` is `p->prio - MAX_RT_PRIO`, not the nice value, and
    // for a task in the fair class `p->prio == p->static_prio == nice +
    // DEFAULT_PRIO`, so the two fields are always exactly 20 apart. `ps` and
    // `top` print them as the PRI and NI columns; reporting `prio` itself in
    // field 18 makes PRI read back as 120 for an untouched task.
    EXPECT_EQ(nice + kDefaultPrio - kMaxRtPrio, priority)
        << what << " must report task_prio() == nice + 20 in stat field " << kFieldPriority
        << "; got priority=" << priority << " with nice=" << nice;
}

// Child exit codes. The parent maps these back to a message, so a failing case
// never has to rely on sharing memory with the child.
//
// Only kRtUnsupported may be turned into a skip: EPERM genuinely means "this
// environment does not grant CAP_SYS_NICE". Every other non-zero code is a hard
// failure, so a kernel regression in the realtime path cannot be downgraded
// into a green build.
enum ChildExit {
    kOk = 0,
    // 10..19: the SCHED_FIFO -> SCHED_OTHER round trip.
    kRtUnsupported = 11,   // SCHED_FIFO was refused with EPERM
    kFifoSetFailed = 12,   // SCHED_FIFO refused with some other errno
    kFifoNotApplied = 13,  // the call succeeded but the policy did not change
    kOtherSetFailed = 14,  // the switch back to SCHED_OTHER failed
    kOtherNotApplied = 15, // the switch back did not take effect
    // 20: reading /proc/<pid>/stat.
    kStatUnreadable = 20,
    // 30..: the observed nice value was not zero, encoded as 30 + |nice|.
    kNiceNotZeroBase = 30,
};

int NiceExitCode(long nice) {
    const long magnitude = nice < 0 ? -nice : nice;
    return kNiceNotZeroBase + static_cast<int>(magnitude);
}

const char* DescribeChildExit(int code) {
    switch (code) {
        case kRtUnsupported:
            return "SCHED_FIFO was refused with EPERM (no CAP_SYS_NICE)";
        case kFifoSetFailed:
            return "sched_setscheduler(SCHED_FIFO) failed with an errno other than EPERM";
        case kFifoNotApplied:
            return "sched_setscheduler(SCHED_FIFO) succeeded but the policy did not change";
        case kOtherSetFailed:
            return "sched_setscheduler(SCHED_OTHER) failed while leaving SCHED_FIFO";
        case kOtherNotApplied:
            return "sched_setscheduler(SCHED_OTHER) succeeded but the policy did not change";
        case kStatUnreadable:
            return "failed to read the priority/nice fields of /proc/self/stat";
        default:
            return "the task did not report nice 0";
    }
}

struct ThreadObservation {
    pid_t tid;
    long priority;
    long nice;
    bool read_ok;
};

void* ThreadEntry(void* arg) {
    ThreadObservation* observation = static_cast<ThreadObservation*>(arg);
    observation->tid = static_cast<pid_t>(syscall(SYS_gettid));

    // Read through the thread's own task directory, which is the path that is
    // scoped to this specific thread rather than to the thread group leader.
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/self/task/%d/stat", observation->tid);
    observation->read_ok =
        ReadStatPriorityAndNiceAt(path, &observation->priority, &observation->nice);
    return nullptr;
}

}  // namespace

// The reproduction from the issue: a task that never called setpriority() or
// nice() must report nice 0.
TEST(SchedDefaultNice, SelfReportsNiceZero) {
    ExpectDefaultNice(getpid(), "the calling process");
}

// PID 1 is created during boot without any explicit nice adjustment.
TEST(SchedDefaultNice, InitReportsNiceZero) {
    long priority = 0;
    long nice = 0;
    if (!ReadStatPriorityAndNice(1, &priority, &nice)) {
        GTEST_SKIP() << "pid 1 is not visible in this environment";
    }
    ExpectDefaultNice(1, "pid 1");
}

// A thread gets its own PCB, so it must start at the default too rather than
// inheriting a stale offset from the thread group leader.
TEST(SchedDefaultNice, CreatedThreadReportsNiceZero) {
    ThreadObservation observation {};
    pthread_t worker {};
    ASSERT_EQ(0, pthread_create(&worker, nullptr, ThreadEntry, &observation)) << strerror(errno);
    ASSERT_EQ(0, pthread_join(worker, nullptr)) << strerror(errno);

    ASSERT_TRUE(observation.read_ok)
        << "failed to read /proc/self/task/" << observation.tid << "/stat";
    EXPECT_EQ(0, observation.nice)
        << "a newly created thread must start at nice 0; got nice=" << observation.nice
        << " (field " << kFieldNice << "=" << observation.nice << ", field " << kFieldPriority
        << "=" << observation.priority << ")";
}

// fork() must hand the child the Linux default when the parent has never
// changed its nice value.
TEST(SchedDefaultNice, ForkedChildReportsNiceZero) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);

    if (child == 0) {
        long priority = 0;
        long nice = 0;
        if (!ReadStatPriorityAndNice(getpid(), &priority, &nice)) _exit(kStatUnreadable);
        _exit(nice == 0 ? kOk : NiceExitCode(nice));
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status))
        << "the forked child did not report nice 0: " << DescribeChildExit(WEXITSTATUS(status));
}

// Moving a task into a realtime policy and back must not disturb its nice
// value: Linux keeps `static_prio` (the nice-derived value) untouched while a
// task runs under SCHED_FIFO/SCHED_RR.
TEST(SchedDefaultNice, RealtimeRoundTripKeepsNiceZero) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);

    if (child == 0) {
        if (RawSetScheduler(0, SCHED_FIFO, 1) != 0) {
            _exit(errno == EPERM ? kRtUnsupported : kFifoSetFailed);
        }
        if (sched_getscheduler(0) != SCHED_FIFO) _exit(kFifoNotApplied);

        if (RawSetScheduler(0, SCHED_OTHER, 0) != 0) _exit(kOtherSetFailed);
        if (sched_getscheduler(0) != SCHED_OTHER) _exit(kOtherNotApplied);

        long priority = 0;
        long nice = 0;
        if (!ReadStatPriorityAndNice(getpid(), &priority, &nice)) _exit(kStatUnreadable);
        _exit(nice == 0 ? kOk : NiceExitCode(nice));
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));

    const int code = WEXITSTATUS(status);
    // EPERM is the only outcome that legitimately means "realtime scheduling is
    // unavailable here". Any other failure is a regression and must not be
    // reported as a skip.
    if (code == kRtUnsupported) {
        GTEST_SKIP() << DescribeChildExit(code);
    }
    EXPECT_EQ(0, code) << DescribeChildExit(code);
}

// /proc/stat must split user CPU time into `user` and `nice` by the task's
// nice value. With every task at the default nice 0, a spinning process has to
// advance `user` while leaving `nice` untouched.
//
// Both halves are asserted on purpose: before the fix every user tick was
// accounted to `nice` because the default internal priority decoded to nice
// +1, so `user` never advanced at all. A test that only checked "nice did not
// grow" — or that treated the timeout as success — would have passed on the
// broken kernel.
TEST(SchedDefaultNice, SpinningTaskAccountsToUserNotNice) {
    char before_line[256] = {};
    uint64_t user_before = 0;
    uint64_t nice_before = 0;
    ASSERT_TRUE(ReadAggregateUserNice(&user_before, &nice_before, before_line, sizeof(before_line)))
        << "failed to read /proc/stat";

    struct timespec start {};
    ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &start));

    uint64_t user_now = user_before;
    uint64_t nice_now = nice_before;
    char now_line[256] = {};
    volatile uint64_t sink = 0;
    bool timed_out = false;

    while (user_now <= user_before) {
        for (int i = 0; i < 200000; ++i) {
            sink += static_cast<uint64_t>(i);
        }

        ASSERT_TRUE(ReadAggregateUserNice(&user_now, &nice_now, now_line, sizeof(now_line)));

        struct timespec now {};
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &now));
        if (now.tv_sec - start.tv_sec >= 3) {
            timed_out = true;
            break;
        }
    }

    EXPECT_FALSE(timed_out)
        << "the `user` column of /proc/stat did not advance within 3s while spinning; "
        << "before the fix every user tick was accounted as nice time. before: " << before_line
        << "now: " << now_line;
    EXPECT_GT(user_now, user_before)
        << "the `user` column of /proc/stat did not advance while spinning. now: " << now_line;
    EXPECT_EQ(nice_before, nice_now)
        << "a task at the default nice 0 must not accrue `nice` time. before: " << before_line
        << "now: " << now_line;
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
