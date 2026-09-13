// Regression coverage for setpriority(2)/getpriority(2) and the priority
// values they publish (DragonOS issue #2268).
//
// Issue #2268's remaining acceptance checks needed these two syscalls, and
// DragonOS answered them with ENOSYS. This suite makes those checks
// executable: the pair has to exist, return the documented errno for each
// documented failure, clamp an out-of-range nice value instead of rejecting
// it, honour RLIMIT_NICE, and agree with the `priority`/`nice` fields of
// /proc/<pid>/stat.
//
// Every expectation below is the Linux ABI value; nothing here copies
// DragonOS's current output as the expected value.
//
// Every call that mutates a nice value runs in a forked child. The suite
// therefore never re-nices the test process itself, and the PRIO_PGRP and
// PRIO_USER cases are scoped to a process group or uid that contains nothing
// but that child, so no other task in the guest is affected either.

#include <gtest/gtest.h>

#include <pthread.h>
#include <sched.h>

#include <atomic>
#include <cerrno>
#include <climits>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

constexpr int kMinNice = -20;
constexpr int kMaxNice = 19;
constexpr int kMaxRtPrio = 100;
constexpr int kDefaultPrio = 120;

// Field numbers of /proc/<pid>/stat, 1-based as documented in proc(5).
constexpr int kFieldPriority = 18;
constexpr int kFieldNice = 19;

// A uid that no task in the test environment holds, used for the
// "selects nothing" case of PRIO_USER.
constexpr uid_t kUnusedUid = 0x5A5A;

// The nice values the PRIO_PGRP and PRIO_USER selectors are asked to apply.
// The second is strictly above the first because a nice value *below* the
// current one is a priority raise: the selector case has already dropped to an
// unprivileged uid by then, so a raise would be refused by the privilege rule
// rather than exercised by the selector under test.
constexpr int kSelectorPgrpNice = 7;
constexpr int kSelectorUserNice = 9;

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

// The raw syscall behind getpriority(3). It returns the RLIMIT_NICE style
// encoding `MAX_NICE - nice + 1`, not the nice value, which is why libc's
// wrapper subtracts the result from 20. Every encoded value is >= 1, so a `-1`
// return is unambiguously an error rather than a legitimate value.
long RawGetpriority(int which, int who, int* err) {
    errno = 0;
    const long value = syscall(SYS_getpriority, which, who);
    *err = errno;
    return value;
}

// The raw syscall behind setpriority(3); returns 0 or -1.
long RawSetpriority(int which, int who, int nice_value, int* err) {
    errno = 0;
    const long value = syscall(SYS_setpriority, which, who, nice_value);
    *err = errno;
    return value;
}

bool WriteAll(int fd, const void* value, size_t size) {
    const char* cursor = static_cast<const char*>(value);
    size_t written = 0;
    while (written < size) {
        const ssize_t n = write(fd, cursor + written, size - written);
        if (n < 0) {
            if (errno == EINTR) continue;
            return false;
        }
        if (n == 0) return false;
        written += static_cast<size_t>(n);
    }
    return true;
}

bool ReadAll(int fd, void* value, size_t size) {
    char* cursor = static_cast<char*>(value);
    size_t read_bytes = 0;
    while (read_bytes < size) {
        const ssize_t n = read(fd, cursor + read_bytes, size - read_bytes);
        if (n < 0) {
            if (errno == EINTR) continue;
            return false;
        }
        if (n == 0) return false;
        read_bytes += static_cast<size_t>(n);
    }
    return true;
}

// Runs `body` in a forked child and copies the single `Result` it returns back
// through a pipe. The child reports raw observations rather than a pass/fail
// verdict, so the parent — which still holds its own privileges — does all the
// asserting and can report exactly which expectation failed.
//
// A child that dies before writing, or that writes a partial record, makes this
// fail; the caller then reports a failed ASSERT rather than asserting on a
// zero-initialized result.
template <typename Result, typename Body>
::testing::AssertionResult RunInChild(const Body& body, Result* out) {
    int fds[2] = {-1, -1};
    if (pipe(fds) != 0) {
        return ::testing::AssertionFailure() << "pipe() failed: " << strerror(errno);
    }

    const pid_t child = fork();
    if (child < 0) {
        const int saved = errno;
        close(fds[0]);
        close(fds[1]);
        return ::testing::AssertionFailure() << "fork() failed: " << strerror(saved);
    }

    if (child == 0) {
        close(fds[0]);
        const Result result = body();
        const bool written = WriteAll(fds[1], &result, sizeof(result));
        close(fds[1]);
        // Never run a gtest assertion in the child: `_exit` keeps it from
        // touching the parent's test state or flushing its output twice.
        _exit(written ? 0 : 1);
    }

    close(fds[1]);
    const bool read_ok = ReadAll(fds[0], out, sizeof(*out));
    close(fds[0]);

    int status = 0;
    if (waitpid(child, &status, 0) != child) {
        return ::testing::AssertionFailure() << "waitpid() failed: " << strerror(errno);
    }
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0) {
        return ::testing::AssertionFailure()
               << "the child exited with status " << status
               << " instead of reporting its observations";
    }
    if (!read_ok) {
        return ::testing::AssertionFailure() << "the child's observations were truncated";
    }
    return ::testing::AssertionSuccess();
}

