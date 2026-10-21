#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <csignal>
#include <cstdint>
#include <cstdlib>
#include <fcntl.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <thread>
#include <time.h>
#include <unistd.h>
#include <vector>

#ifndef FUTEX_LOCK_PI2
#define FUTEX_LOCK_PI2 13
#endif

namespace {

long Futex(std::atomic<uint32_t>* word, int operation,
           const timespec* timeout = nullptr) {
    return syscall(SYS_futex, word, operation, 0, timeout, nullptr, 0);
}

uint32_t Tid() { return static_cast<uint32_t>(syscall(SYS_gettid)); }

timespec After(clockid_t clock, long nanoseconds) {
    timespec now = {};
    if (clock_gettime(clock, &now) != 0) return now;
    now.tv_nsec += nanoseconds;
    now.tv_sec += now.tv_nsec / 1000000000;
    now.tv_nsec %= 1000000000;
    return now;
}

int64_t Now() {
    timespec now = {};
    clock_gettime(CLOCK_MONOTONIC, &now);
    return int64_t(now.tv_sec) * 1000000000 + now.tv_nsec;
}

class PiFutex : public ::testing::TestWithParam<int> {
protected:
    void SetUp() override { alarm(15); }
    void TearDown() override { alarm(0); }
    int Op(int operation) const { return operation | GetParam(); }
    bool Registered(std::atomic<uint32_t>& word) {
        const int64_t end = Now() + 2000000000;
        while (Now() < end) {
            if ((word.load() & FUTEX_WAITERS) != 0) return true;
            usleep(1000);
        }
        return false;
    }
};

TEST_P(PiFutex, BasicOwnershipAndTrylock) {
    std::atomic<uint32_t> word{0};
    ASSERT_EQ(0, Futex(&word, Op(FUTEX_LOCK_PI)));
    EXPECT_EQ(Tid(), word.load() & FUTEX_TID_MASK);
    errno = 0;
    EXPECT_EQ(-1, Futex(&word, Op(FUTEX_LOCK_PI)));
    EXPECT_EQ(EDEADLK, errno);
    errno = 0;
    EXPECT_EQ(-1, Futex(&word, Op(FUTEX_TRYLOCK_PI)));
    EXPECT_EQ(EDEADLK, errno);
    long busy = 0;
    int error = 0;
    std::thread worker([&] {
        busy = Futex(&word, Op(FUTEX_TRYLOCK_PI));
        error = errno;
    });
    worker.join();
    EXPECT_EQ(-1, busy);
    EXPECT_EQ(EAGAIN, error);
    EXPECT_EQ(0, Futex(&word, Op(FUTEX_UNLOCK_PI)));
    EXPECT_EQ(0u, word.load());
    errno = 0;
    EXPECT_EQ(-1, Futex(&word, Op(FUTEX_UNLOCK_PI)));
    EXPECT_EQ(EPERM, errno);
    ASSERT_EQ(0, Futex(&word, Op(FUTEX_TRYLOCK_PI)));
    EXPECT_EQ(0, Futex(&word, Op(FUTEX_UNLOCK_PI)));
}

TEST_P(PiFutex, RegisteredOwnerHandoff) {
    std::atomic<uint32_t> word{0};
    ASSERT_EQ(0, Futex(&word, Op(FUTEX_LOCK_PI)));
    long acquired = -2, released = -2;
    uint32_t observed = 0, worker_tid = 0;
    std::thread worker([&] {
        worker_tid = Tid();
        const timespec deadline = After(CLOCK_REALTIME, 5000000000L);
        acquired = Futex(&word, Op(FUTEX_LOCK_PI), &deadline);
        if (acquired == 0) {
            observed = word.load() & FUTEX_TID_MASK;
            released = Futex(&word, Op(FUTEX_UNLOCK_PI));
        }
    });
    const bool registered = Registered(word);
    EXPECT_EQ(0, Futex(&word, Op(FUTEX_UNLOCK_PI)));
    worker.join();
    EXPECT_TRUE(registered);
    EXPECT_EQ(0, acquired);
    EXPECT_EQ(worker_tid, observed);
    EXPECT_EQ(0, released);
    EXPECT_EQ(0u, word.load());
}

void CheckDeadline(std::atomic<uint32_t>& word, int flags, int operation,
                   clockid_t clock) {
    ASSERT_EQ(0, Futex(&word, FUTEX_LOCK_PI | flags));
    long result = -2;
    int error = 0;
    int64_t elapsed = 0;
    std::atomic<bool> finished{false};
    std::thread worker([&] {
        const timespec deadline = After(clock, 50000000);
        const int64_t start = Now();
        result = Futex(&word, operation | flags, &deadline);
        elapsed = Now() - start;
        error = errno;
        finished.store(true, std::memory_order_release);
        // Bound the broken baseline too: the owner is released below, so a
        // kernel ignoring the timeout cannot leave an unjoinable worker.
        if (result == 0) Futex(&word, FUTEX_UNLOCK_PI | flags);
    });
    const int64_t cleanup_deadline = Now() + 1000000000;
    while (!finished.load(std::memory_order_acquire) && Now() < cleanup_deadline) {
        usleep(1000);
    }
    EXPECT_EQ(0, Futex(&word, FUTEX_UNLOCK_PI | flags));
    worker.join();
    EXPECT_EQ(-1, result);
    EXPECT_EQ(ETIMEDOUT, error);
    EXPECT_GE(elapsed, 40000000);
    EXPECT_EQ(0u, word.load());
}

TEST_P(PiFutex, LockPiUsesAbsoluteRealtime) {
    std::atomic<uint32_t> word{0};
    CheckDeadline(word, GetParam(), FUTEX_LOCK_PI, CLOCK_REALTIME);
}

TEST_P(PiFutex, LockPi2UsesAbsoluteMonotonic) {
    std::atomic<uint32_t> word{0};
    CheckDeadline(word, GetParam(), FUTEX_LOCK_PI2, CLOCK_MONOTONIC);
}

TEST_P(PiFutex, LockPi2SelectsAbsoluteRealtime) {
    std::atomic<uint32_t> word{0};
    CheckDeadline(word, GetParam(), FUTEX_LOCK_PI2 | FUTEX_CLOCK_REALTIME,
                  CLOCK_REALTIME);
}

TEST_P(PiFutex, InvalidTimespecPrecedesAcquisition) {
    for (int operation : {FUTEX_LOCK_PI, FUTEX_LOCK_PI2}) {
        for (timespec invalid : {timespec{-1, 0}, timespec{0, 1000000000}}) {
            std::atomic<uint32_t> word{0};
            errno = 0;
            const long result = Futex(&word, Op(operation), &invalid);
            const int error = errno;
            if (result == 0) Futex(&word, Op(FUTEX_UNLOCK_PI));
            EXPECT_EQ(-1, result);
            EXPECT_EQ(EINVAL, error);
            EXPECT_EQ(0u, word.load());
        }
    }
}

TEST_P(PiFutex, ExpiredDeadlineStillAcquiresFreeLock) {
    const timespec expired = {0, 0};
    for (int operation : {FUTEX_LOCK_PI, FUTEX_LOCK_PI2}) {
        std::atomic<uint32_t> word{0};
        const long result = Futex(&word, Op(operation), &expired);
        EXPECT_EQ(0, result);
        if (result == 0) { EXPECT_EQ(0, Futex(&word, Op(FUTEX_UNLOCK_PI))); }
    }
}

void Handler(int) {}

TEST_P(PiFutex, SignalDoesNotPreventLaterAcquisition) {
    struct sigaction action = {}, previous = {};
    action.sa_handler = Handler;
    sigemptyset(&action.sa_mask);
    ASSERT_EQ(0, sigaction(SIGUSR1, &action, &previous));
    std::atomic<uint32_t> word{0};
    ASSERT_EQ(0, Futex(&word, Op(FUTEX_LOCK_PI)));
    long interrupted = -2, acquired = -2;
    int interrupted_error = 0;
    std::thread first([&] {
        const timespec deadline = After(CLOCK_REALTIME, 5000000000L);
        interrupted = Futex(&word, Op(FUTEX_LOCK_PI), &deadline);
        interrupted_error = errno;
        if (interrupted == 0) Futex(&word, Op(FUTEX_UNLOCK_PI));
    });
    const bool registered = Registered(word);
    if (registered) { EXPECT_EQ(0, pthread_kill(first.native_handle(), SIGUSR1)); }
    // Linux may restart LOCK_PI after the signal handler. Unlock before join
    // and accept that documented outcome rather than requiring EINTR.
    std::thread second([&] {
        const timespec deadline = After(CLOCK_REALTIME, 5000000000L);
        acquired = Futex(&word, Op(FUTEX_LOCK_PI), &deadline);
        if (acquired == 0) Futex(&word, Op(FUTEX_UNLOCK_PI));
    });
    EXPECT_EQ(0, Futex(&word, Op(FUTEX_UNLOCK_PI)));
    first.join();
    second.join();
    EXPECT_TRUE(registered);
    EXPECT_TRUE(interrupted == 0 || (interrupted == -1 && interrupted_error == EINTR));
    EXPECT_EQ(0, acquired);
    EXPECT_EQ(0u, word.load());
    EXPECT_EQ(0, sigaction(SIGUSR1, &previous, nullptr));
}

TEST_P(PiFutex, ContentionRetainsAllOwners) {
    std::atomic<uint32_t> word{0};
    std::atomic<int> errors{0};
    int protected_count = 0;
    std::vector<std::thread> workers;
    for (int worker = 0; worker < 8; ++worker) {
        workers.emplace_back([&] {
            for (int iteration = 0; iteration < 64; ++iteration) {
                const timespec deadline = After(CLOCK_REALTIME, 5000000000L);
                if (Futex(&word, Op(FUTEX_LOCK_PI), &deadline) != 0) {
                    ++errors;
                    return;
                }
                if ((word.load() & FUTEX_TID_MASK) != Tid()) ++errors;
                ++protected_count;
                if (Futex(&word, Op(FUTEX_UNLOCK_PI)) != 0) ++errors;
            }
        });
    }
    for (auto& worker : workers) worker.join();
    EXPECT_EQ(0, errors.load());
    EXPECT_EQ(8 * 64, protected_count);
    EXPECT_EQ(0u, word.load());
}

TEST_P(PiFutex, InaccessibleAndReadonlyWordsReturnEfault) {
    const size_t page = static_cast<size_t>(sysconf(_SC_PAGESIZE));
    auto* word = static_cast<std::atomic<uint32_t>*>(
        mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, word);
    for (int operation : {FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI}) {
        errno = 0;
        EXPECT_EQ(-1, Futex(word, Op(operation)));
        EXPECT_EQ(EFAULT, errno);
    }
    ASSERT_EQ(0, mprotect(word, page, PROT_READ));
    for (int operation : {FUTEX_LOCK_PI, FUTEX_TRYLOCK_PI}) {
        errno = 0;
        EXPECT_EQ(-1, Futex(word, Op(operation)));
        EXPECT_EQ(EFAULT, errno);
    }
    ASSERT_EQ(0, mprotect(word, page, PROT_READ | PROT_WRITE));
    ASSERT_EQ(0, Futex(word, Op(FUTEX_LOCK_PI)));
    ASSERT_EQ(0, mprotect(word, page, PROT_READ));
    long busy = 0;
    int busy_error = 0;
    std::thread worker([&] {
        busy = Futex(word, Op(FUTEX_TRYLOCK_PI));
        busy_error = errno;
    });
    worker.join();
    EXPECT_EQ(-1, busy);
    EXPECT_EQ(EFAULT, busy_error);
    EXPECT_EQ(0, mprotect(word, page, PROT_READ | PROT_WRITE));
    EXPECT_EQ(0, Futex(word, Op(FUTEX_UNLOCK_PI)));
    EXPECT_EQ(0, munmap(word, page));
}

class PiFutexAlias : public ::testing::Test {
protected:
    void SetUp() override {
        alarm(15);
        page_ = static_cast<size_t>(sysconf(_SC_PAGESIZE));
        char name[] = "/tmp/dunitest_pi_alias_XXXXXX";
        fd_ = mkstemp(name);
        ASSERT_GE(fd_, 0);
        EXPECT_EQ(0, unlink(name));
        ASSERT_EQ(0, ftruncate(fd_, page_));
        first_ = static_cast<std::atomic<uint32_t>*>(mmap(
            nullptr, page_, PROT_READ | PROT_WRITE, MAP_SHARED, fd_, 0));
        ASSERT_NE(MAP_FAILED, first_);
        second_ = static_cast<std::atomic<uint32_t>*>(mmap(
            nullptr, page_, PROT_READ | PROT_WRITE, MAP_SHARED, fd_, 0));
        ASSERT_NE(MAP_FAILED, second_);
    }
    void TearDown() override {
        if (first_ != MAP_FAILED) munmap(first_, page_);
        if (second_ != MAP_FAILED) munmap(second_, page_);
        if (fd_ >= 0) close(fd_);
        alarm(0);
    }
    bool Registered() {
        const int64_t end = Now() + 2000000000;
        while (Now() < end) {
            if ((first_->load() & FUTEX_WAITERS) != 0) return true;
            usleep(1000);
        }
        return false;
    }
    bool ReplaceWaitingAlias() {
        return mmap(second_, page_, PROT_NONE,
                    MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0) == second_;
    }
    size_t page_ = 0;
    int fd_ = -1;
    std::atomic<uint32_t>* first_ = reinterpret_cast<std::atomic<uint32_t>*>(MAP_FAILED);
    std::atomic<uint32_t>* second_ = reinterpret_cast<std::atomic<uint32_t>*>(MAP_FAILED);
};

TEST_F(PiFutexAlias, HandoffDoesNotReadReplacedWaitingMapping) {
    ASSERT_EQ(0, Futex(first_, FUTEX_LOCK_PI));
    long acquired = -2, released = -2;
    std::thread worker([&] {
        const timespec deadline = After(CLOCK_REALTIME, 5000000000L);
        acquired = Futex(second_, FUTEX_LOCK_PI, &deadline);
        // The original file remains available through first_. Replacing the
        // sleeping address must not lose a successful handoff of that lock.
        if (acquired == 0) released = Futex(first_, FUTEX_UNLOCK_PI);
    });
    const bool registered = Registered();
    const bool replaced = ReplaceWaitingAlias();
    EXPECT_EQ(0, Futex(first_, FUTEX_UNLOCK_PI));
    worker.join();
    EXPECT_TRUE(registered);
    EXPECT_TRUE(replaced);
    EXPECT_EQ(0, acquired);
    EXPECT_EQ(0, released);
    EXPECT_EQ(0u, first_->load());
}

TEST_F(PiFutexAlias, TimeoutCancelsOriginalRegistrationAfterReplacement) {
    ASSERT_EQ(0, Futex(first_, FUTEX_LOCK_PI));
    long waited = -2;
    int error = 0;
    std::thread worker([&] {
        const timespec deadline = After(CLOCK_REALTIME, 200000000);
        waited = Futex(second_, FUTEX_LOCK_PI, &deadline);
        error = errno;
        if (waited == 0) Futex(first_, FUTEX_UNLOCK_PI);
    });
    const bool registered = Registered();
    const bool replaced = ReplaceWaitingAlias();
    // The worker has an absolute deadline and never needs the replacement
    // mapping to unregister. The owner remains held throughout cancellation.
    worker.join();
    EXPECT_TRUE(registered);
    EXPECT_TRUE(replaced);
    EXPECT_EQ(-1, waited);
    EXPECT_EQ(ETIMEDOUT, error);
    EXPECT_EQ(Tid(), first_->load() & FUTEX_TID_MASK);
    EXPECT_EQ(0, Futex(first_, FUTEX_UNLOCK_PI));
    EXPECT_EQ(0u, first_->load());
    ASSERT_EQ(0, Futex(first_, FUTEX_TRYLOCK_PI));
    EXPECT_EQ(0, Futex(first_, FUTEX_UNLOCK_PI));
}

INSTANTIATE_TEST_SUITE_P(PrivateAndShared, PiFutex,
                        ::testing::Values(FUTEX_PRIVATE_FLAG, 0));

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
