#include <gtest/gtest.h>

#include <atomic>
#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <string>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <time.h>
#include <vector>

namespace {

constexpr int64_t kEpoch2020Seconds = 1577836800LL;
constexpr int64_t kNanosecondsPerSecond = 1000000000LL;
constexpr int64_t kShortWaitNanoseconds = 40000000LL;
constexpr int64_t kMinimumObservedWaitNanoseconds = 20000000LL;
constexpr int64_t kMaximumObservedWaitNanoseconds = 2000000000LL;
constexpr int kSemanticReadIterations = 10000;
constexpr uint64_t kStressReads = 10000000ULL;

volatile sig_atomic_t g_nanosleep_signal_observed = 0;

void RecordNanosleepSignal(int) {
    g_nanosleep_signal_observed = 1;
}

timespec ReadClock(clockid_t clock_id) {
    timespec value = {};
    EXPECT_EQ(0, clock_gettime(clock_id, &value))
        << "clock_gettime(" << clock_id << ") failed: errno=" << errno << " ("
        << strerror(errno) << ")";
    return value;
}

void ExpectNormalized(const timespec& value) {
    EXPECT_GE(value.tv_sec, 0);
    EXPECT_GE(value.tv_nsec, 0);
    EXPECT_LT(value.tv_nsec, 1000000000L);
}

int Compare(const timespec& lhs, const timespec& rhs) {
    if (lhs.tv_sec != rhs.tv_sec) {
        return lhs.tv_sec < rhs.tv_sec ? -1 : 1;
    }
    if (lhs.tv_nsec != rhs.tv_nsec) {
        return lhs.tv_nsec < rhs.tv_nsec ? -1 : 1;
    }
    return 0;
}

int64_t DiffNs(const timespec& start, const timespec& end) {
    return (end.tv_sec - start.tv_sec) * kNanosecondsPerSecond +
           (end.tv_nsec - start.tv_nsec);
}

timespec AddNs(const timespec& value, int64_t nanoseconds) {
    const __int128 total = static_cast<__int128>(value.tv_sec) * kNanosecondsPerSecond +
                           value.tv_nsec + nanoseconds;
    timespec result = {};
    result.tv_sec = static_cast<time_t>(total / kNanosecondsPerSecond);
    result.tv_nsec = static_cast<long>(total % kNanosecondsPerSecond);
    return result;
}

bool ReadClockRaw(clockid_t clock_id, timespec* value, int* error) {
    if (clock_gettime(clock_id, value) == 0) {
        return true;
    }
    if (error != nullptr) {
        *error = errno;
    }
    return false;
}

std::vector<int> AllowedCpus() {
    cpu_set_t set;
    CPU_ZERO(&set);
    if (sched_getaffinity(0, sizeof(set), &set) != 0) {
        return {};
    }

    std::vector<int> cpus;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
        if (CPU_ISSET(cpu, &set)) {
            cpus.push_back(cpu);
        }
    }
    return cpus;
}

bool SetCurrentCpu(int cpu) {
    cpu_set_t set;
    CPU_ZERO(&set);
    CPU_SET(cpu, &set);
    return sched_setaffinity(0, sizeof(set), &set) == 0;
}

int CurrentCpu() {
    unsigned int cpu = 0;
    if (syscall(SYS_getcpu, &cpu, nullptr, nullptr) != 0) {
        return -1;
    }
    return static_cast<int>(cpu);
}

bool WaitUntilCurrentCpu(int target_cpu) {
    timespec start = {};
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        return false;
    }
    for (int attempts = 0; attempts < 2000000; ++attempts) {
        if (CurrentCpu() == target_cpu) {
            return true;
        }
        timespec now = {};
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0 ||
            DiffNs(start, now) > kMaximumObservedWaitNanoseconds) {
            return false;
        }
        sched_yield();
    }
    return false;
}

class ScopedAffinity {
public:
    bool Capture() {
        CPU_ZERO(&saved_);
        active_ = sched_getaffinity(0, sizeof(saved_), &saved_) == 0;
        return active_;
    }

    ~ScopedAffinity() {
        if (active_ && sched_setaffinity(0, sizeof(saved_), &saved_) != 0) {
            ADD_FAILURE() << "failed to restore affinity: errno=" << errno << " ("
                          << strerror(errno) << ")";
        }
    }

    ScopedAffinity(const ScopedAffinity&) = delete;
    ScopedAffinity& operator=(const ScopedAffinity&) = delete;
    ScopedAffinity() = default;

private:
    cpu_set_t saved_ {};
    bool active_ = false;
};

bool ReadWholeFile(const char* path, std::string* output, int* error) {
    const int fd = open(path, O_RDONLY);
    if (fd < 0) {
        if (error != nullptr) {
            *error = errno;
        }
        return false;
    }

    output->clear();
    char buffer[4096];
    for (;;) {
        const ssize_t count = read(fd, buffer, sizeof(buffer));
        if (count > 0) {
            output->append(buffer, static_cast<size_t>(count));
            continue;
        }
        if (count == 0) {
            close(fd);
            return true;
        }
        if (errno == EINTR) {
            continue;
        }
        if (error != nullptr) {
            *error = errno;
        }
        close(fd);
        return false;
    }
}