// A RunInChild() call cannot be spelled directly inside ASSERT_TRUE: the
// preprocessor would read the comma before the out-parameter as a second macro
// argument. Every call site therefore does
//
//     const ::testing::AssertionResult child_ran = RunInChild(body, &out);
//     ASSERT_TRUE(child_ran) << "…";
//
// so that the observation is only asserted on once the child has really
// reported it.

// ---------------------------------------------------------------------------
// setpriority()/getpriority() round trip and procfs agreement.
// ---------------------------------------------------------------------------

struct RoundTripObservation {
    int set_errno;
    long raw_value;
    int raw_errno;
    int libc_value;
    int libc_errno;
    long stat_priority;
    long stat_nice;
    bool stat_ok;
};

RoundTripObservation ObserveNiceRoundTrip(int nice_value) {
    RoundTripObservation observation {};
    if (RawSetpriority(PRIO_PROCESS, 0, nice_value, &observation.set_errno) != 0) {
        return observation;
    }

    observation.raw_value = RawGetpriority(PRIO_PROCESS, 0, &observation.raw_errno);
    errno = 0;
    observation.libc_value = getpriority(PRIO_PROCESS, 0);
    observation.libc_errno = errno;
    observation.stat_ok =
        ReadStatPriorityAndNice(getpid(), &observation.stat_priority, &observation.stat_nice);
    return observation;
}

// A nice value round trips through every layer that exposes it: the raw
// syscall's encoded result, libc's decoded result, and the two procfs fields.
void ExpectNiceRoundTrip(int nice_value) {
    RoundTripObservation observation {};
    const ::testing::AssertionResult child_ran =
        RunInChild([nice_value] { return ObserveNiceRoundTrip(nice_value); }, &observation);
    ASSERT_TRUE(child_ran) << "for nice " << nice_value;

    ASSERT_EQ(0, observation.set_errno)
        << "setpriority(PRIO_PROCESS, 0, " << nice_value << "): "
        << strerror(observation.set_errno);

    // `MAX_NICE - nice + 1`: this encoding is what libc's wrapper reverses, so
    // asserting only on the decoded value would not pin the ABI.
    EXPECT_EQ(kMaxNice - nice_value + 1, observation.raw_value)
        << "getpriority(PRIO_PROCESS, 0) must return the RLIMIT_NICE encoding (errno "
        << observation.raw_errno << ")";
    EXPECT_EQ(nice_value, observation.libc_value)
        << "getpriority(3) must return the nice value itself (errno " << observation.libc_errno
        << ")";

    ASSERT_TRUE(observation.stat_ok) << "failed to read /proc/self/stat";
    EXPECT_EQ(nice_value, observation.stat_nice)
        << "stat field " << kFieldNice << " must report the new nice value";
    // `task_prio()` is `prio - MAX_RT_PRIO`, and a fair task has `prio ==
    // static_prio == nice + DEFAULT_PRIO`, so the priority field tracks the
    // nice value with a fixed offset of 20.
    EXPECT_EQ(nice_value + kDefaultPrio - kMaxRtPrio, observation.stat_priority)
        << "stat field " << kFieldPriority << " must report nice + 20";
}

TEST(Setpriority, RoundTripMatchesProcfs) {
    ExpectNiceRoundTrip(12);
    ExpectNiceRoundTrip(-12);
    ExpectNiceRoundTrip(kMaxNice);
    ExpectNiceRoundTrip(kMinNice);
    ExpectNiceRoundTrip(0);
}

struct ClampObservation {
    int set_errno;
    long raw_value;
    int raw_errno;
    int libc_value;
    int libc_errno;
};

ClampObservation ObserveClamp(int requested) {
    ClampObservation observation {};
    if (RawSetpriority(PRIO_PROCESS, 0, requested, &observation.set_errno) != 0) {
        return observation;
    }

    observation.raw_value = RawGetpriority(PRIO_PROCESS, 0, &observation.raw_errno);
    errno = 0;
    observation.libc_value = getpriority(PRIO_PROCESS, 0);
    observation.libc_errno = errno;
    return observation;
}

// Linux normalizes rather than rejects: a `niceval` below MIN_NICE becomes
// MIN_NICE and one above MAX_NICE becomes MAX_NICE, and the call still
// succeeds. A setpriority() that returned EINVAL here would break `nice(1)`,
// which relies on the kernel to do the clamping.
void ExpectClamp(int requested, int expected) {
    ClampObservation observation {};
    const ::testing::AssertionResult child_ran =
        RunInChild([requested] { return ObserveClamp(requested); }, &observation);
    ASSERT_TRUE(child_ran) << "for nice " << requested;

    EXPECT_EQ(0, observation.set_errno)
        << "setpriority(PRIO_PROCESS, 0, " << requested << ") must clamp, not fail: "
        << strerror(observation.set_errno);
    EXPECT_EQ(expected, observation.libc_value)
        << "setpriority(PRIO_PROCESS, 0, " << requested << ") must clamp to " << expected
        << " (errno " << observation.libc_errno << ")";
    EXPECT_EQ(kMaxNice - expected + 1, observation.raw_value)
        << "the clamped value must be what getpriority() reports (errno " << observation.raw_errno
        << ")";
}

