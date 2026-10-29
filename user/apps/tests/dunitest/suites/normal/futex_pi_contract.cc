#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <functional>
#include <linux/futex.h>
#include <new>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/utsname.h>
#include <sys/wait.h>
#include <thread>
#include <time.h>
#include <unistd.h>

#ifndef FUTEX_LOCK_PI2
#define FUTEX_LOCK_PI2 13
#endif

namespace {

constexpr int64_t kMs = 1000000;
uint32_t Tid() { return static_cast<uint32_t>(syscall(SYS_gettid)); }

int64_t Now(clockid_t clock = CLOCK_MONOTONIC) {
    timespec value = {};
    if (clock_gettime(clock, &value)) return -1;
    return int64_t(value.tv_sec) * 1000000000 + value.tv_nsec;
}

timespec At(int64_t ns) { return {ns / 1000000000, ns % 1000000000}; }
timespec After(clockid_t clock, int64_t ns) { return At(Now(clock) + ns); }

long Futex(void* word, int operation, const timespec* timeout = nullptr,
           int value = 0) {
    return syscall(SYS_futex, word, operation, value, timeout, nullptr, 0);
}

bool Await(const std::function<bool()>& predicate, int64_t duration = 2000 * kMs) {
    const int64_t end = Now() + duration;
    do {
        if (predicate()) return true;
        usleep(1000);
    } while (Now() < end);
    return predicate();
}

// A child timeout, assertion or baseline kernel bug cannot strand a joinable
// thread in the test runner. The parent's bound uses CLOCK_MONOTONIC.
bool Isolated(const std::function<void()>& test, bool* skipped = nullptr) {
    fflush(nullptr);
    const pid_t child = fork();
    if (child < 0) return false;
    if (child == 0) {
        alarm(6);
        test();
        fflush(nullptr);
        _exit(::testing::Test::HasFailure() ? 1 : 0);
    }
    int status = 0;
    const bool done = Await([&] {
        const pid_t result = waitpid(child, &status, WNOHANG);
        return result == child;
    }, 8000 * kMs);
    if (!done) {
        kill(child, SIGKILL);
        while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    }
    if (done && WIFEXITED(status) && WEXITSTATUS(status) == 77 && skipped) {
        *skipped = true;
        return true;
    }
    return done && WIFEXITED(status) && WEXITSTATUS(status) == 0;
}

bool Registered(std::atomic<uint32_t>& word) {
    return Await([&] { return (word.load() & FUTEX_WAITERS) != 0; });
}

int ReadByte(int fd, int timeout_ms = 2500) {
    const int64_t end = Now() + int64_t(timeout_ms) * kMs;
    while (Now() < end) {
        pollfd descriptor = {fd, POLLIN, 0};
        const int remaining = static_cast<int>((end - Now() + kMs - 1) / kMs);
        const int result = poll(&descriptor, 1, remaining > 0 ? remaining : 0);
        if (result < 0 && errno == EINTR) continue;
        if (result <= 0) return -1;
        unsigned char byte = 0;
        if (read(fd, &byte, 1) == 1) return byte;
        return -1;
    }
    return -1;
}

// Entered only through this test executable's explicit helper argument. Stay
// alive after exec so recovering the old shared lock cannot be blamed on exit.
int ExecHelper(int command_fd, int reply_fd) {
    alarm(4);
    std::atomic<uint32_t> fresh{0};
    const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
    if (Futex(&fresh, FUTEX_LOCK_PI_PRIVATE, &limit)) return 81;
    long acquired = -2;
    std::thread waiter([&] {
        const timespec deadline = After(CLOCK_REALTIME, 2000 * kMs);
        acquired = Futex(&fresh, FUTEX_LOCK_PI_PRIVATE, &deadline);
        if (acquired == 0) Futex(&fresh, FUTEX_UNLOCK_PI_PRIVATE);
    });
    const bool registered = Registered(fresh);
    const long unlocked = Futex(&fresh, FUTEX_UNLOCK_PI_PRIVATE);
    waiter.join();
    if (!registered || unlocked != 0 || acquired != 0 || fresh.load() != 0) return 82;
    // A fresh contender attached to this same task in the new mm: the exec
    // cleanup's lifecycle must have returned from Cleaning to Alive.
    if (write(reply_fd, "R", 1) != 1) return 83;
    return ReadByte(command_fd) == 'X' ? 0 : 84;
}

TEST(PiExecContract, ContendedOwnerExecRecoversOldLockAndAcceptsNewState) {
    ASSERT_TRUE(Isolated([] {
        const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
        void* mapping = mmap(nullptr, page, PROT_READ | PROT_WRITE,
                             MAP_SHARED | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, mapping);
        auto* word = new (mapping) std::atomic<uint32_t>(0);
        int command[2], reply[2];
        ASSERT_EQ(0, pipe(command));
        ASSERT_EQ(0, pipe(reply));
        const pid_t owner = fork();
        ASSERT_GE(owner, 0);
        if (owner == 0) {
            close(command[1]);
            close(reply[0]);
            alarm(5);
            const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
            if (Futex(word, FUTEX_LOCK_PI, &limit)) _exit(85);
            if (write(reply[1], "L", 1) != 1 || ReadByte(command[0]) != 'E') _exit(86);
            char command_arg[24], reply_arg[24];
            snprintf(command_arg, sizeof(command_arg), "%d", command[0]);
            snprintf(reply_arg, sizeof(reply_arg), "%d", reply[1]);
            execl("/proc/self/exe", "futex_pi_contract_test", "--pi-exec-helper",
                  command_arg, reply_arg, static_cast<char*>(nullptr));
            _exit(87);
        }
        close(command[0]);
        close(reply[1]);
        ASSERT_EQ('L', ReadByte(reply[0]));
        long acquired = -2;
        uint32_t observed = 0;
        std::atomic<bool> finished{false};
        std::thread waiter([&] {
            const timespec deadline = After(CLOCK_REALTIME, 3000 * kMs);
            acquired = Futex(word, FUTEX_LOCK_PI, &deadline);
            observed = word->load();
            if (acquired == 0) Futex(word, FUTEX_UNLOCK_PI);
            finished.store(true);
        });
        const bool registered = Registered(*word);
        EXPECT_EQ(1, write(command[1], "E", 1));
        const int helper_ready = ReadByte(reply[0]);
        const bool recovered_before_exit = Await([&] { return finished.load(); }, 1000 * kMs);
        int status = 0;
        EXPECT_EQ(0, waitpid(owner, &status, WNOHANG));
        EXPECT_EQ(1, write(command[1], "X", 1));
        waiter.join();
        EXPECT_EQ(owner, waitpid(owner, &status, 0));
        EXPECT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
        EXPECT_TRUE(registered);
        EXPECT_EQ('R', helper_ready);
        EXPECT_TRUE(recovered_before_exit);
        EXPECT_EQ(0, acquired);
        EXPECT_NE(0u, observed & FUTEX_OWNER_DIED);
        EXPECT_EQ(0u, word->load());
        EXPECT_EQ(0, close(command[1]));
        EXPECT_EQ(0, close(reply[0]));
        EXPECT_EQ(0, munmap(mapping, page));
    }));
}

class PiContract : public ::testing::TestWithParam<int> {};

TEST_P(PiContract, RawOwnerDiedSucceedsAndOrdinaryUnlockClearsIt) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] {
        for (int operation : {FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI}) {
            std::atomic<uint32_t> word{FUTEX_OWNER_DIED};
            const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
            ASSERT_EQ(0, Futex(&word, operation | flags, &limit));
            EXPECT_EQ(Tid(), word.load() & FUTEX_TID_MASK);
            EXPECT_NE(0u, word.load() & FUTEX_OWNER_DIED);
            EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
            EXPECT_EQ(0u, word.load());
        }
    }));
}