bool ParseBtime(const std::string& contents, int64_t* btime, std::string* error) {
    size_t offset = 0;
    int matches = 0;
    int64_t parsed = -1;
    while (offset < contents.size()) {
        const size_t end = contents.find('\n', offset);
        const size_t length = (end == std::string::npos ? contents.size() : end) - offset;
        if (length >= 6 && contents.compare(offset, 6, "btime ") == 0) {
            const std::string value = contents.substr(offset + 6, length - 6);
            char* parse_end = nullptr;
            errno = 0;
            const long long candidate = strtoll(value.c_str(), &parse_end, 10);
            if (errno != 0 || parse_end == value.c_str() || *parse_end != '\0' || candidate < 0) {
                *error = "malformed btime line: " + value;
                return false;
            }
            parsed = static_cast<int64_t>(candidate);
            ++matches;
        }
        if (end == std::string::npos) {
            break;
        }
        offset = end + 1;
    }
    if (matches != 1) {
        *error = "expected exactly one btime line, found " + std::to_string(matches);
        return false;
    }
    *btime = parsed;
    return true;
}

bool ReadBtime(int64_t* btime, std::string* error) {
    std::string contents;
    int read_error = 0;
    if (!ReadWholeFile("/proc/stat", &contents, &read_error)) {
        *error = "read /proc/stat failed: " + std::to_string(read_error) + " (" +
                 strerror(read_error) + ")";
        return false;
    }
    return ParseBtime(contents, btime, error);
}

struct NanosleepResult {
    int return_code = -1;
    int64_t elapsed_ns = -1;
    timespec deadline = {};
    timespec end_clock = {};
};

bool ReadExact(int fd, void* buffer, size_t length) {
    auto* bytes = static_cast<char*>(buffer);
    size_t done = 0;
    while (done < length) {
        const ssize_t count = read(fd, bytes + done, length - done);
        if (count > 0) {
            done += static_cast<size_t>(count);
            continue;
        }
        if (count < 0 && errno == EINTR) {
            continue;
        }
        return false;
    }
    return true;
}

bool WriteExact(int fd, const void* buffer, size_t length) {
    const auto* bytes = static_cast<const char*>(buffer);
    size_t done = 0;
    while (done < length) {
        const ssize_t count = write(fd, bytes + done, length - done);
        if (count > 0) {
            done += static_cast<size_t>(count);
            continue;
        }
        if (count < 0 && errno == EINTR) {
            continue;
        }
        return false;
    }
    return true;
}

bool RunClockNanosleepBounded(clockid_t clock_id, int flags, const timespec& request,
                              NanosleepResult* result, std::string* error) {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
        *error = "pipe failed";
        return false;
    }
    const pid_t child = fork();
    if (child < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        *error = "fork failed";
        return false;
    }
    if (child == 0) {
        close(pipe_fds[0]);
        NanosleepResult child_result;
        timespec start = {};
        timespec end = {};
        if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
            _exit(120);
        }
        timespec effective_request = request;
        if ((flags & TIMER_ABSTIME) != 0) {
            timespec clock_start = {};
            if (clock_gettime(clock_id, &clock_start) != 0) {
                _exit(127);
            }
            effective_request = AddNs(clock_start, DiffNs(timespec {}, request));
            child_result.deadline = effective_request;
        }
        child_result.return_code =
            clock_nanosleep(clock_id, flags, &effective_request, nullptr);
        if (clock_gettime(CLOCK_MONOTONIC, &end) != 0 ||
            clock_gettime(clock_id, &child_result.end_clock) != 0) {
            _exit(121);
        }
        child_result.elapsed_ns = DiffNs(start, end);
        const bool wrote = WriteExact(pipe_fds[1], &child_result, sizeof(child_result));
        close(pipe_fds[1]);
        _exit(wrote ? 0 : 122);
    }

    close(pipe_fds[1]);
    timespec wait_start = {};
    clock_gettime(CLOCK_MONOTONIC, &wait_start);
    int status = 0;
    for (int polls = 0;; ++polls) {
        const pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) {
            break;
        }
        if (waited < 0) {
            kill(child, SIGKILL);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
            }
            close(pipe_fds[0]);
            *error = "waitpid failed";
            return false;
        }
        timespec now = {};
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (polls >= 2000 || DiffNs(wait_start, now) > kMaximumObservedWaitNanoseconds) {
            kill(child, SIGKILL);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
            }
            close(pipe_fds[0]);
            *error = "clock_nanosleep exceeded bounded wait (possible wrong clock domain)";
            return false;
        }
        usleep(1000);
    }

    const bool read_ok = ReadExact(pipe_fds[0], result, sizeof(*result));
    close(pipe_fds[0]);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || !read_ok) {
        *error = "clock_nanosleep child failed, status=" + std::to_string(status);
        return false;
    }
    return true;
}

struct ConcurrentReadResult {
    int error = 0;
    int clock_index = -1;
    int iteration = -1;
    timespec previous = {};
    timespec current = {};
};

struct ConcurrentReadArgs {
    std::atomic<bool>* start;
    ConcurrentReadResult* result;
};