TEST(Setpriority, OutsideRangeIsClampedNotRejected) {
    ExpectClamp(kMaxNice + 1, kMaxNice);
    ExpectClamp(kMinNice - 1, kMinNice);
    ExpectClamp(INT_MAX, kMaxNice);
    ExpectClamp(INT_MIN, kMinNice);
}

struct ForkInheritanceObservation {
    int setup_errno;
    int set_errno;
    long parent_raw;
    long child_raw;
    bool child_reported;
    bool child_stat_ok;
    long child_stat_priority;
    long child_stat_nice;
    int child_status;
};

// A task that raises its own nice value must hand that value to the children it
// forks: the child's scheduler state is copied from the parent's, so the
// default-0 initialization from #2268 must not overwrite it. This is the
// acceptance check the issue spells as "preservation of valid parent nice
// settings across fork/clone".
//
// The child reports its own encoded value and then blocks until the parent has
// read its /proc entry, so the task's own view and the externally visible stat
// fields describe the same live child rather than two samples that a
// rescheduling could separate.
constexpr int kInheritedNice = 7;

ForkInheritanceObservation ObserveForkInheritance() {
    ForkInheritanceObservation observation {};
    if (RawSetpriority(PRIO_PROCESS, 0, kInheritedNice, &observation.set_errno) != 0) {
        return observation;
    }

    int scratch = 0;
    observation.parent_raw = RawGetpriority(PRIO_PROCESS, 0, &scratch);

    int report[2] = {-1, -1};
    int release[2] = {-1, -1};
    if (pipe(report) != 0 || pipe(release) != 0) {
        observation.setup_errno = errno;
        return observation;
    }

    const pid_t child = fork();
    if (child < 0) {
        observation.setup_errno = errno;
        return observation;
    }

    if (child == 0) {
        close(report[0]);
        close(release[1]);
        const long own = RawGetpriority(PRIO_PROCESS, 0, &scratch);
        char byte = 0;
        if (!WriteAll(report[1], &own, sizeof(own))) _exit(1);
        if (read(release[0], &byte, 1) != 1) _exit(2);
        _exit(0);
    }

    close(report[1]);
    close(release[0]);
    observation.child_reported =
        ReadAll(report[0], &observation.child_raw, sizeof(observation.child_raw));
    observation.child_stat_ok = ReadStatPriorityAndNice(
        child, &observation.child_stat_priority, &observation.child_stat_nice);

    const char done = 1;
    WriteAll(release[1], &done, 1);
    close(report[0]);
    close(release[1]);

    if (waitpid(child, &observation.child_status, 0) != child) {
        observation.setup_errno = errno;
    }
    return observation;
}

TEST(Setpriority, ForkedChildInheritsParentsNice) {
    ForkInheritanceObservation observation {};
    const ::testing::AssertionResult child_ran =
        RunInChild([] { return ObserveForkInheritance(); }, &observation);
    ASSERT_TRUE(child_ran);

    ASSERT_EQ(0, observation.setup_errno) << strerror(observation.setup_errno);
    ASSERT_EQ(0, observation.set_errno)
        << "setpriority(PRIO_PROCESS, 0, " << kInheritedNice << "): "
        << strerror(observation.set_errno);

    EXPECT_EQ(kMaxNice - kInheritedNice + 1, observation.parent_raw)
        << "the forking task must have taken the nice value it asked for";

    ASSERT_TRUE(observation.child_reported)
        << "the forked child died before reporting its own nice value";
    EXPECT_EQ(kMaxNice - kInheritedNice + 1, observation.child_raw)
        << "a forked child must inherit its parent's nice value, not the default";

    ASSERT_TRUE(observation.child_stat_ok)
        << "failed to read the forked child's /proc/<pid>/stat";
    EXPECT_EQ(kInheritedNice, observation.child_stat_nice)
        << "stat field " << kFieldNice << " of the child must report the inherited nice value";
    EXPECT_EQ(kInheritedNice + kDefaultPrio - kMaxRtPrio, observation.child_stat_priority)
        << "stat field " << kFieldPriority << " of the child must report its inherited nice + 20";

    EXPECT_TRUE(WIFEXITED(observation.child_status) &&
                WEXITSTATUS(observation.child_status) == 0)
        << "the forked child exited with status " << observation.child_status;
}

struct NiceWrapperObservation {
    int initial_nice;
    int initial_errno;
    int ret;
    int err;
    int observed;
    int observed_errno;
};

// `nice(3)` is a libc routine rather than a syscall: glibc implements it on top
// of getpriority() and setpriority(). It is the call most programs actually
// use, which is why issue #2268 could not be fixed by adding a DragonOS-only
// SYS_nice — no architecture this kernel builds for has one.
NiceWrapperObservation ObserveNiceWrapper(int inc) {
    NiceWrapperObservation observation {};
    errno = 0;
    observation.initial_nice = getpriority(PRIO_PROCESS, 0);
    observation.initial_errno = errno;

    errno = 0;
    observation.ret = nice(inc);
    observation.err = errno;

    errno = 0;
    observation.observed = getpriority(PRIO_PROCESS, 0);
    observation.observed_errno = errno;
    return observation;
}