TEST_P(PiContract, OrdinaryHandoffClearsOwnerDied) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] {
        std::atomic<uint32_t> word{FUTEX_OWNER_DIED};
        const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
        ASSERT_EQ(0, Futex(&word, FUTEX_LOCK_PI | flags, &limit));
        long result = -2;
        uint32_t observed = 0;
        std::thread waiter([&] {
            const timespec deadline = After(CLOCK_REALTIME, 2000 * kMs);
            result = Futex(&word, FUTEX_LOCK_PI | flags, &deadline);
            observed = word.load();
            if (result == 0) Futex(&word, FUTEX_UNLOCK_PI | flags);
        });
        const bool registered = Registered(word);
        EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
        waiter.join();
        EXPECT_TRUE(registered);
        EXPECT_EQ(0, result);
        EXPECT_EQ(0u, observed & FUTEX_OWNER_DIED);
        EXPECT_EQ(0u, word.load());
    }));
}

TEST_P(PiContract, LastSlowOwnerRetiresBeforeUserspaceRelockAndFreshContention) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] {
        std::atomic<uint32_t> word{0};
        const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
        ASSERT_EQ(0, Futex(&word, FUTEX_LOCK_PI | flags, &limit));
        long first_acquired = -2, first_unlocked = -2;
        std::thread first([&] {
            const timespec deadline = After(CLOCK_REALTIME, 2000 * kMs);
            first_acquired = Futex(&word, FUTEX_LOCK_PI | flags, &deadline);
            if (first_acquired == 0) {
                first_unlocked = Futex(&word, FUTEX_UNLOCK_PI | flags);
            }
        });
        const bool first_registered = Registered(word);
        EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
        first.join();
        ASSERT_TRUE(first_registered);
        ASSERT_EQ(0, first_acquired);
        ASSERT_EQ(0, first_unlocked);
        ASSERT_EQ(0u, word.load());

        // Only acquire through the legal uncontended userspace fast path.
        // The preceding slow owner's syscall cleared WAITERS along with its
        // ownership; never discard a live waiter hint with a blind store.
        uint32_t expected = 0;
        ASSERT_TRUE(word.compare_exchange_strong(expected, Tid()));
        long fresh_acquired = -2, fresh_unlocked = -2;
        std::thread fresh([&] {
            const timespec deadline = After(CLOCK_REALTIME, 2000 * kMs);
            fresh_acquired = Futex(&word, FUTEX_LOCK_PI | flags, &deadline);
            if (fresh_acquired == 0) {
                fresh_unlocked = Futex(&word, FUTEX_UNLOCK_PI | flags);
            }
        });
        const bool fresh_registered = Registered(word);
        EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
        fresh.join();
        EXPECT_TRUE(fresh_registered);
        EXPECT_EQ(0, fresh_acquired);
        EXPECT_EQ(0, fresh_unlocked);
        EXPECT_EQ(0u, word.load());
    }));
}