void* ConcurrentReader(void* opaque) {
    auto* args = static_cast<ConcurrentReadArgs*>(opaque);
    while (!args->start->load(std::memory_order_acquire)) {
        sched_yield();
    }
    const clockid_t clocks[] = {CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_BOOTTIME};
    timespec previous[3] = {};
    for (int clock_index = 0; clock_index < 3; ++clock_index) {
        if (!ReadClockRaw(clocks[clock_index], &previous[clock_index], &args->result->error)) {
            args->result->clock_index = clock_index;
            return nullptr;
        }
    }
    for (int iteration = 0; iteration < kSemanticReadIterations; ++iteration) {
        for (int clock_index = 0; clock_index < 3; ++clock_index) {
            timespec current = {};
            if (!ReadClockRaw(clocks[clock_index], &current, &args->result->error) ||
                Compare(previous[clock_index], current) > 0) {
                args->result->clock_index = clock_index;
                args->result->iteration = iteration;
                args->result->previous = previous[clock_index];
                args->result->current = current;
                return nullptr;
            }
            previous[clock_index] = current;
        }
    }
    return nullptr;
}

class ScopedSignalMask {
public:
    bool CaptureAndBlock(int signal_number) {
        if (pthread_sigmask(SIG_SETMASK, nullptr, &saved_) != 0) {
            return false;
        }
        sigset_t one;
        sigemptyset(&one);
        sigaddset(&one, signal_number);
        active_ = pthread_sigmask(SIG_BLOCK, &one, nullptr) == 0;
        return active_;
    }

    ~ScopedSignalMask() {
        if (active_) {
            const int result = pthread_sigmask(SIG_SETMASK, &saved_, nullptr);
            if (result != 0) {
                ADD_FAILURE() << "failed to restore signal mask: " << strerror(result);
            }
        }
    }

private:
    sigset_t saved_ {};
    bool active_ = false;
};

struct SigtimedwaitResult {
    int return_value = 0;
    int error = 0;
    int64_t elapsed_ns = 0;
};

bool RunSigtimedwaitBounded(int signal_number, const timespec& timeout,
                            SigtimedwaitResult* result, std::string* error) {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
        *error = "pipe failed";
        return false;
    }
    const pid_t child = fork();
    if (child < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        *error = "fork failed";
        return false;
    }
    if (child == 0) {
        close(pipe_fds[0]);
        sigset_t awaited;
        sigemptyset(&awaited);
        sigaddset(&awaited, signal_number);
        if (pthread_sigmask(SIG_BLOCK, &awaited, nullptr) != 0) {
            _exit(123);
        }
        timespec start = {};
        timespec end = {};
        if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
            _exit(124);
        }
        siginfo_t info = {};
        errno = 0;
        SigtimedwaitResult child_result;
        child_result.return_value = sigtimedwait(&awaited, &info, &timeout);
        child_result.error = errno;
        if (clock_gettime(CLOCK_MONOTONIC, &end) != 0) {
            _exit(125);
        }
        child_result.elapsed_ns = DiffNs(start, end);
        const bool wrote = WriteExact(pipe_fds[1], &child_result, sizeof(child_result));
        close(pipe_fds[1]);
        _exit(wrote ? 0 : 126);
    }

    close(pipe_fds[1]);
    timespec wait_start = {};
    clock_gettime(CLOCK_MONOTONIC, &wait_start);
    int status = 0;
    for (int polls = 0;; ++polls) {
        const pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) {
            break;
        }
        if (waited < 0) {
            kill(child, SIGKILL);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
            }
            close(pipe_fds[0]);
            *error = "waitpid failed";
            return false;
        }
        timespec now = {};
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (polls >= 2000 || DiffNs(wait_start, now) > kMaximumObservedWaitNanoseconds) {
            kill(child, SIGKILL);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
            }
            close(pipe_fds[0]);
            *error = "sigtimedwait exceeded bounded wait";
            return false;
        }
        usleep(1000);
    }
    const bool read_ok = ReadExact(pipe_fds[0], result, sizeof(*result));
    close(pipe_fds[0]);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || !read_ok) {
        *error = "sigtimedwait child failed, status=" + std::to_string(status);
        return false;
    }
    return true;
}

struct FutexGuardArgs {
    uint32_t* word;
    std::atomic<bool>* done;
    bool fired = false;
};

void* FutexGuard(void* opaque) {
    auto* args = static_cast<FutexGuardArgs*>(opaque);
    for (int i = 0; i < 500; ++i) {
        if (args->done->load(std::memory_order_acquire)) {
            return nullptr;
        }
        usleep(1000);
    }
    args->fired = true;
    syscall(SYS_futex, args->word, FUTEX_WAKE | FUTEX_PRIVATE_FLAG, 1, nullptr, nullptr, 0);
    return nullptr;
}

struct FutexWaitResult {
    long return_value = 0;
    int error = 0;
    int64_t elapsed_ns = 0;
    bool guard_fired = false;
};

FutexWaitResult RunFutexWaitBitsetInProcess(clockid_t clock_id, bool realtime) {
    alignas(4) uint32_t word = 0;
    std::atomic<bool> done {false};
    FutexGuardArgs guard_args {&word, &done, false};
    pthread_t guard {};
    FutexWaitResult result;
    if (pthread_create(&guard, nullptr, FutexGuard, &guard_args) != 0) {
        result.error = EAGAIN;
        return result;
    }

    timespec now = {};
    timespec start = {};
    timespec end = {};
    if (clock_gettime(clock_id, &now) != 0 || clock_gettime(CLOCK_MONOTONIC, &start) != 0) {
        result.error = errno;
        done.store(true, std::memory_order_release);
        pthread_join(guard, nullptr);
        return result;
    }
    const timespec deadline = AddNs(now, kShortWaitNanoseconds);
    const int operation = FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG |
                          (realtime ? FUTEX_CLOCK_REALTIME : 0);
    errno = 0;
    result.return_value = syscall(SYS_futex, &word, operation, 0, &deadline, nullptr,
                                  FUTEX_BITSET_MATCH_ANY);
    result.error = errno;
    clock_gettime(CLOCK_MONOTONIC, &end);
    result.elapsed_ns = DiffNs(start, end);
    done.store(true, std::memory_order_release);
    pthread_join(guard, nullptr);
    result.guard_fired = guard_args.fired;
    return result;
}