// Starting from nice 0, `nice(inc)` returns the resulting nice value with the
// sum clamped to the Linux range.
TEST(Nice, WrapperAddsAndClamps) {
    struct Case {
        int inc;
        int expected;
    };
    // Each case runs in a fresh child, so every one of them starts at nice 0.
    const Case cases[] = {
        {5, 5}, {-3, -3}, {kMaxNice, kMaxNice}, {100, kMaxNice}, {-100, kMinNice}, {0, 0}};

    for (const Case& test_case : cases) {
        NiceWrapperObservation observation {};
        const ::testing::AssertionResult child_ran =
            RunInChild([inc = test_case.inc] { return ObserveNiceWrapper(inc); }, &observation);
        ASSERT_TRUE(child_ran) << "for inc " << test_case.inc;

        ASSERT_EQ(0, observation.initial_errno) << "getpriority() at the start of the child";
        ASSERT_EQ(0, observation.initial_nice)
            << "the child must start at the Linux default nice 0";

        EXPECT_EQ(test_case.expected, observation.ret)
            << "nice(" << test_case.inc << ") must return the resulting nice value (errno "
            << observation.err << ")";
        EXPECT_EQ(test_case.expected, observation.observed)
            << "getpriority() after nice(" << test_case.inc << ") (errno "
            << observation.observed_errno << ")";
    }
}

// ---------------------------------------------------------------------------
// Error paths.
// ---------------------------------------------------------------------------

struct ErrnoObservation {
    int ret_errno;
    bool succeeded;
};

ErrnoObservation ObserveGetpriority(int which, int who) {
    ErrnoObservation observation {};
    int err = 0;
    const long value = RawGetpriority(which, who, &err);
    observation.succeeded = value != -1;
    observation.ret_errno = err;
    return observation;
}

ErrnoObservation ObserveSetpriority(int which, int who, int nice_value) {
    ErrnoObservation observation {};
    int err = 0;
    observation.succeeded = RawSetpriority(which, who, nice_value, &err) == 0;
    observation.ret_errno = err;
    return observation;
}

// `which` outside [PRIO_PROCESS, PRIO_USER] is EINVAL, and Linux tests it
// before it looks at `who` or at the nice value.
void ExpectWhichEinval(int which) {
    ErrnoObservation get {};
    const ::testing::AssertionResult get_ran =
        RunInChild([which] { return ObserveGetpriority(which, 0); }, &get);
    ASSERT_TRUE(get_ran) << "for which " << which;
    EXPECT_FALSE(get.succeeded) << "getpriority(which=" << which << ") must fail";
    EXPECT_EQ(EINVAL, get.ret_errno) << "getpriority(which=" << which << ")";

    ErrnoObservation set {};
    const ::testing::AssertionResult set_ran =
        RunInChild([which] { return ObserveSetpriority(which, 0, 5); }, &set);
    ASSERT_TRUE(set_ran) << "for which " << which;
    EXPECT_FALSE(set.succeeded) << "setpriority(which=" << which << ") must fail";
    EXPECT_EQ(EINVAL, set.ret_errno) << "setpriority(which=" << which << ")";
}

TEST(Getpriority, InvalidWhichIsEinval) {
    ExpectWhichEinval(PRIO_USER + 1);
    ExpectWhichEinval(-1);
    ExpectWhichEinval(INT_MAX);
    ExpectWhichEinval(INT_MIN);
}

// A `who` that names no task is ESRCH. In particular a negative pid is not
// EINVAL: Linux treats it as an id that does not resolve, and callers rely on
// that to probe for liveness.
void ExpectWhoEsrch(int which, int who) {
    ErrnoObservation get {};
    const ::testing::AssertionResult get_ran =
        RunInChild([which, who] { return ObserveGetpriority(which, who); }, &get);
    ASSERT_TRUE(get_ran) << "for which " << which << " who " << who;
    EXPECT_FALSE(get.succeeded) << "getpriority(" << which << ", " << who << ") must fail";
    EXPECT_EQ(ESRCH, get.ret_errno) << "getpriority(" << which << ", " << who << ")";

    ErrnoObservation set {};
    const ::testing::AssertionResult set_ran =
        RunInChild([which, who] { return ObserveSetpriority(which, who, 5); }, &set);
    ASSERT_TRUE(set_ran) << "for which " << which << " who " << who;
    EXPECT_FALSE(set.succeeded) << "setpriority(" << which << ", " << who << ") must fail";
    EXPECT_EQ(ESRCH, set.ret_errno) << "setpriority(" << which << ", " << who << ")";
}

TEST(Getpriority, UnknownWhoIsEsrch) {
    ExpectWhoEsrch(PRIO_PROCESS, INT_MAX - 1);
    ExpectWhoEsrch(PRIO_PROCESS, -1);
    // No task lives in this process group.
    ExpectWhoEsrch(PRIO_PGRP, INT_MAX - 1);
    ExpectWhoEsrch(PRIO_PGRP, -1);
    // No task has this uid, so the selector matches nothing.
    ExpectWhoEsrch(PRIO_USER, static_cast<int>(kUnusedUid));
    ExpectWhoEsrch(PRIO_USER, static_cast<int>(kUnusedUid) + 1);
}