TEST_P(PiContract, ReapedOwnerReturnsEsrchForLockAndTrylock) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] {
        const pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) _exit(0);
        ASSERT_EQ(child, waitpid(child, nullptr, 0));
        errno = 0;
        ASSERT_EQ(-1, kill(child, 0));
        ASSERT_EQ(ESRCH, errno);
        for (int operation : {FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI}) {
            std::atomic<uint32_t> word{static_cast<uint32_t>(child)};
            const timespec deadline = After(CLOCK_REALTIME, 500 * kMs);
            errno = 0;
            EXPECT_EQ(-1, Futex(&word, operation | flags, &deadline));
            EXPECT_EQ(ESRCH, errno);
        }
    }));
}

TEST_P(PiContract, ExitedUnreapedOwnerReturnsEsrchForLockAndTrylock) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] {
        const pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) _exit(0);
        // WNOWAIT confirms exit cleanup completed while preserving the zombie
        // task. Its absent mm does not make it a kernel-thread owner (EPERM).
        siginfo_t info = {};
        ASSERT_EQ(0, waitid(P_PID, child, &info, WEXITED | WNOWAIT));
        ASSERT_EQ(child, info.si_pid);
        ASSERT_EQ(CLD_EXITED, info.si_code);
        for (int operation : {FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI}) {
            std::atomic<uint32_t> word{static_cast<uint32_t>(child)};
            const timespec deadline = After(CLOCK_REALTIME, 500 * kMs);
            errno = 0;
            EXPECT_EQ(-1, Futex(&word, operation | flags, &deadline));
            EXPECT_EQ(ESRCH, errno);
        }
        EXPECT_EQ(child, waitpid(child, nullptr, 0));
    }));
}