bool RunFutexWaitBitsetBounded(clockid_t clock_id, bool realtime, FutexWaitResult* result,
                               std::string* error) {
    int pipe_fds[2];
    if (pipe(pipe_fds) != 0) {
        *error = "pipe failed";
        return false;
    }
    const pid_t child = fork();
    if (child < 0) {
        close(pipe_fds[0]);
        close(pipe_fds[1]);
        *error = "fork failed";
        return false;
    }
    if (child == 0) {
        close(pipe_fds[0]);
        const FutexWaitResult child_result = RunFutexWaitBitsetInProcess(clock_id, realtime);
        const bool wrote = WriteExact(pipe_fds[1], &child_result, sizeof(child_result));
        close(pipe_fds[1]);
        _exit(wrote ? 0 : 128);
    }

    close(pipe_fds[1]);
    timespec wait_start = {};
    clock_gettime(CLOCK_MONOTONIC, &wait_start);
    int status = 0;
    for (int polls = 0;; ++polls) {
        const pid_t waited = waitpid(child, &status, WNOHANG);
        if (waited == child) {
            break;
        }
        if (waited < 0) {
            kill(child, SIGKILL);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
            }
            close(pipe_fds[0]);
            *error = "waitpid failed";
            return false;
        }
        timespec now = {};
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (polls >= 2000 || DiffNs(wait_start, now) > kMaximumObservedWaitNanoseconds) {
            kill(child, SIGKILL);
            while (waitpid(child, &status, 0) < 0 && errno == EINTR) {
            }
            close(pipe_fds[0]);
            *error = "futex wait exceeded bounded wait (possible wrong clock domain)";
            return false;
        }
        usleep(1000);
    }
    const bool read_ok = ReadExact(pipe_fds[0], result, sizeof(*result));
    close(pipe_fds[0]);
    if (!WIFEXITED(status) || WEXITSTATUS(status) != 0 || !read_ok) {
        *error = "futex child failed, status=" + std::to_string(status);
        return false;
    }
    return true;
}

struct ReadLoopResult {
    bool ok = true;
    int error = 0;
    uint64_t iteration = 0;
    timespec previous = {};
    timespec current = {};
};

ReadLoopResult RunReadLoop(clockid_t clock_id, uint64_t reads) {
    ReadLoopResult result;
    if (!ReadClockRaw(clock_id, &result.previous, &result.error)) {
        result.ok = false;
        return result;
    }
    for (uint64_t i = 0; i < reads; ++i) {
        if (!ReadClockRaw(clock_id, &result.current, &result.error) ||
            Compare(result.previous, result.current) > 0) {
            result.ok = false;
            result.iteration = i;
            return result;
        }
        result.previous = result.current;
    }
    return result;
}

void ExpectReadLoopSucceeded(clockid_t clock_id, const ReadLoopResult& result) {
    EXPECT_TRUE(result.ok) << "clock=" << clock_id << " iteration=" << result.iteration
                           << " errno=" << result.error << " previous="
                           << result.previous.tv_sec << "." << result.previous.tv_nsec
                           << " current=" << result.current.tv_sec << "."
                           << result.current.tv_nsec;
}

}  // namespace

TEST(TimekeepingSemantics, WallClockAndUptimeClocksUseDifferentDomains) {
    const timespec realtime = ReadClock(CLOCK_REALTIME);
    const timespec monotonic = ReadClock(CLOCK_MONOTONIC);
    const timespec raw = ReadClock(CLOCK_MONOTONIC_RAW);
    const timespec boottime = ReadClock(CLOCK_BOOTTIME);

    ExpectNormalized(realtime);
    ExpectNormalized(monotonic);
    ExpectNormalized(raw);
    ExpectNormalized(boottime);

    ASSERT_GE(realtime.tv_sec, kEpoch2020Seconds)
        << "RTC/realtime was not initialized to a plausible wall-clock epoch";
    EXPECT_GE(realtime.tv_sec - monotonic.tv_sec, kEpoch2020Seconds)
        << "CLOCK_MONOTONIC incorrectly shares CLOCK_REALTIME's epoch";
    EXPECT_GE(realtime.tv_sec - raw.tv_sec, kEpoch2020Seconds)
        << "CLOCK_MONOTONIC_RAW incorrectly shares CLOCK_REALTIME's epoch";
    EXPECT_GE(realtime.tv_sec - boottime.tv_sec, kEpoch2020Seconds)
        << "CLOCK_BOOTTIME incorrectly shares CLOCK_REALTIME's epoch";
}

TEST(TimekeepingSemantics, UptimeClocksNeverRegress) {
    const clockid_t clocks[] = {CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_BOOTTIME};

    for (clockid_t clock_id : clocks) {
        timespec previous = ReadClock(clock_id);
        for (int i = 0; i < kSemanticReadIterations; ++i) {
            const timespec current = ReadClock(clock_id);
            ASSERT_LE(Compare(previous, current), 0)
                << "clock " << clock_id << " regressed at iteration " << i;
            previous = current;
        }
    }
}

