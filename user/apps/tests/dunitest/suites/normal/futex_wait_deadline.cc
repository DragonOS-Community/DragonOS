#include <gtest/gtest.h>

#include <atomic>
#include <array>
#include <memory>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <functional>
#include <csignal>
#include <thread>
#include <pthread.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <cerrno>
#include <climits>
#include <cstdint>
#include <linux/futex.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

namespace {
long Wait(int* word, int operation, int expected, const timespec* timeout,
          uint32_t mask = FUTEX_BITSET_MATCH_ANY) {
    return syscall(SYS_futex, word, operation, expected, timeout, nullptr, mask);
}
int64_t Ns(const timespec& t) {
    return static_cast<int64_t>(t.tv_sec) * 1000000000LL + t.tv_nsec;
}
timespec Add(timespec t, long ns) {
    t.tv_nsec += ns;
    t.tv_sec += t.tv_nsec / 1000000000L;
    t.tv_nsec %= 1000000000L;
    return t;
}

TEST(FutexWaitDeadline, InvalidTimeoutPrecedesValueMismatch) {
    int word = 1;
    const timespec bad{0, 1000000000L};
    for (int op : {FUTEX_WAIT_PRIVATE, FUTEX_WAIT_BITSET_PRIVATE}) {
        errno = 0;
        EXPECT_EQ(Wait(&word, op, 0, &bad), -1);
        EXPECT_EQ(errno, EINVAL);
    }
}

TEST(FutexWaitDeadline, PastAbsoluteStillChecksValue) {
    int word = 1;
    const timespec past{0, 0};
    for (int clock_flag : {0, FUTEX_CLOCK_REALTIME}) {
        const int op = FUTEX_WAIT_BITSET_PRIVATE | clock_flag;
        errno = 0;
        EXPECT_EQ(Wait(&word, op, 0, &past), -1);
        EXPECT_EQ(errno, EAGAIN);
        errno = 0;
        EXPECT_EQ(Wait(&word, op, 1, &past), -1);
        EXPECT_EQ(errno, ETIMEDOUT);
    }
}

TEST(FutexWaitDeadline, RelativeBudgetDoesNotExpireEarly) {
    int word = 0;
    const timespec duration{0, 5000000};
    timespec begin{}, end{};
    ASSERT_EQ(clock_gettime(CLOCK_MONOTONIC, &begin), 0);
    errno = 0;
    const long ret = Wait(&word, FUTEX_WAIT_PRIVATE, 0, &duration);
    const int error = errno;
    ASSERT_EQ(clock_gettime(CLOCK_MONOTONIC, &end), 0);
    EXPECT_EQ(ret, -1);
    EXPECT_EQ(error, ETIMEDOUT);
    EXPECT_GE(Ns(end) - Ns(begin), Ns(duration));
    // No tight upper bound: scheduling and host supply are not timer precision.
}

TEST(FutexWaitDeadline, AbsoluteMonotonicAndRealtime) {
    int word = 0;
    for (int clock_flag : {0, FUTEX_CLOCK_REALTIME}) {
        const clockid_t clock = clock_flag ? CLOCK_REALTIME : CLOCK_MONOTONIC;
        timespec now{}, end{};
        ASSERT_EQ(clock_gettime(clock, &now), 0);
        const timespec deadline = Add(now, 5000000);
        errno = 0;
        const long ret = Wait(&word, FUTEX_WAIT_BITSET_PRIVATE | clock_flag,
                              0, &deadline);
        const int error = errno;
        ASSERT_EQ(clock_gettime(clock, &end), 0);
        EXPECT_EQ(ret, -1);
        EXPECT_EQ(error, ETIMEDOUT);
        EXPECT_GE(Ns(end), Ns(deadline));
    }
}

TEST(FutexWaitDeadline, ZeroBitsetRemainsInvalid) {
    int word = 1;
    const timespec past{0, 0};
    errno = 0;
    EXPECT_EQ(Wait(&word, FUTEX_WAIT_BITSET_PRIVATE, 0, &past, 0), -1);
    EXPECT_EQ(errno, EINVAL);
}

constexpr int64_t kMs = 1000000;
int64_t Now(clockid_t clock = CLOCK_MONOTONIC) {
    timespec t{};
    return clock_gettime(clock, &t) == 0 ? Ns(t) : -1;
}
timespec At(int64_t ns) { return {ns / 1000000000, ns % 1000000000}; }
bool Until(const std::function<bool()>& test, int64_t budget) {
    const int64_t start = Now();
    if (start < 0) return false;
    do {
        if (test()) return true;
        usleep(1000);
        const int64_t now = Now();
        if (now < 0 || now - start >= budget) return false;
    } while (true);
}
// Isolate hung joins and failed assertions. ECHILD means ownership is gone:
// never send a signal to that numeric PID afterwards.
bool Isolated(const std::function<void()>& body) {
    fflush(nullptr);
    const pid_t pid = fork();
    if (pid < 0) return false;
    if (pid == 0) {
        alarm(6);
        body();
        fflush(nullptr);
        _exit(::testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    bool reaped = false, lost = false;
    const bool completed = Until([&] {
        const pid_t r = waitpid(pid, &status, WNOHANG);
        if (r == pid) reaped = true;
        if (r < 0 && errno == ECHILD) lost = true;
        return reaped || lost;
    }, 8000 * kMs);
    if (!reaped && !lost) {
        // Verify still owned immediately before timeout cleanup.
        const pid_t r = waitpid(pid, &status, WNOHANG);
        if (r == pid) reaped = true;
        else if (r == 0) {
            kill(pid, SIGKILL);
            while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {}
        }
    }
    return completed && reaped && WIFEXITED(status) && WEXITSTATUS(status) == 0;
}
bool Blocked(pid_t tid) {
    if (tid <= 0) return false;
    char path[96];
    snprintf(path, sizeof(path), "/proc/self/task/%d/status", tid);
    FILE* f = fopen(path, "r");
    if (!f) return false;
    char line[256];
    bool blocked = false;
    while (fgets(line, sizeof(line), f)) {
        if (strncmp(line, "State:", 6) == 0) {
            const char* p = line + 6;
            while (*p == ' ' || *p == '\t') ++p;
            blocked = strncmp(p, "Blocked(true)", 13) == 0 ||
                      (*p == 'S' && (p[1] == ' ' || p[1] == '\t'));
            break;
        }
    }
    fclose(f);
    return blocked;
}
struct WaitOperation {
    int* word;
    std::atomic<pid_t> tid{0};
    std::atomic<bool> done{false};
    long result = -2;
    int error = 0;
    int64_t end = -1;
    std::thread worker;
    WaitOperation(int* address, int op, timespec timeout)
        : word(address), worker([this, op, timeout] {
              tid.store(static_cast<pid_t>(syscall(SYS_gettid)), std::memory_order_release);
              result = Wait(word, op, 0, &timeout);
              error = errno;
              end = Now();
              done.store(true, std::memory_order_release);
          }) {}
    bool Registered() {
        return Until([&] { return !done.load(std::memory_order_acquire) &&
                                 Blocked(tid.load(std::memory_order_acquire)); }, 1000 * kMs);
    }
    ~WaitOperation() {
        syscall(SYS_futex, word, FUTEX_WAKE_PRIVATE, INT_MAX, nullptr, nullptr, 0);
        if (worker.joinable()) worker.join();
    }
};
void FortyOneExpire(bool same_key) {
    ASSERT_TRUE(Isolated([same_key] {
        constexpr size_t kWaiters = 41;
        std::array<int, kWaiters> words{};
        std::array<std::unique_ptr<WaitOperation>, kWaiters> operations{};
        const int64_t start = Now();
        ASSERT_GE(start, 0);
        const int64_t deadline_ns = start + 3000 * kMs;
        const timespec deadline = At(deadline_ns);
        for (size_t i = 0; i < kWaiters; ++i) {
            operations[i] = std::make_unique<WaitOperation>(
                &words[same_key ? 0 : i], FUTEX_WAIT_BITSET_PRIVATE, deadline);
        }
        // This is an observed state scan, not an atomic task snapshot. Each
        // worker has exactly one blocking call; no test WAKE occurs before done.
        ASSERT_TRUE(Until([&] {
            for (const auto& op : operations) {
                if (op->done.load(std::memory_order_acquire) ||
                    !Blocked(op->tid.load(std::memory_order_acquire))) {
                    return false;
                }
            }
            for (const auto& op : operations) {
                if (op->done.load(std::memory_order_acquire)) return false;
            }
            const int64_t now = Now();
            return now >= 0 && now < deadline_ns;
        }, 2000 * kMs)) << "all 41 waiters must be observed blocked before expiry";
        const int64_t qualified = Now();
        ASSERT_GE(qualified, 0);
        ASSERT_LT(qualified, deadline_ns);
        ASSERT_TRUE(Until([&] {
            for (const auto& op : operations) {
                if (!op->done.load(std::memory_order_acquire)) return false;
            }
            return true;
        }, deadline_ns + 1500 * kMs - qualified));
        for (size_t i = 0; i < kWaiters; ++i) {
            auto& op = operations[i];
            op->worker.join();
            EXPECT_EQ(op->result, -1) << i;
            EXPECT_EQ(op->error, ETIMEDOUT) << i;
            EXPECT_GE(op->end, deadline_ns) << i;
            // This broad bound is hang protection, not a precision target.
            EXPECT_LT(op->end, deadline_ns + 1500 * kMs) << i;
        }
        // All waits completed naturally before checking for residual nodes.
        for (size_t i = 0; i < (same_key ? 1 : kWaiters); ++i) {
            EXPECT_EQ(syscall(SYS_futex, &words[i], FUTEX_WAKE_PRIVATE,
                              INT_MAX, nullptr, nullptr, 0), 0) << i;
        }
    }));
}

TEST(FutexWaitDeadline, SameAbsoluteDeadlineExpiresAllFortyOneWaiters) {
    FortyOneExpire(false);
}

TEST(FutexWaitDeadline, SameKeyAbsoluteDeadlineExpiresAllFortyOneWaiters) {
    FortyOneExpire(true);
}

bool GuestClockEnabled() {
    utsname u{};
    if (uname(&u) || !strstr(u.release, "-dragonos")) return false;
    FILE* f = fopen("/proc/version_signature", "r");
    if (!f) return false;
    char text[64]{};
    const bool dragon = fgets(text, sizeof(text), f) && strncmp(text, "DragonOS ", 9) == 0;
    fclose(f);
    const char* opt = getenv("DUNITEST_FUTEX_CLOCK_SET");
    return dragon && opt && strcmp(opt, "1") == 0;
}
class RestoreRealtime {
public:
    int64_t real = Now(CLOCK_REALTIME), mono = Now();
    bool Restore() {
        const int64_t now = Now();
        if (real < 0 || mono < 0 || now < mono) return false;
        const timespec t = At(real + now - mono);
        return clock_settime(CLOCK_REALTIME, &t) == 0;
    }
    ~RestoreRealtime() { EXPECT_TRUE(Restore()); }
};
void ClockScenario(bool monotonic, bool forward) {
    int word = 0;
    const int64_t begin = Now();
    const clockid_t domain = monotonic ? CLOCK_MONOTONIC : CLOCK_REALTIME;
    const int64_t budget = !monotonic && forward ? 2000 * kMs : 300 * kMs;
    const int64_t domain_start = Now(domain);
    ASSERT_GE(begin, 0);
    ASSERT_GE(domain_start, 0);
    WaitOperation op(&word, FUTEX_WAIT_BITSET_PRIVATE |
                            (monotonic ? 0 : FUTEX_CLOCK_REALTIME),
                     At(domain_start + budget));
    ASSERT_TRUE(op.Registered()) << "missing proc state/registration is a failure";
    ASSERT_LT(Now() - begin, budget);
    const int64_t saved_real = Now(CLOCK_REALTIME), saved_mono = Now();
    ASSERT_GE(saved_real, 0);
    const timespec shifted = At(saved_real + (forward ? 4000 : -2000) * kMs);
    ASSERT_EQ(clock_settime(CLOCK_REALTIME, &shifted), 0);
    if (!monotonic && !forward) {
        ASSERT_TRUE(Until([&] { return Now() >= begin + 700 * kMs; }, 1000 * kMs));
        EXPECT_FALSE(op.done.load(std::memory_order_acquire));
        const timespec restored = At(saved_real + Now() - saved_mono);
        ASSERT_EQ(clock_settime(CLOCK_REALTIME, &restored), 0);
    }
    ASSERT_TRUE(Until([&] { return op.done.load(std::memory_order_acquire); }, 1200 * kMs));
    op.worker.join();
    EXPECT_EQ(op.result, -1);
    EXPECT_EQ(op.error, ETIMEDOUT);
    if (monotonic) { EXPECT_GE(op.end - begin, budget); }
    if (!monotonic && !forward) { EXPECT_GE(op.end - begin, 700 * kMs); }
    // These broad bounds distinguish multi-second clock-domain mistakes;
    // they are watchdog/semantic bounds, not a realtime latency guarantee.
    if (forward || monotonic) { EXPECT_LT(op.end - begin, 1500 * kMs); }
}
class FutexWaitClockStep : public ::testing::TestWithParam<int> {};
TEST_P(FutexWaitClockStep, RegisteredAbsoluteWaitTracksClockDomain) {
    if (!GuestClockEnabled())
        GTEST_SKIP() << "owned DragonOS guest only; DUNITEST_FUTEX_CLOCK_SET=1; never host";
    RestoreRealtime restore; // Parent restores even after child alarm/kill.
    ASSERT_GE(restore.real, 0);
    ASSERT_GE(restore.mono, 0);
    const int mode = GetParam();
    EXPECT_TRUE(Isolated([=] { ClockScenario((mode & 2) != 0, (mode & 1) != 0); }));
}
INSTANTIATE_TEST_SUITE_P(OwnedGuest, FutexWaitClockStep, ::testing::Values(0, 1, 2, 3));

TEST(FutexWaitDeadline, RequeueTimeoutRetiresDestinationWithOriginalDeadline) {
    EXPECT_TRUE(Isolated([] {
        int source = 0, destination = 0;
        const int64_t start = Now();
        WaitOperation op(&source, FUTEX_WAIT_PRIVATE, {1, 0});
        ASSERT_TRUE(op.Registered());
        ASSERT_TRUE(Until([&] { return Now() >= start + 700 * kMs; }, 1000 * kMs));
        ASSERT_FALSE(op.done.load(std::memory_order_acquire));
        const long moved = syscall(SYS_futex, &source, FUTEX_CMP_REQUEUE_PRIVATE, 0,
                                   reinterpret_cast<void*>(1), &destination, 0);
        ASSERT_EQ(moved, 1);
        ASSERT_TRUE(Until([&] { return op.done.load(std::memory_order_acquire); }, 1500 * kMs));
        op.worker.join();
        EXPECT_EQ(op.result, -1);
        EXPECT_EQ(op.error, ETIMEDOUT);
        EXPECT_GE(op.end - start, 1000 * kMs);
        // A reset to a fresh 1s budget at requeue would finish after 1.7s.
        // Broad discrimination bound, not an advertised realtime guarantee.
        EXPECT_LT(op.end - start, 1500 * kMs);
        EXPECT_EQ(syscall(SYS_futex, &source, FUTEX_WAKE_PRIVATE, INT_MAX, nullptr, nullptr, 0), 0);
        EXPECT_EQ(syscall(SYS_futex, &destination, FUTEX_WAKE_PRIVATE, INT_MAX, nullptr, nullptr, 0), 0);
    }));
}
TEST(FutexWaitDeadline, WakeAndTimeoutRaceHasOneWinnerAndNoResidualWaiter) {
    EXPECT_TRUE(Isolated([] {
        for (int i = 0; i < 8; ++i) {
            int word = 0;
            const int64_t start = Now();
            WaitOperation op(&word, FUTEX_WAIT_PRIVATE, {0, 100000000});
            ASSERT_TRUE(op.Registered());
            // Fixed alternate early/near-deadline wake attempts; not a claim
            // that the scheduler must realize a particular race ordering.
            if (i & 1) { ASSERT_TRUE(Until([&] { return Now() >= start + 100 * kMs; }, 500 * kMs)); }
            const long woke = syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, 1, nullptr, nullptr, 0);
            ASSERT_TRUE(woke == 0 || woke == 1);
            ASSERT_TRUE(Until([&] { return op.done.load(std::memory_order_acquire); }, 1000 * kMs));
            op.worker.join();
            if (woke == 1) { EXPECT_EQ(op.result, 0); }
            else { EXPECT_EQ(op.result, -1); EXPECT_EQ(op.error, ETIMEDOUT); }
            EXPECT_EQ(syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, INT_MAX, nullptr, nullptr, 0), 0);
        }
    }));
}
void NoRestartSignal(int) {}
TEST(FutexWaitDeadline, RegisteredWaitIsInterruptedByUnblockedSignal) {
    EXPECT_TRUE(Isolated([] {
        struct sigaction action{};
        action.sa_handler = NoRestartSignal;
        sigemptyset(&action.sa_mask);
        ASSERT_EQ(sigaction(SIGUSR1, &action, nullptr), 0);
        sigset_t set;
        sigemptyset(&set); sigaddset(&set, SIGUSR1);
        ASSERT_EQ(pthread_sigmask(SIG_UNBLOCK, &set, nullptr), 0);
        int word = 0;
        WaitOperation op(&word, FUTEX_WAIT_PRIVATE, {2, 0});
        ASSERT_TRUE(op.Registered());
        ASSERT_EQ(pthread_kill(op.worker.native_handle(), SIGUSR1), 0);
        ASSERT_TRUE(Until([&] { return op.done.load(std::memory_order_acquire); }, 1000 * kMs));
        op.worker.join();
        EXPECT_EQ(op.result, -1); EXPECT_EQ(op.error, EINTR);
        EXPECT_EQ(syscall(SYS_futex, &word, FUTEX_WAKE_PRIVATE, INT_MAX, nullptr, nullptr, 0), 0);
    }));
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