TEST_P(PiContract, ExistingStateRejectsOwnerDiedWithMismatchingNonzeroTid) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] {
        std::atomic<uint32_t> word{0};
        std::atomic<uint32_t> waiter_tid{0};
        const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
        ASSERT_EQ(0, Futex(&word, FUTEX_LOCK_PI | flags, &limit));
        long acquired = -2;
        std::thread waiter([&] {
            waiter_tid.store(Tid());
            const timespec deadline = After(CLOCK_REALTIME, 3000 * kMs);
            acquired = Futex(&word, FUTEX_LOCK_PI | flags, &deadline);
            if (acquired == 0) Futex(&word, FUTEX_UNLOCK_PI | flags);
        });
        const bool registered = Registered(word);
        const uint32_t original = word.load();
        if (registered) {
            EXPECT_NE(Tid(), waiter_tid.load());
            EXPECT_NE(0u, waiter_tid.load());
            // OWNER_DIED permits a zero-TID transition during owner cleanup;
            // it never excuses a different nonzero owner for existing state.
            word.store(FUTEX_OWNER_DIED | waiter_tid.load());
            for (int operation : {FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI}) {
                const timespec deadline = After(CLOCK_REALTIME, 500 * kMs);
                errno = 0;
                EXPECT_EQ(-1, Futex(&word, operation | flags, &deadline));
                EXPECT_EQ(EINVAL, errno);
            }
            word.store(original);
        }
        EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
        waiter.join();
        EXPECT_TRUE(registered);
        EXPECT_EQ(0, acquired);
        EXPECT_EQ(0u, word.load());
    }));
}

TEST_P(PiContract, AttachedNonrobustOwnerExitRecoversKernelState) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] {
        std::atomic<uint32_t> word{0};
        std::atomic<int> ready{0};
        std::atomic<bool> registered{false};
        std::thread owner([&] {
            const timespec deadline = After(CLOCK_REALTIME, 1000 * kMs);
            if (Futex(&word, FUTEX_LOCK_PI | flags, &deadline)) {
                ready.store(-1);
                return;
            }
            ready.store(1);
            registered.store(Registered(word));
            // This raw word is never linked into the libc robust list.
            // Exiting here must release an already attached kernel PI state.
        });
        const bool owner_ready = Await([&] { return ready.load() != 0; });
        if (!owner_ready || ready.load() != 1) {
            owner.join();
            FAIL() << "owner could not acquire lock";
        }
        const timespec deadline = After(CLOCK_REALTIME, 3000 * kMs);
        const long result = Futex(&word, FUTEX_LOCK_PI | flags, &deadline);
        const int error = errno;
        owner.join();
        EXPECT_TRUE(registered.load());
        EXPECT_EQ(0, result) << "errno=" << error;
        if ((word.load() & FUTEX_TID_MASK) == Tid()) {
            EXPECT_NE(0u, word.load() & FUTEX_OWNER_DIED);
            EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
        }
    }));
}

static_assert(std::atomic<int>::is_always_lock_free);
static_assert(std::atomic<int64_t>::is_always_lock_free);
std::atomic<int> handler_count{0};
std::atomic<int64_t> handler_until{0};
std::atomic<int64_t> handler_finished{0};

void Handler(int) {
    handler_count.fetch_add(1, std::memory_order_relaxed);
    // Explicitly consume time across the absolute deadline in the second
    // signal test. clock_gettime is async-signal-safe; no locks/allocations.
    while (Now() < handler_until.load(std::memory_order_relaxed)) {}
    handler_finished.store(Now(), std::memory_order_release);
}