TEST(TimekeepingSemantics, SleepAdvancesMonotonicAndRawConsistently) {
    const timespec mono_start = ReadClock(CLOCK_MONOTONIC);
    const timespec raw_start = ReadClock(CLOCK_MONOTONIC_RAW);
    ASSERT_EQ(0, usleep(200000));
    const timespec mono_end = ReadClock(CLOCK_MONOTONIC);
    const timespec raw_end = ReadClock(CLOCK_MONOTONIC_RAW);

    const int64_t mono_elapsed = DiffNs(mono_start, mono_end);
    const int64_t raw_elapsed = DiffNs(raw_start, raw_end);
    EXPECT_GE(mono_elapsed, 150000000LL);
    EXPECT_LE(mono_elapsed, 2000000000LL);
    EXPECT_GE(raw_elapsed, 150000000LL);
    EXPECT_LE(raw_elapsed, 2000000000LL);
    EXPECT_LE(mono_elapsed > raw_elapsed ? mono_elapsed - raw_elapsed : raw_elapsed - mono_elapsed,
              10000000LL);
}

TEST(TimekeepingSemantics, ReportedResolutionMatchesLowResolutionTimer) {
    const clockid_t clocks[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW,
                                CLOCK_BOOTTIME};
    for (clockid_t clock_id : clocks) {
        timespec resolution = {};
        ASSERT_EQ(0, clock_getres(clock_id, &resolution));
        ExpectNormalized(resolution);
        const int64_t resolution_ns = DiffNs(timespec{}, resolution);
        EXPECT_EQ(4000000LL, resolution_ns);
    }

    const clockid_t coarse_clocks[] = {CLOCK_REALTIME_COARSE, CLOCK_MONOTONIC_COARSE};
    for (clockid_t clock_id : coarse_clocks) {
        timespec resolution = {};
        ASSERT_EQ(0, clock_getres(clock_id, &resolution));
        const int64_t resolution_ns = DiffNs(timespec{}, resolution);
        EXPECT_EQ(4000000LL, resolution_ns);
    }
}

TEST(TimekeepingSemantics, ClockNanosleepRejectsUnsupportedClockDomains) {
    const timespec request = {.tv_sec = 0, .tv_nsec = 1};
    EXPECT_EQ(EINVAL, clock_nanosleep(CLOCK_THREAD_CPUTIME_ID, 0, &request, nullptr));
    // Linux 6.6 interprets TIMER_ABSTIME and ignores other flag bits.
    EXPECT_EQ(0, clock_nanosleep(CLOCK_MONOTONIC, 2, &request, nullptr));
    EXPECT_EQ(EOPNOTSUPP, clock_nanosleep(CLOCK_MONOTONIC_RAW, 0, &request, nullptr));
    EXPECT_EQ(EOPNOTSUPP, clock_nanosleep(CLOCK_REALTIME_COARSE, 0, &request, nullptr));
    EXPECT_EQ(EOPNOTSUPP, clock_nanosleep(CLOCK_MONOTONIC_COARSE, 0, &request, nullptr));
}

TEST(TimekeepingSemantics, RawClockNanosleepThreadCpuClockIsUnsupported) {
    const timespec request = {.tv_sec = 0, .tv_nsec = 1};
    errno = 0;
    const long result = syscall(SYS_clock_nanosleep, CLOCK_THREAD_CPUTIME_ID, 0, &request,
                                nullptr);
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EOPNOTSUPP, errno);
}

TEST(TimekeepingSemantics, SuccessfulNanosleepLeavesRemainingUntouched) {
    const timespec request = {.tv_sec = 0, .tv_nsec = 1000000};
    timespec remaining = {.tv_sec = 123, .tv_nsec = 456};
    ASSERT_EQ(0, nanosleep(&request, &remaining));
    EXPECT_EQ(123, remaining.tv_sec);
    EXPECT_EQ(456, remaining.tv_nsec);
}

TEST(TimekeepingSemantics, InterruptedNanosleepReportsNormalizedRemainingTime) {
    struct sigaction action = {};
    action.sa_handler = RecordNanosleepSignal;
    sigemptyset(&action.sa_mask);

    struct sigaction previous_action = {};
    ASSERT_EQ(0, sigaction(SIGUSR1, &action, &previous_action));
    g_nanosleep_signal_observed = 0;

    const pid_t parent = getpid();
    const pid_t sender = fork();
    ASSERT_GE(sender, 0);
    if (sender == 0) {
        const timespec delay = {.tv_sec = 0, .tv_nsec = 50000000};
        if (nanosleep(&delay, nullptr) != 0 || kill(parent, SIGUSR1) != 0) {
            _exit(1);
        }
        _exit(0);
    }

    const timespec request = {.tv_sec = 1, .tv_nsec = 0};
    timespec remaining = {.tv_sec = 123, .tv_nsec = 456};
    const timespec start = ReadClock(CLOCK_MONOTONIC);
    const int sleep_result = nanosleep(&request, &remaining);
    const int sleep_errno = errno;
    const timespec end = ReadClock(CLOCK_MONOTONIC);

    int sender_status = 0;
    const pid_t waited = waitpid(sender, &sender_status, 0);
    const int restore_result = sigaction(SIGUSR1, &previous_action, nullptr);

    ASSERT_EQ(sender, waited);
    ASSERT_TRUE(WIFEXITED(sender_status));
    ASSERT_EQ(0, WEXITSTATUS(sender_status));
    ASSERT_EQ(0, restore_result);
    EXPECT_EQ(1, g_nanosleep_signal_observed);
    EXPECT_EQ(-1, sleep_result);
    EXPECT_EQ(EINTR, sleep_errno);
    ExpectNormalized(remaining);
    EXPECT_GT(DiffNs(timespec{}, remaining), 0);
    EXPECT_LT(DiffNs(timespec{}, remaining), DiffNs(timespec{}, request));
    EXPECT_GE(DiffNs(start, end), 20000000LL);
    EXPECT_LT(DiffNs(start, end), 500000000LL);
}