struct PrecedenceObservation {
    int setresuid_errno;
    uid_t uid;
    int denied_errno;  // a valid selector on a task the caller does not own
    int invalid_errno; // the same call with `which` out of range
};

// `which` out of range is checked before anything else, so it outranks the
// permission check. The child addresses pid 1, which it may not touch, and gets
// EPERM when the selector is valid and EINVAL when it is not; a caller that
// tested the permission first would report EPERM for both and be told the
// selector it passed was acceptable.
PrecedenceObservation ObservePrecedence() {
    PrecedenceObservation observation {};
    errno = 0;
    if (setresuid(kUnusedUid, kUnusedUid, 0) != 0) {
        observation.setresuid_errno = errno;
        return observation;
    }
    observation.uid = getuid();

    RawSetpriority(PRIO_PROCESS, 1, 0, &observation.denied_errno);
    RawSetpriority(PRIO_USER + 1, 1, 0, &observation.invalid_errno);
    return observation;
}

TEST(Setpriority, InvalidWhichOutranksPermissionDenial) {
    PrecedenceObservation observation {};
    const ::testing::AssertionResult child_ran = RunInChild(ObservePrecedence, &observation);
    ASSERT_TRUE(child_ran);
    ASSERT_EQ(0, observation.setresuid_errno)
        << "setresuid(" << kUnusedUid << "): " << strerror(observation.setresuid_errno);
    ASSERT_EQ(kUnusedUid, observation.uid);
    ASSERT_EQ(EPERM, observation.denied_errno)
        << "setpriority(PRIO_PROCESS, 1) must be refused for uid " << kUnusedUid;
    EXPECT_EQ(EINVAL, observation.invalid_errno)
        << "an out-of-range `which` must be rejected before the permission check";
}

// ---------------------------------------------------------------------------
// The PRIO_PGRP and PRIO_USER selectors.
// ---------------------------------------------------------------------------

struct SelectorObservation {
    int setpgid_errno;
    int pgrp_set_errno;
    long pgrp_raw_value;
    int pgrp_raw_errno;
    int pgrp_libc_value;
    int pgrp_libc_errno;
    int self_after_pgrp;
    int self_errno;
    int setresuid_errno;
    uid_t uid;
    int user_set_errno;
    long user_raw_value;
    int user_raw_errno;
    int user_libc_value;
    int user_libc_errno;
    long user_self_raw_value;
    int user_self_raw_errno;
};

// Runs in a child that has made itself the only member of a fresh process
// group, and — after the PRIO_PGRP half — the only task holding its uid.
SelectorObservation ObserveSelectors() {
    SelectorObservation observation {};

    // A new process group containing exactly this child, so PRIO_PGRP cannot
    // reach the shell or any other task in the guest. The rest of this function
    // only runs once that succeeded: `PRIO_PGRP, 0` would otherwise re-nice the
    // group this child inherited — the test runner and everything around it.
    errno = 0;
    if (setpgid(0, 0) != 0) {
        observation.setpgid_errno = errno;
        return observation;
    }
    if (RawSetpriority(PRIO_PGRP, 0, kSelectorPgrpNice, &observation.pgrp_set_errno) != 0) {
        return observation;
    }

    observation.pgrp_raw_value = RawGetpriority(PRIO_PGRP, 0, &observation.pgrp_raw_errno);
    errno = 0;
    observation.pgrp_libc_value = getpriority(PRIO_PGRP, 0);
    observation.pgrp_libc_errno = errno;

    errno = 0;
    observation.self_after_pgrp = getpriority(PRIO_PROCESS, 0);
    observation.self_errno = errno;

    // Move to a uid that nothing else holds, which makes PRIO_USER select this
    // child and nothing else. Keeping the saved uid at 0 lets the child reach
    // both branches of Linux's uid selection in the same process: an explicit
    // `who`, and `who == 0` meaning "my real uid".
    errno = 0;
    if (setresuid(kUnusedUid, kUnusedUid, 0) != 0) {
        observation.setresuid_errno = errno;
        return observation;
    }
    observation.uid = getuid();

    if (RawSetpriority(PRIO_USER, static_cast<int>(kUnusedUid), kSelectorUserNice,
                       &observation.user_set_errno) != 0) {
        return observation;
    }

    observation.user_raw_value =
        RawGetpriority(PRIO_USER, static_cast<int>(kUnusedUid), &observation.user_raw_errno);
    errno = 0;
    observation.user_libc_value = getpriority(PRIO_USER, static_cast<int>(kUnusedUid));
    observation.user_libc_errno = errno;

    observation.user_self_raw_value = RawGetpriority(PRIO_USER, 0, &observation.user_self_raw_errno);
    return observation;
}