void SignalCase(int flags, bool cross_deadline) {
    struct sigaction action = {};
    action.sa_handler = Handler;
    sigemptyset(&action.sa_mask);
    action.sa_flags = 0;
    ASSERT_EQ(0, sigaction(SIGUSR1, &action, nullptr));
    handler_count.store(0);
    handler_until.store(0);
    handler_finished.store(0);
    std::atomic<uint32_t> word{0};
    const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
    ASSERT_EQ(0, Futex(&word, FUTEX_LOCK_PI | flags, &limit));
    long result = -2;
    int error = 0;
    int64_t finished = 0;
    std::atomic<int64_t> waiter_deadline{0};
    std::thread waiter([&] {
        waiter_deadline.store(Now() + 700 * kMs);
        const timespec deadline = After(CLOCK_REALTIME,
                                        (cross_deadline ? 700 : 3000) * kMs);
        result = Futex(&word, FUTEX_LOCK_PI | flags, &deadline);
        error = errno;
        finished = Now();
        if (result == 0) Futex(&word, FUTEX_UNLOCK_PI | flags);
    });
    const bool registered = Registered(word);
    if (cross_deadline) handler_until.store(waiter_deadline.load() + 500 * kMs);
    if (registered) {
        EXPECT_EQ(0, pthread_kill(waiter.native_handle(), SIGUSR1));
    }
    const bool handled = Await([&] { return handler_finished.load() != 0; });
    if (!cross_deadline) {
        EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
    }
    waiter.join();
    EXPECT_TRUE(registered);
    EXPECT_TRUE(handled);
    EXPECT_EQ(1, handler_count.load());
    if (cross_deadline) {
        EXPECT_EQ(-1, result);
        EXPECT_EQ(ETIMEDOUT, error);
        EXPECT_GE(finished, handler_finished.load());
        // Renewing the original 700ms budget after the 1.2s handler is wrong.
        EXPECT_LT(finished - handler_finished.load(), 400 * kMs);
        EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
    } else {
        EXPECT_EQ(0, result) << "LOCK_PI must restart even without SA_RESTART; errno=" << error;
    }
}

TEST_P(PiContract, SignalWithoutSaRestartTransparentlyRestarts) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] { SignalCase(flags, false); }));
}

TEST_P(PiContract, SignalHandlerDoesNotRenewAbsoluteDeadline) {
    const int flags = GetParam();
    ASSERT_TRUE(Isolated([=] { SignalCase(flags, true); }));
}

TEST(PiKeyContract, UnmappedSharedAndPrivateAddressRange) {
    ASSERT_TRUE(Isolated([] {
        const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
        void* address = mmap(nullptr, page, PROT_READ | PROT_WRITE,
                             MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, address);
        ASSERT_EQ(0, munmap(address, page));
        const timespec limit = After(CLOCK_REALTIME, 500 * kMs);
        for (int operation : {FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI, FUTEX_WAKE}) {
            errno = 0;
            EXPECT_EQ(-1, Futex(address, operation, &limit));
            EXPECT_EQ(EFAULT, errno);
        }
        // Linux private WAKE uses only mm+VA, but still checks access_ok.
        EXPECT_EQ(0, Futex(address, FUTEX_WAKE_PRIVATE));
        void* kernel_range = reinterpret_cast<void*>(~uintptr_t(4095));
        errno = 0;
        EXPECT_EQ(-1, Futex(kernel_range, FUTEX_WAKE_PRIVATE));
        EXPECT_EQ(EFAULT, errno);
        errno = 0;
        EXPECT_EQ(-1, Futex(reinterpret_cast<void*>(~uintptr_t(4095) + 1),
                             FUTEX_WAKE_PRIVATE));
        EXPECT_EQ(EINVAL, errno);
    }));
}