TEST(TimekeepingSemantics, RelativeBoottimeSleepIsAcceptedAndAdvances) {
    const timespec start = ReadClock(CLOCK_BOOTTIME);
    const timespec request = {.tv_sec = 0, .tv_nsec = 20000000};
    ASSERT_EQ(0, clock_nanosleep(CLOCK_BOOTTIME, 0, &request, nullptr));
    const timespec end = ReadClock(CLOCK_BOOTTIME);
    EXPECT_GE(DiffNs(start, end), 15000000LL);
    EXPECT_LE(DiffNs(start, end), 1000000000LL);
}

TEST(TimekeepingSemantics, ForcedTwoCpuMigrationNeverRegresses) {
    ScopedAffinity affinity;
    ASSERT_TRUE(affinity.Capture()) << "sched_getaffinity failed: errno=" << errno << " ("
                                    << strerror(errno) << ")";
    const std::vector<int> cpus = AllowedCpus();
    ASSERT_FALSE(cpus.empty());
    if (cpus.size() < 2) {
        GTEST_SKIP() << "requires at least two allowed CPUs";
    }

    const clockid_t clocks[] = {CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW, CLOCK_BOOTTIME};
    timespec previous[3] = {};
    ASSERT_TRUE(SetCurrentCpu(cpus[0])) << "pin to first CPU failed: errno=" << errno;
    ASSERT_TRUE(WaitUntilCurrentCpu(cpus[0])) << "did not reach first CPU";
    for (int i = 0; i < 3; ++i) {
        ASSERT_EQ(0, clock_gettime(clocks[i], &previous[i]));
    }

    bool saw_first = false;
    bool saw_second = false;
    for (int iteration = 0; iteration < 500; ++iteration) {
        const int target = cpus[iteration & 1];
        ASSERT_TRUE(SetCurrentCpu(target)) << "iteration=" << iteration << " errno=" << errno;
        ASSERT_TRUE(WaitUntilCurrentCpu(target)) << "iteration=" << iteration
                                                << " target_cpu=" << target;
        saw_first |= CurrentCpu() == cpus[0];
        saw_second |= CurrentCpu() == cpus[1];
        for (int clock_index = 0; clock_index < 3; ++clock_index) {
            timespec current = {};
            ASSERT_EQ(0, clock_gettime(clocks[clock_index], &current));
            ASSERT_LE(Compare(previous[clock_index], current), 0)
                << "clock=" << clocks[clock_index] << " iteration=" << iteration
                << " cpu=" << target;
            previous[clock_index] = current;
        }
    }
    EXPECT_TRUE(saw_first);
    EXPECT_TRUE(saw_second);
}

TEST(TimekeepingSemantics, ConcurrentReadersAreMonotonicPerThread) {
    constexpr int kThreadCount = 4;
    std::atomic<bool> start {false};
    pthread_t threads[kThreadCount] = {};
    ConcurrentReadResult results[kThreadCount] = {};
    ConcurrentReadArgs args[kThreadCount] = {};
    int created = 0;
    for (; created < kThreadCount; ++created) {
        args[created] = ConcurrentReadArgs {&start, &results[created]};
        const int error = pthread_create(&threads[created], nullptr, ConcurrentReader, &args[created]);
        if (error != 0) {
            start.store(true, std::memory_order_release);
            for (int i = 0; i < created; ++i) {
                pthread_join(threads[i], nullptr);
            }
            FAIL() << "pthread_create failed: " << strerror(error);
        }
    }
    start.store(true, std::memory_order_release);
    int first_join_error = 0;
    for (int i = 0; i < kThreadCount; ++i) {
        const int error = pthread_join(threads[i], nullptr);
        if (first_join_error == 0 && error != 0) {
            first_join_error = error;
        }
    }
    ASSERT_EQ(0, first_join_error) << "pthread_join failed: " << strerror(first_join_error);
    for (int i = 0; i < kThreadCount; ++i) {
        EXPECT_EQ(-1, results[i].clock_index)
            << "thread=" << i << " clock_index=" << results[i].clock_index
            << " iteration=" << results[i].iteration << " errno=" << results[i].error
            << " previous=" << results[i].previous.tv_sec << "."
            << results[i].previous.tv_nsec << " current=" << results[i].current.tv_sec << "."
            << results[i].current.tv_nsec;
    }
}