TEST(Setpriority, PgrpAndUserSelectorsApplyToSelf) {
    SelectorObservation observation {};
    const ::testing::AssertionResult child_ran =
        RunInChild([] { return ObserveSelectors(); }, &observation);
    ASSERT_TRUE(child_ran);

    ASSERT_EQ(0, observation.setpgid_errno)
        << "setpgid(0, 0): " << strerror(observation.setpgid_errno);
    ASSERT_EQ(0, observation.pgrp_set_errno)
        << "setpriority(PRIO_PGRP, 0, " << kSelectorPgrpNice << "): "
        << strerror(observation.pgrp_set_errno);
    EXPECT_EQ(kMaxNice - kSelectorPgrpNice + 1, observation.pgrp_raw_value)
        << "getpriority(PRIO_PGRP, 0) (errno " << observation.pgrp_raw_errno << ")";
    EXPECT_EQ(kSelectorPgrpNice, observation.pgrp_libc_value)
        << "getpriority(PRIO_PGRP, 0) (errno " << observation.pgrp_libc_errno << ")";
    // The child is a member of the group it just addressed.
    EXPECT_EQ(kSelectorPgrpNice, observation.self_after_pgrp)
        << "PRIO_PGRP must have applied to the process group member (errno "
        << observation.self_errno << ")";

    ASSERT_EQ(0, observation.setresuid_errno)
        << "setresuid(" << kUnusedUid << "): " << strerror(observation.setresuid_errno);
    ASSERT_EQ(kUnusedUid, observation.uid);
    ASSERT_EQ(0, observation.user_set_errno)
        << "setpriority(PRIO_USER, " << kUnusedUid << ", " << kSelectorUserNice
        << "): " << strerror(observation.user_set_errno);
    EXPECT_EQ(kMaxNice - kSelectorUserNice + 1, observation.user_raw_value)
        << "getpriority(PRIO_USER, " << kUnusedUid << ") (errno " << observation.user_raw_errno
        << ")";
    EXPECT_EQ(kSelectorUserNice, observation.user_libc_value)
        << "getpriority(PRIO_USER, " << kUnusedUid << ") (errno " << observation.user_libc_errno
        << ")";
    EXPECT_EQ(kMaxNice - kSelectorUserNice + 1, observation.user_self_raw_value)
        << "getpriority(PRIO_USER, 0) must select the caller's real uid (errno "
        << observation.user_self_raw_errno << ")";
}

// ---------------------------------------------------------------------------
// PRIO_PGRP reaches every thread of every member.
// ---------------------------------------------------------------------------

// The worker holds the lower value, so the maximum `getpriority()` reports can
// only come from a thread the process-group index does not link directly.
constexpr int kGroupLeaderNice = 3;
constexpr int kGroupWorkerNice = -4;
constexpr int kGroupTargetNice = 6;

struct GroupThreadState {
    std::atomic<bool> worker_ready {false};
    std::atomic<bool> release_worker {false};
    pid_t worker_tid;
    int worker_nice_errno;
};

// Releases the worker on every path out of the observation, including a fatal
// assertion that unwinds before the explicit release.
struct WorkerReleaser {
    std::atomic<bool>* flag;
    ~WorkerReleaser() { flag->store(true); }
};

void* GroupWorkerEntry(void* arg) {
    GroupThreadState* state = static_cast<GroupThreadState*>(arg);
    state->worker_tid = static_cast<pid_t>(syscall(SYS_gettid));

    // Diverge this thread from its thread group leader: a nice value belongs to
    // a task, so the group is only uniform once the selector under test has run.
    RawSetpriority(PRIO_PROCESS, static_cast<int>(state->worker_tid), kGroupWorkerNice,
                   &state->worker_nice_errno);
    state->worker_ready.store(true);

    while (!state->release_worker.load()) sched_yield();
    return nullptr;
}

struct GroupObservation {
    int setpgid_errno;
    int pthread_errno;
    int leader_nice_errno;
    pid_t worker_tid;
    int worker_nice_errno;
    int pgrp_before_raw;
    int pgrp_before_errno;
    int pgrp_set_errno;
    int pgrp_after_raw;
    int pgrp_after_errno;
    bool leader_stat_ok;
    long leader_stat_nice;
    bool worker_stat_ok;
    long worker_stat_nice;
};