TEST(PiKeyContract, ReadonlyFileAndAnonymousHaveDifferentSharedReadContracts) {
    ASSERT_TRUE(Isolated([] {
        const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
        char name[] = "/tmp/pi_contract_XXXXXX";
        const int fd = mkstemp(name);
        ASSERT_GE(fd, 0);
        ASSERT_EQ(0, unlink(name));
        ASSERT_EQ(0, ftruncate(fd, page));
        void* file = mmap(nullptr, page, PROT_READ, MAP_SHARED, fd, 0);
        void* anonymous = mmap(nullptr, page, PROT_READ,
                               MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        void* none = mmap(nullptr, page, PROT_NONE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, file);
        ASSERT_NE(MAP_FAILED, anonymous);
        ASSERT_NE(MAP_FAILED, none);
        EXPECT_EQ(0, Futex(file, FUTEX_WAKE));
        errno = 0;
        EXPECT_EQ(-1, Futex(file, FUTEX_WAIT, nullptr, 1));
        EXPECT_EQ(EAGAIN, errno);
        for (void* address : {anonymous, none}) {
            for (int operation : {FUTEX_WAKE, FUTEX_WAIT}) {
                errno = 0;
                EXPECT_EQ(-1, Futex(address, operation, nullptr, 1));
                EXPECT_EQ(EFAULT, errno);
            }
        }
        EXPECT_EQ(0, Futex(anonymous, FUTEX_WAKE_PRIVATE));
        for (void* address : {file, anonymous, none}) {
            for (int flags : {0, FUTEX_PRIVATE_FLAG}) {
                for (int operation : {FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI}) {
                    const timespec limit = After(CLOCK_REALTIME, 500 * kMs);
                    errno = 0;
                    EXPECT_EQ(-1, Futex(address, operation | flags, &limit));
                    EXPECT_EQ(EFAULT, errno);
                }
            }
        }
        EXPECT_EQ(0, munmap(file, page));
        EXPECT_EQ(0, munmap(anonymous, page));
        EXPECT_EQ(0, munmap(none, page));
        EXPECT_EQ(0, close(fd));
    }));
}

bool GuestClockTestsEnabled() {
    utsname identity = {};
    // DragonOS deliberately reports Linux in sysname for binary compatibility.
    // Require both its kernel release and proc signature, plus explicit opt-in.
    if (uname(&identity) || !strstr(identity.release, "-dragonos")) return false;
    FILE* signature = fopen("/proc/version_signature", "r");
    if (!signature) return false;
    char text[64] = {};
    const bool dragonos = fgets(text, sizeof(text), signature) &&
                          strncmp(text, "DragonOS ", 9) == 0;
    fclose(signature);
    const char* enabled = getenv("DUNITEST_PI_CLOCK_SET");
    return dragonos && enabled && strcmp(enabled, "1") == 0;
}

void ClockCase(bool monotonic, bool forward) {
    const int64_t saved_real = Now(CLOCK_REALTIME);
    const int64_t saved_mono = Now();
    std::atomic<uint32_t> word{0};
    const timespec owner_limit = After(CLOCK_REALTIME, 1000 * kMs);
    ASSERT_EQ(0, Futex(&word, FUTEX_LOCK_PI_PRIVATE, &owner_limit));
    std::atomic<bool> finished{false};
    long result = -2;
    int error = 0;
    int64_t elapsed = 0;
    const int64_t start = Now();
    const int64_t budget = !monotonic && forward ? 2000 * kMs : 300 * kMs;
    std::thread waiter([&] {
        const timespec deadline = After(monotonic ? CLOCK_MONOTONIC : CLOCK_REALTIME,
                                        budget);
        result = Futex(&word, (monotonic ? FUTEX_LOCK_PI2 : FUTEX_LOCK_PI) |
                                 FUTEX_PRIVATE_FLAG, &deadline);
        error = errno;
        elapsed = Now() - start;
        if (result == 0) Futex(&word, FUTEX_UNLOCK_PI_PRIVATE);
        finished.store(true, std::memory_order_release);
    });
    const bool registered = Registered(word);
    const timespec shifted = At(Now(CLOCK_REALTIME) + (forward ? 4000 : -2000) * kMs);
    const int set_result = registered ? clock_settime(CLOCK_REALTIME, &shifted) : -1;
    EXPECT_TRUE(registered);
    EXPECT_EQ(0, set_result) << strerror(errno);
    if (!monotonic && !forward && set_result == 0) {
        // Wait past the *original* absolute deadline, not an inferred enqueue.
        Await([&] { return Now() >= start + 700 * kMs; }, 1000 * kMs);
        EXPECT_FALSE(finished.load());
        const timespec restored = At(saved_real + Now() - saved_mono);
        EXPECT_EQ(0, clock_settime(CLOCK_REALTIME, &restored));
    }
    const bool timed_out = Await([&] { return finished.load(); }, 1200 * kMs);
    // Release before join even on a broken baseline, preserving bounded cleanup.
    EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI_PRIVATE));
    waiter.join();
    EXPECT_TRUE(timed_out);
    EXPECT_EQ(-1, result);
    EXPECT_EQ(ETIMEDOUT, error);
    if (monotonic) {
        EXPECT_GE(elapsed, 250 * kMs);
        EXPECT_LT(elapsed, 1200 * kMs);
    } else if (forward) {
        EXPECT_LT(elapsed, 1200 * kMs);
    } else {
        EXPECT_GE(elapsed, 650 * kMs);
    }
}