TEST(TimekeepingSemantics, ProcStatBtimeIsStableBootEpoch) {
    const timespec realtime_before = ReadClock(CLOCK_REALTIME);
    const timespec boottime_after = ReadClock(CLOCK_BOOTTIME);

    int64_t first_btime = -1;
    std::string error;
    ASSERT_TRUE(ReadBtime(&first_btime, &error)) << error;
    ASSERT_EQ(0, usleep(50000));
    int64_t second_btime = -1;
    ASSERT_TRUE(ReadBtime(&second_btime, &error)) << error;

    const timespec boottime_before = ReadClock(CLOCK_BOOTTIME);
    const timespec realtime_after = ReadClock(CLOCK_REALTIME);
    const __int128 lower_ns =
        static_cast<__int128>(realtime_before.tv_sec) * kNanosecondsPerSecond +
        realtime_before.tv_nsec -
        (static_cast<__int128>(boottime_after.tv_sec) * kNanosecondsPerSecond +
         boottime_after.tv_nsec);
    const __int128 upper_ns =
        static_cast<__int128>(realtime_after.tv_sec) * kNanosecondsPerSecond +
        realtime_after.tv_nsec -
        (static_cast<__int128>(boottime_before.tv_sec) * kNanosecondsPerSecond +
         boottime_before.tv_nsec);
    const int64_t lower_seconds = static_cast<int64_t>(lower_ns / kNanosecondsPerSecond) - 1;
    const int64_t upper_seconds = static_cast<int64_t>(upper_ns / kNanosecondsPerSecond) + 1;

    EXPECT_EQ(first_btime, second_btime);
    EXPECT_GE(first_btime, kEpoch2020Seconds);
    EXPECT_LE(first_btime, realtime_after.tv_sec);
    EXPECT_GE(first_btime, lower_seconds);
    EXPECT_LE(first_btime, upper_seconds);
    EXPECT_GT(first_btime, boottime_before.tv_sec + 86400)
        << "btime appears to contain uptime rather than the boot epoch";
}

TEST(TimekeepingSemantics, ClockNanosleepRelativeAndAbsoluteDomains) {
    const clockid_t clocks[] = {CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_BOOTTIME};
    for (clockid_t clock_id : clocks) {
        const timespec relative = AddNs(timespec {}, kShortWaitNanoseconds);
        NanosleepResult relative_result;
        std::string error;
        ASSERT_TRUE(RunClockNanosleepBounded(clock_id, 0, relative, &relative_result, &error))
            << "relative clock=" << clock_id << ": " << error;
        EXPECT_EQ(0, relative_result.return_code) << "clock=" << clock_id;
        EXPECT_GE(relative_result.elapsed_ns, kMinimumObservedWaitNanoseconds)
            << "clock=" << clock_id;
        EXPECT_LE(relative_result.elapsed_ns, kMaximumObservedWaitNanoseconds)
            << "clock=" << clock_id;

        // The bounded child constructs the absolute deadline from its own
        // clock sample, so fork latency cannot consume the requested budget.
        const timespec absolute_offset = AddNs(timespec {}, kShortWaitNanoseconds);
        NanosleepResult absolute_result;
        ASSERT_TRUE(RunClockNanosleepBounded(clock_id, TIMER_ABSTIME, absolute_offset,
                                             &absolute_result, &error))
            << "absolute clock=" << clock_id << ": " << error;
        EXPECT_EQ(0, absolute_result.return_code) << "clock=" << clock_id;
        EXPECT_GE(absolute_result.elapsed_ns, kMinimumObservedWaitNanoseconds)
            << "clock=" << clock_id;
        EXPECT_LE(absolute_result.elapsed_ns, kMaximumObservedWaitNanoseconds)
            << "clock=" << clock_id;
        EXPECT_LE(Compare(absolute_result.deadline, absolute_result.end_clock), 0)
            << "clock=" << clock_id;
    }
}

TEST(TimekeepingSemantics, RtSigtimedwaitRelativeTimeoutExpiresWithinBudget) {
    ScopedSignalMask mask;
    ASSERT_TRUE(mask.CaptureAndBlock(SIGUSR2));
    sigset_t awaited;
    sigemptyset(&awaited);
    sigaddset(&awaited, SIGUSR2);

    const timespec zero = {};
    siginfo_t info = {};
    errno = 0;
    EXPECT_EQ(-1, sigtimedwait(&awaited, &info, &zero));
    EXPECT_EQ(EAGAIN, errno);

    const timespec invalid = {.tv_sec = 0, .tv_nsec = kNanosecondsPerSecond};
    errno = 0;
    EXPECT_EQ(-1, sigtimedwait(&awaited, &info, &invalid));
    EXPECT_EQ(EINVAL, errno);

    const timespec timeout = AddNs(timespec {}, kShortWaitNanoseconds);
    SigtimedwaitResult result;
    std::string error;
    ASSERT_TRUE(RunSigtimedwaitBounded(SIGUSR2, timeout, &result, &error)) << error;
    EXPECT_EQ(-1, result.return_value);
    EXPECT_EQ(EAGAIN, result.error);
    EXPECT_GE(result.elapsed_ns, kMinimumObservedWaitNanoseconds);
    EXPECT_LE(result.elapsed_ns, kMaximumObservedWaitNanoseconds);
}