// Runs in a child that owns a process group of its own, made of one thread
// group with two threads that hold different nice values.
GroupObservation ObserveGroupThreads() {
    GroupObservation observation {};

    // A group this child owns, so the selector cannot reach the test runner.
    errno = 0;
    if (setpgid(0, 0) != 0) {
        observation.setpgid_errno = errno;
        return observation;
    }

    GroupThreadState state {};
    pthread_t worker {};
    errno = 0;
    if (pthread_create(&worker, nullptr, GroupWorkerEntry, &state) != 0) {
        observation.pthread_errno = errno;
        return observation;
    }
    WorkerReleaser releaser {&state.release_worker};

    RawSetpriority(PRIO_PROCESS, 0, kGroupLeaderNice, &observation.leader_nice_errno);
    while (!state.worker_ready.load()) sched_yield();
    observation.worker_tid = state.worker_tid;
    observation.worker_nice_errno = state.worker_nice_errno;

    // The group is not uniform here, so the maximum has to be taken over both
    // threads for this to report the worker's value.
    observation.pgrp_before_raw = RawGetpriority(PRIO_PGRP, 0, &observation.pgrp_before_errno);
    RawSetpriority(PRIO_PGRP, 0, kGroupTargetNice, &observation.pgrp_set_errno);
    observation.pgrp_after_raw = RawGetpriority(PRIO_PGRP, 0, &observation.pgrp_after_errno);

    long priority = 0;
    observation.leader_stat_ok =
        ReadStatPriorityAndNiceAt("/proc/self/stat", &priority, &observation.leader_stat_nice);
    char path[64] = {};
    snprintf(path, sizeof(path), "/proc/self/task/%d/stat",
             static_cast<int>(observation.worker_tid));
    observation.worker_stat_ok =
        ReadStatPriorityAndNiceAt(path, &priority, &observation.worker_stat_nice);

    // Leave the group as it was found. Other cases in this binary read the
    // calling process's own nice value.
    int restore_errno = 0;
    RawSetpriority(PRIO_PGRP, 0, 0, &restore_errno);

    state.release_worker.store(true);
    pthread_join(worker, nullptr);
    return observation;
}

TEST(Setpriority, PgrpSelectorReachesEveryThreadOfAMember) {
    GroupObservation observation {};
    const ::testing::AssertionResult child_ran = RunInChild(ObserveGroupThreads, &observation);
    ASSERT_TRUE(child_ran);
    ASSERT_EQ(0, observation.setpgid_errno)
        << "setpgid(0, 0): " << strerror(observation.setpgid_errno);
    ASSERT_EQ(0, observation.pthread_errno)
        << "pthread_create: " << strerror(observation.pthread_errno);
    ASSERT_EQ(0, observation.leader_nice_errno)
        << "setpriority(PRIO_PROCESS, 0, " << kGroupLeaderNice
        << "): " << strerror(observation.leader_nice_errno);
    ASSERT_NE(0, observation.worker_tid);
    ASSERT_EQ(0, observation.worker_nice_errno)
        << "setpriority(PRIO_PROCESS, " << observation.worker_tid << ", " << kGroupWorkerNice
        << "): " << strerror(observation.worker_nice_errno);

    EXPECT_EQ(kMaxNice - kGroupWorkerNice + 1, observation.pgrp_before_raw)
        << "getpriority(PRIO_PGRP, 0) must take the maximum over every thread of every "
        << "member (errno " << observation.pgrp_before_errno << "); the leader holds "
        << kGroupLeaderNice << " and the worker holds " << kGroupWorkerNice;

    ASSERT_EQ(0, observation.pgrp_set_errno)
        << "setpriority(PRIO_PGRP, 0, " << kGroupTargetNice
        << "): " << strerror(observation.pgrp_set_errno);
    EXPECT_EQ(kMaxNice - kGroupTargetNice + 1, observation.pgrp_after_raw)
        << "getpriority(PRIO_PGRP, 0) after the group-wide assignment (errno "
        << observation.pgrp_after_errno << ")";

    ASSERT_TRUE(observation.leader_stat_ok) << "failed to read /proc/self/stat";
    EXPECT_EQ(kGroupTargetNice, observation.leader_stat_nice)
        << "the thread group leader must have been re-niced by PRIO_PGRP";
    ASSERT_TRUE(observation.worker_stat_ok)
        << "failed to read /proc/self/task/" << observation.worker_tid << "/stat";
    EXPECT_EQ(kGroupTargetNice, observation.worker_stat_nice)
        << "PRIO_PGRP must reach every thread of a member, not only the leader the "
        << "process group index links";
}

struct ForeignObservation {
    int setresuid_errno;
    uid_t uid;
    bool parent_stat_ok;
    long parent_stat_nice;
    long parent_stat_priority;
    int raw_errno;
    long raw_value;
    int libc_value;
    int libc_errno;
};

// getpriority() has no permission check at all: any task may read any other
// task's nice value, whatever uid either of them holds. This is the half of the
// ABI that `ps` and `top` depend on, so it must not be gated behind
// CAP_SYS_NICE. The child drops to an unprivileged uid first so that reading
// the root-owned parent really is a cross-uid read.
TEST(Getpriority, ReadingAnotherTaskNeedsNoPrivilege) {
    const pid_t parent = getpid();
    ForeignObservation observation {};
    const ::testing::AssertionResult child_ran = RunInChild(
        [parent] {
            ForeignObservation result {};
            errno = 0;
            if (setresuid(kUnusedUid, kUnusedUid, 0) != 0) {
                result.setresuid_errno = errno;
                return result;
            }
            result.uid = getuid();
            result.parent_stat_ok = ReadStatPriorityAndNice(
                parent, &result.parent_stat_priority, &result.parent_stat_nice);
            result.raw_value = RawGetpriority(PRIO_PROCESS, parent, &result.raw_errno);
            errno = 0;
            result.libc_value = getpriority(PRIO_PROCESS, parent);
            result.libc_errno = errno;
            return result;
        },
        &observation);
    ASSERT_TRUE(child_ran);

    ASSERT_EQ(0, observation.setresuid_errno)
        << "setresuid(" << kUnusedUid << "): " << strerror(observation.setresuid_errno);
    ASSERT_EQ(kUnusedUid, observation.uid);
    ASSERT_TRUE(observation.parent_stat_ok) << "failed to read /proc/<parent>/stat";
    // The parent has never been re-niced, so both sides are the default.
    EXPECT_EQ(0, observation.parent_stat_nice);
    EXPECT_EQ(kMaxNice - observation.parent_stat_nice + 1, observation.raw_value)
        << "getpriority() on another task must succeed (errno " << observation.raw_errno << ")";
    EXPECT_EQ(observation.parent_stat_nice, observation.libc_value)
        << "getpriority() on another task must succeed (errno " << observation.libc_errno << ")";
    EXPECT_EQ(kDefaultPrio - kMaxRtPrio, observation.parent_stat_priority);
}