class PiClockContract : public ::testing::TestWithParam<int> {};
TEST_P(PiClockContract, AbsoluteClockTracksOnlyItsOwnDomain) {
    if (!GuestClockTestsEnabled()) {
        GTEST_SKIP() << "requires disposable DragonOS guest and DUNITEST_PI_CLOCK_SET=1; never sets host clock";
    }
    // fork does not isolate the wall clock. The supervising parent restores it
    // even if the child asserts, receives its alarm, or has to be killed.
    const int64_t saved_real = Now(CLOCK_REALTIME);
    const int64_t saved_mono = Now();
    const int mode = GetParam();
    const bool passed = Isolated([=] { ClockCase((mode & 2) != 0, (mode & 1) != 0); });
    const timespec restored = At(saved_real + Now() - saved_mono);
    EXPECT_EQ(0, clock_settime(CLOCK_REALTIME, &restored));
    EXPECT_TRUE(passed);
}

void NamespaceAlarm(int) { _exit(92); }

void NamespaceExercise(pid_t external_pid) {
    // The same init task has a real outside PID, but TID 1 in its active PID
    // namespace. All owner words and lookups below must use this inner view.
    ASSERT_GT(external_pid, 1);
    ASSERT_EQ(1, getpid());
    ASSERT_EQ(1u, Tid());
    ASSERT_NE(static_cast<uint32_t>(external_pid), Tid());
    std::atomic<uint32_t> word{0};
    std::atomic<uint32_t> owner_tid{0};
    std::atomic<int> ready{0};
    std::atomic<bool> attached{false};
    std::thread owner([&] {
        owner_tid.store(Tid());
        const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
        if (Futex(&word, FUTEX_LOCK_PI_PRIVATE, &limit)) {
            ready.store(-1);
            return;
        }
        ready.store(1);
        attached.store(Registered(word));
        // No robust entry: the namespace-local owner exits while its kernel
        // state is attached. A global numeric PID lookup would find a different
        // task or miss this owner instead of pinning the actual inner thread.
    });
    const bool owner_ready = Await([&] { return ready.load() != 0; });
    if (!owner_ready || ready.load() != 1) {
        owner.join();
        FAIL() << "namespace-local owner could not acquire";
    }
    EXPECT_NE(Tid(), owner_tid.load());
    EXPECT_GT(owner_tid.load(), 1u);
    EXPECT_EQ(owner_tid.load(), word.load() & FUTEX_TID_MASK);
    const timespec deadline = After(CLOCK_REALTIME, 2500 * kMs);
    const long recovered = Futex(&word, FUTEX_LOCK_PI_PRIVATE, &deadline);
    const int error = errno;
    owner.join();
    EXPECT_TRUE(attached.load());
    ASSERT_EQ(0, recovered) << "errno=" << error;
    EXPECT_EQ(Tid(), word.load() & FUTEX_TID_MASK);
    EXPECT_NE(0u, word.load() & FUTEX_OWNER_DIED);
    ASSERT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI_PRIVATE));
    ASSERT_EQ(0u, word.load());

    // Also hand the recovered task's next lock to a fresh inner thread. Both
    // directions must encode the owner's TID in the caller's PID namespace.
    const timespec limit = After(CLOCK_REALTIME, 1000 * kMs);
    ASSERT_EQ(0, Futex(&word, FUTEX_LOCK_PI_PRIVATE, &limit));
    long acquired = -2, unlocked = -2;
    uint32_t fresh_tid = 0, observed = 0;
    std::thread fresh([&] {
        fresh_tid = Tid();
        const timespec limit = After(CLOCK_REALTIME, 2000 * kMs);
        acquired = Futex(&word, FUTEX_LOCK_PI_PRIVATE, &limit);
        observed = word.load() & FUTEX_TID_MASK;
        if (acquired == 0) unlocked = Futex(&word, FUTEX_UNLOCK_PI_PRIVATE);
    });
    const bool registered = Registered(word);
    EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI_PRIVATE));
    fresh.join();
    EXPECT_TRUE(registered);
    EXPECT_NE(Tid(), fresh_tid);
    EXPECT_EQ(0, acquired);
    EXPECT_EQ(fresh_tid, observed);
    EXPECT_EQ(0, unlocked);
    EXPECT_EQ(0u, word.load());
}