TEST(TimekeepingSemantics, FutexWaitBitsetUsesAbsoluteClockDomain) {
    alignas(4) uint32_t invalid_word = 0;
    const timespec negative_deadline = {.tv_sec = -1, .tv_nsec = 0};
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_futex, &invalid_word,
                          FUTEX_WAIT_BITSET | FUTEX_PRIVATE_FLAG, 0, &negative_deadline,
                          nullptr, FUTEX_BITSET_MATCH_ANY));
    EXPECT_EQ(EINVAL, errno);

    FutexWaitResult monotonic;
    std::string error;
    ASSERT_TRUE(RunFutexWaitBitsetBounded(CLOCK_MONOTONIC, false, &monotonic, &error))
        << error;
    EXPECT_EQ(-1, monotonic.return_value);
    EXPECT_EQ(ETIMEDOUT, monotonic.error);
    EXPECT_FALSE(monotonic.guard_fired) << "monotonic futex needed the bounded guard wake";
    EXPECT_GE(monotonic.elapsed_ns, kMinimumObservedWaitNanoseconds);
    EXPECT_LE(monotonic.elapsed_ns, kMaximumObservedWaitNanoseconds);

    FutexWaitResult realtime;
    ASSERT_TRUE(RunFutexWaitBitsetBounded(CLOCK_REALTIME, true, &realtime, &error)) << error;
    EXPECT_EQ(-1, realtime.return_value);
    EXPECT_EQ(ETIMEDOUT, realtime.error);
    EXPECT_FALSE(realtime.guard_fired) << "realtime futex likely used the monotonic domain";
    EXPECT_GE(realtime.elapsed_ns, kMinimumObservedWaitNanoseconds);
    EXPECT_LE(realtime.elapsed_ns, kMaximumObservedWaitNanoseconds);
}

TEST(TimekeepingStress, TenMillionFixedCpuMonotonicAndRawReads) {
    if (getenv("DUNITEST_TIMEKEEPING_STRESS") == nullptr ||
        strcmp(getenv("DUNITEST_TIMEKEEPING_STRESS"), "1") != 0) {
        GTEST_SKIP() << "set DUNITEST_TIMEKEEPING_STRESS=1 to run the strict gate";
    }
    ScopedAffinity affinity;
    ASSERT_TRUE(affinity.Capture());
    const std::vector<int> cpus = AllowedCpus();
    ASSERT_FALSE(cpus.empty());
    ASSERT_TRUE(SetCurrentCpu(cpus[0]));
    ASSERT_TRUE(WaitUntilCurrentCpu(cpus[0]));

    ExpectReadLoopSucceeded(CLOCK_MONOTONIC, RunReadLoop(CLOCK_MONOTONIC, kStressReads));
    ExpectReadLoopSucceeded(CLOCK_MONOTONIC_RAW, RunReadLoop(CLOCK_MONOTONIC_RAW, kStressReads));
}

TEST(TimekeepingStress, TenMillionMigratingMonotonicAndRawReads) {
    if (getenv("DUNITEST_TIMEKEEPING_STRESS") == nullptr ||
        strcmp(getenv("DUNITEST_TIMEKEEPING_STRESS"), "1") != 0) {
        GTEST_SKIP() << "set DUNITEST_TIMEKEEPING_STRESS=1 to run the strict gate";
    }
    ScopedAffinity affinity;
    ASSERT_TRUE(affinity.Capture());
    const std::vector<int> cpus = AllowedCpus();
    ASSERT_FALSE(cpus.empty());
    if (cpus.size() < 2) {
        GTEST_SKIP() << "requires at least two allowed CPUs";
    }

    constexpr uint64_t kBatchReads = 65536;
    const clockid_t clocks[] = {CLOCK_MONOTONIC, CLOCK_MONOTONIC_RAW};
    timespec previous[2] = {};
    for (int i = 0; i < 2; ++i) {
        ASSERT_EQ(0, clock_gettime(clocks[i], &previous[i]));
    }
    bool visited[2] = {false, false};
    uint64_t completed = 0;
    while (completed < kStressReads) {
        const int cpu_index = static_cast<int>((completed / kBatchReads) & 1);
        ASSERT_TRUE(SetCurrentCpu(cpus[cpu_index]));
        ASSERT_TRUE(WaitUntilCurrentCpu(cpus[cpu_index]));
        visited[cpu_index] = true;
        const uint64_t batch =
            (kStressReads - completed < kBatchReads) ? kStressReads - completed : kBatchReads;
        bool batch_ok = true;
        int failure_errno = 0;
        int failure_clock = -1;
        uint64_t failure_iteration = 0;
        timespec failure_previous = {};
        timespec failure_current = {};
        for (uint64_t i = 0; i < batch; ++i) {
            for (int clock_index = 0; clock_index < 2; ++clock_index) {
                timespec current = {};
                if (!ReadClockRaw(clocks[clock_index], &current, &failure_errno) ||
                    Compare(previous[clock_index], current) > 0) {
                    batch_ok = false;
                    failure_clock = clock_index;
                    failure_iteration = completed + i;
                    failure_previous = previous[clock_index];
                    failure_current = current;
                    break;
                }
                previous[clock_index] = current;
            }
            if (!batch_ok) {
                break;
            }
        }
        ASSERT_TRUE(batch_ok)
            << "clock=" << (failure_clock >= 0 ? clocks[failure_clock] : -1)
            << " iteration=" << failure_iteration << " cpu=" << cpus[cpu_index]
            << " errno=" << failure_errno << " previous=" << failure_previous.tv_sec << "."
            << failure_previous.tv_nsec << " current=" << failure_current.tv_sec << "."
            << failure_current.tv_nsec;
        completed += batch;
    }
    EXPECT_TRUE(visited[0]);
    EXPECT_TRUE(visited[1]);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