// ---------------------------------------------------------------------------
// RLIMIT_NICE.
// ---------------------------------------------------------------------------

struct RlimitObservation {
    int setrlimit_errno;
    int setresuid_errno;
    uid_t uid;
    int raise_errno;      // nice +1: making the task *less* important
    int at_limit_errno;   // nice_to_rlimit(nice) == the soft limit
    int at_limit_nice;
    int over_limit_errno; // one step past the soft limit
    int clamped_errno;    // a far out-of-range request that clamps past the limit
    int foreign_errno;    // another task owned by a different uid
};

// The child starts as root so that it can raise its own RLIMIT_NICE, then drops
// to an unprivileged uid. RLIMIT_NICE is then the only thing that governs how
// far it may lower its nice value, and a soft limit of 25 permits exactly
// `nice_to_rlimit(nice) <= 25`, i.e. nice >= -5.
RlimitObservation ObserveRlimitNice(pid_t foreign_target) {
    constexpr int kSoftLimit = 25;
    constexpr int kAllowedNice = -5;
    constexpr int kDeniedNice = -6;

    RlimitObservation observation {};
    struct rlimit limit = {kSoftLimit, kSoftLimit};
    if (setrlimit(RLIMIT_NICE, &limit) != 0) {
        observation.setrlimit_errno = errno;
        return observation;
    }

    // Keeping the saved uid at 0 keeps the privilege drop reversible in
    // principle; the effective capability set is emptied either way, which is
    // what can_nice() consults.
    errno = 0;
    if (setresuid(1000, 1000, 0) != 0) {
        observation.setresuid_errno = errno;
        return observation;
    }
    observation.uid = getuid();

    RawSetpriority(PRIO_PROCESS, 0, 1, &observation.raise_errno);

    if (RawSetpriority(PRIO_PROCESS, 0, kAllowedNice, &observation.at_limit_errno) == 0) {
        errno = 0;
        observation.at_limit_nice = getpriority(PRIO_PROCESS, 0);
    }

    RawSetpriority(PRIO_PROCESS, 0, kDeniedNice, &observation.over_limit_errno);
    RawSetpriority(PRIO_PROCESS, 0, kMinNice, &observation.clamped_errno);
    RawSetpriority(PRIO_PROCESS, foreign_target, 1, &observation.foreign_errno);
    return observation;
}

TEST(Setpriority, RlimitNiceGatesOnlyPrivilegeRaising) {
    const pid_t parent = getpid();
    RlimitObservation observation {};
    const ::testing::AssertionResult child_ran =
        RunInChild([parent] { return ObserveRlimitNice(parent); }, &observation);
    ASSERT_TRUE(child_ran);

    ASSERT_EQ(0, observation.setrlimit_errno)
        << "setrlimit(RLIMIT_NICE): " << strerror(observation.setrlimit_errno);
    ASSERT_EQ(0, observation.setresuid_errno)
        << "setresuid(1000, 1000, 0): " << strerror(observation.setresuid_errno);
    ASSERT_EQ(1000u, observation.uid);

    // Raising the nice value lowers the task's priority, which never needs a
    // privilege.
    EXPECT_EQ(0, observation.raise_errno)
        << "an unprivileged task must always be allowed to raise its nice value: "
        << strerror(observation.raise_errno);

    EXPECT_EQ(0, observation.at_limit_errno)
        << "nice_to_rlimit(-5) == 25 must fit under a soft limit of 25: "
        << strerror(observation.at_limit_errno);
    EXPECT_EQ(-5, observation.at_limit_nice) << "the allowed reduction must have taken effect";

    EXPECT_EQ(EACCES, observation.over_limit_errno)
        << "nice_to_rlimit(-6) == 26 exceeds RLIMIT_NICE 25 and must be refused with EACCES";
    // Linux clamps the request first and checks the limit second, so a request
    // far below the range still reports EACCES rather than EINVAL.
    EXPECT_EQ(EACCES, observation.clamped_errno)
        << "an out-of-range nice must clamp and then be refused with EACCES";

    // The parent belongs to a different uid and the child has no CAP_SYS_NICE,
    // so the ownership rule rejects the call before the limit is even
    // consulted.
    EXPECT_EQ(EPERM, observation.foreign_errno)
        << "an unprivileged task must not re-nice a task owned by another uid";
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