TEST(PiNamespaceContract, OwnerLookupExitAndHandoffUseActivePidNamespace) {
    bool skipped = false;
    const bool passed = Isolated([] {
        if (unshare(CLONE_NEWPID)) {
            if (errno == EPERM || errno == EACCES || errno == ENOSYS || errno == EINVAL) {
                _exit(77);
            }
            FAIL() << "unshare(CLONE_NEWPID): " << strerror(errno);
        }
        int identity[2];
        ASSERT_EQ(0, pipe(identity));
        const pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            close(identity[1]);
            // PID-namespace init ignores many default signal dispositions.
            // An explicit handler makes its independent watchdog effective.
            struct sigaction action = {};
            action.sa_handler = NamespaceAlarm;
            sigemptyset(&action.sa_mask);
            if (sigaction(SIGALRM, &action, nullptr)) _exit(93);
            alarm(5);
            pid_t external_pid = 0;
            if (read(identity[0], &external_pid, sizeof(external_pid)) != sizeof(external_pid)) {
                _exit(94);
            }
            close(identity[0]);
            NamespaceExercise(external_pid);
            fflush(nullptr);
            _exit(::testing::Test::HasFailure() ? 1 : 0);
        }
        close(identity[0]);
        EXPECT_EQ(static_cast<ssize_t>(sizeof(child)), write(identity[1], &child, sizeof(child)));
        close(identity[1]);
        int status = 0;
        EXPECT_EQ(child, waitpid(child, &status, 0));
        EXPECT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
    }, &skipped);
    if (skipped) GTEST_SKIP() << "PID namespace creation unavailable or not permitted";
    EXPECT_TRUE(passed);
}

INSTANTIATE_TEST_SUITE_P(PrivateAndShared, PiContract,
                         ::testing::Values(0, FUTEX_PRIVATE_FLAG));
INSTANTIATE_TEST_SUITE_P(RealtimeAndMonotonicForwardAndBackward, PiClockContract,
                         ::testing::Values(0, 1, 2, 3));

} // namespace

int main(int argc, char** argv) {
    if (argc == 4 && strcmp(argv[1], "--pi-exec-helper") == 0) {
        return ExecHelper(atoi(argv[2]), atoi(argv[3]));
    }
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
