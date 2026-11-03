#include <gtest/gtest.h>

#include <atomic>
#include <cerrno>
#include <climits>
#include <csignal>
#include <cstdint>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <linux/futex.h>
#include <pthread.h>
#include <sched.h>
#include <sstream>
#include <string>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <thread>
#include <time.h>
#include <unistd.h>
#include <vector>

namespace {
using Word = std::atomic<uint32_t>;

long Futex(Word* word, int operation, int value = 0,
           const void* fourth = nullptr, Word* target = nullptr,
           uint32_t comparison = 0) {
    return syscall(SYS_futex, word, operation, value, fourth, target, comparison);
}

timespec Deadline(long milliseconds = 3000, clockid_t clock = CLOCK_MONOTONIC) {
    timespec result{};
    clock_gettime(clock, &result);
    result.tv_sec += milliseconds / 1000;
    result.tv_nsec += (milliseconds % 1000) * 1000000;
    result.tv_sec += result.tv_nsec / 1000000000;
    result.tv_nsec %= 1000000000;
    return result;
}

int64_t Now() {
    timespec value{};
    clock_gettime(CLOCK_MONOTONIC, &value);
    return int64_t(value.tv_sec) * 1000000000 + value.tv_nsec;
}

// Every waiter has a kernel deadline. In particular, ordinary FUTEX_WAKE is
// not a cleanup operation for WAIT_REQUEUE_PI: Linux rejects that mismatch.
class Waiter {
public:
    Waiter(Word& source, Word& target, int flags, long milliseconds = 3000,
           bool ordinary = false, std::atomic<int>* sequence = nullptr,
           int* position = nullptr, int rt_priority = 0)
        : thread_([&, flags, milliseconds, ordinary, sequence, position, rt_priority] {
              if (rt_priority) {
                  sched_param priority{};
                  priority.sched_priority = rt_priority;
                  if (sched_setscheduler(0, SCHED_FIFO, &priority) != 0) {
                      result = -3;
                      error = errno;
                      return;
                  }
              }
              tid.store(static_cast<uint32_t>(syscall(SYS_gettid)));
              const timespec timeout = ordinary ? timespec{milliseconds / 1000,
                                                   milliseconds % 1000 * 1000000}
                                               : Deadline(milliseconds,
                                                     flags & FUTEX_CLOCK_REALTIME
                                                         ? CLOCK_REALTIME
                                                         : CLOCK_MONOTONIC);
              result = Futex(&source,
                             (ordinary ? FUTEX_WAIT : FUTEX_WAIT_REQUEUE_PI) | flags,
                             0, &timeout, &target);
              error = errno;
              if (result == 0 && sequence && position)
                  *position = sequence->fetch_add(1);
              if (!ordinary && result == 0) {
                  owner = target.load() & FUTEX_TID_MASK;
                  owner_died = (target.load() & FUTEX_OWNER_DIED) != 0;
                  unlock = Futex(&target, FUTEX_UNLOCK_PI |
                                             (flags & FUTEX_PRIVATE_FLAG));
              }
              finished.store(true);
          }) {}
    ~Waiter() { Join(); }
    void Join() { if (thread_.joinable()) thread_.join(); }
    pthread_t Handle() { return thread_.native_handle(); }
    bool Sleeping() {
        const int64_t end = Now() + 1000000000;
        while (Now() < end) {
            const uint32_t id = tid.load();
            if (id) {
                const std::string path = "/proc/" + std::to_string(id) + "/stat";
                FILE* file = fopen(path.c_str(), "r");
                if (file) {
                    char line[2048]{};
                    const bool read = fgets(line, sizeof(line), file) != nullptr;
                    fclose(file);
                    char* end_name = read ? strrchr(line, ')') : nullptr;
                    if (end_name && end_name[1] == ' ' && end_name[2] == 'S')
                        return true;
                }
            }
            usleep(1000);
        }
        return false;
    }
    std::atomic<uint32_t> tid{0};
    std::atomic<bool> finished{false};
    long result = -2, unlock = -2;
    int error = 0;
    uint32_t owner = 0;
    bool owner_died = false;
private:
    std::thread thread_;
};

class RequeuePi : public ::testing::TestWithParam<int> {
protected:
    int Flags() const { return GetParam(); }
    long Requeue(Word& source, Word& target, int count = 0, uint32_t compare = 0,
                 int wake = 1) {
        return Futex(&source, FUTEX_CMP_REQUEUE_PI | Flags(), wake,
                     reinterpret_cast<void*>(static_cast<intptr_t>(count)),
                     &target, compare);
    }
    long AwaitTransfer(Word& source, Word& target, int count = 0) {
        const int64_t end = Now() + 1000000000;
        long result;
        do {
            result = Requeue(source, target, count);
            if (result != 0) return result;
            usleep(1000);
        } while (Now() < end);
        return 0;
    }
    void Acquired(Waiter& task) {
        task.Join();
        EXPECT_EQ(0, task.result) << task.error;
        EXPECT_EQ(task.tid.load(), task.owner);
        EXPECT_EQ(0, task.unlock);
    }
};

TEST_P(RequeuePi, EmptySourceAndComparison) {
    Word source{0}, target{0};
    EXPECT_EQ(0, Requeue(source, target));
    errno = 0;
    EXPECT_EQ(-1, Requeue(source, target, 0, 1));
    EXPECT_EQ(EAGAIN, errno);
    errno = 0;
    EXPECT_EQ(-1, Futex(&source, FUTEX_WAIT_REQUEUE_PI | Flags(), 1,
                        nullptr, &target));
    EXPECT_EQ(EAGAIN, errno);
    EXPECT_EQ(0u, target.load());
}

TEST_P(RequeuePi, CountsSameAddressAndClockValidation) {
    Word source{0}, target{0};
    for (int wake : {-1, 0, 2}) {
        errno = 0;
        EXPECT_EQ(-1, Requeue(source, target, 0, 0, wake));
        EXPECT_EQ(EINVAL, errno);
    }
    errno = 0;
    EXPECT_EQ(-1, Requeue(source, target, -1));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Requeue(source, source));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Futex(&source, FUTEX_WAIT_REQUEUE_PI | Flags(), 0,
                        nullptr, &source));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Futex(&source, FUTEX_CMP_REQUEUE_PI | Flags() |
                                    FUTEX_CLOCK_REALTIME,
                        1, nullptr, &target));
    EXPECT_EQ(ENOSYS, errno);
}

TEST_P(RequeuePi, InvalidTimespecPrecedesAddressValidation) {
    Word source{0};
    for (timespec invalid : {timespec{-1, 0}, timespec{0, 1000000000}}) {
        errno = 0;
        EXPECT_EQ(-1, Futex(&source, FUTEX_WAIT_REQUEUE_PI | Flags(), 0,
                            &invalid, &source));
        EXPECT_EQ(EINVAL, errno);
    }
}

TEST_P(RequeuePi, FreeTargetMoveZeroAcquiresOnBehalfOfWaiter) {
    Word source{0}, target{0};
    Waiter task(source, target, Flags());
    EXPECT_EQ(1, AwaitTransfer(source, target));
    Acquired(task);
    EXPECT_EQ(0u, target.load());
}

TEST_P(RequeuePi, BusyTargetMoveZeroAndOrdinaryPiWaiter) {
    Word source{0}, target{0};
    ASSERT_EQ(0, Futex(&target, FUTEX_LOCK_PI | Flags()));
    Waiter task(source, target, Flags());
    EXPECT_EQ(1, AwaitTransfer(source, target));
    long acquired = -2, released = -2;
    std::thread direct([&] {
        const timespec deadline = Deadline(3000, CLOCK_REALTIME);
        acquired = Futex(&target, FUTEX_LOCK_PI | Flags(), 0, &deadline);
        if (acquired == 0) released = Futex(&target, FUTEX_UNLOCK_PI | Flags());
    });
    EXPECT_NE(0u, target.load() & FUTEX_WAITERS);
    EXPECT_EQ(0, Futex(&target, FUTEX_UNLOCK_PI | Flags()));
    task.Join();
    direct.join();
    Acquired(task);
    EXPECT_EQ(0, acquired);
    EXPECT_EQ(0, released);
    EXPECT_EQ(0u, target.load());
}

TEST_P(RequeuePi, PiSourceWithNoOrdinaryChainIsNotAnEmptySource) {
    Word source{0}, target{0};
    ASSERT_EQ(0, Futex(&source, FUTEX_LOCK_PI | Flags()));
    long acquired = -2, released = -2;
    int error = 0;
    std::thread direct([&] {
        const timespec deadline = Deadline(3000, CLOCK_REALTIME);
        acquired = Futex(&source, FUTEX_LOCK_PI | Flags(), 0, &deadline);
        error = errno;
        if (acquired == 0) released = Futex(&source, FUTEX_UNLOCK_PI | Flags());
    });
    const int64_t end = Now() + 1000000000;
    while (!(source.load() & FUTEX_WAITERS) && Now() < end) usleep(1000);
    EXPECT_NE(0u, source.load() & FUTEX_WAITERS);
    errno = 0;
    EXPECT_EQ(-1, Requeue(source, target, 0, source.load()));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0u, target.load());
    EXPECT_EQ(0, Futex(&source, FUTEX_UNLOCK_PI | Flags()));
    direct.join();
    EXPECT_EQ(0, acquired) << error;
    EXPECT_EQ(0, released);
    EXPECT_EQ(0u, source.load());
}

TEST_P(RequeuePi, BatchCountAndUnselectedWaiterRemainOnSource) {
    Word source{0}, target{0};
    std::atomic<int> sequence{0};
    int positions[3] = {-1, -1, -1};
    ASSERT_EQ(0, Futex(&target, FUTEX_LOCK_PI | Flags()));
    Waiter first(source, target, Flags(), 3000, false, &sequence, &positions[0]);
    EXPECT_TRUE(first.Sleeping());
    Waiter second(source, target, Flags(), 3000, false, &sequence, &positions[1]);
    EXPECT_TRUE(second.Sleeping());
    Waiter third(source, target, Flags(), 3000, false, &sequence, &positions[2]);
    EXPECT_TRUE(third.Sleeping());
    EXPECT_EQ(2, Requeue(source, target, 1));
    EXPECT_EQ(1, Requeue(source, target, 0));
    EXPECT_EQ(0, Requeue(source, target, INT_MAX));
    EXPECT_EQ(0, Futex(&target, FUTEX_UNLOCK_PI | Flags()));
    Acquired(first);
    Acquired(second);
    Acquired(third);
    for (int i = 0; i < 3; ++i) EXPECT_EQ(i, positions[i]);
    EXPECT_EQ(0u, target.load());
}

TEST_P(RequeuePi, SourceTimeoutAndIgnoredVal3) {
    Word source{0}, target{0};
    for (int clock_flag : {0, FUTEX_CLOCK_REALTIME}) {
        const timespec end = Deadline(40, clock_flag ? CLOCK_REALTIME : CLOCK_MONOTONIC);
        errno = 0;
        EXPECT_EQ(-1, Futex(&source, FUTEX_WAIT_REQUEUE_PI | Flags() | clock_flag,
                            0, &end, &target, 0));
        EXPECT_EQ(ETIMEDOUT, errno);
        EXPECT_EQ(0, Requeue(source, target));
        EXPECT_EQ(0u, target.load());
    }
}

TEST_P(RequeuePi, TargetTimeoutWithdrawsRegistration) {
    Word source{0}, target{0};
    ASSERT_EQ(0, Futex(&target, FUTEX_LOCK_PI | Flags()));
    Waiter task(source, target, Flags(), 400);
    EXPECT_EQ(1, AwaitTransfer(source, target));
    task.Join();
    EXPECT_EQ(-1, task.result);
    EXPECT_EQ(ETIMEDOUT, task.error);
    EXPECT_EQ(static_cast<uint32_t>(syscall(SYS_gettid)),
              target.load() & FUTEX_TID_MASK);
    EXPECT_EQ(0, Futex(&target, FUTEX_UNLOCK_PI | Flags()));
    EXPECT_EQ(0, Requeue(source, target));
    Waiter next(source, target, Flags());
    EXPECT_EQ(1, AwaitTransfer(source, target));
    Acquired(next);
}

std::atomic<int> signals{0};
static_assert(std::atomic<int>::is_always_lock_free);
void SignalHandler(int) { signals.fetch_add(1); }
class SignalScope {
public:
    SignalScope() {
        struct sigaction action{};
        action.sa_handler = SignalHandler;
        sigemptyset(&action.sa_mask);
        valid = sigaction(SIGUSR1, &action, &previous_) == 0;
        signals = 0;
    }
    ~SignalScope() { if (valid) sigaction(SIGUSR1, &previous_, nullptr); }
    bool valid;
private:
    struct sigaction previous_{};
};

TEST_P(RequeuePi, SignalBeforeTransferRestartsWithoutLosingWait) {
    SignalScope handler;
    ASSERT_TRUE(handler.valid);
    Word source{0}, target{0};
    Waiter task(source, target, Flags());
    EXPECT_TRUE(task.Sleeping());
    EXPECT_EQ(0, pthread_kill(task.Handle(), SIGUSR1));
    const int64_t end = Now() + 1000000000;
    while (!signals && Now() < end) usleep(1000);
    EXPECT_NE(0, signals);
    EXPECT_EQ(1, AwaitTransfer(source, target));
    Acquired(task);
}

TEST_P(RequeuePi, SignalAfterTransferReturnsWouldBlock) {
    SignalScope handler;
    ASSERT_TRUE(handler.valid);
    Word source{0}, target{0};
    ASSERT_EQ(0, Futex(&target, FUTEX_LOCK_PI | Flags()));
    Waiter task(source, target, Flags());
    EXPECT_EQ(1, AwaitTransfer(source, target));
    EXPECT_EQ(0, pthread_kill(task.Handle(), SIGUSR1));
    task.Join();
    EXPECT_EQ(-1, task.result);
    EXPECT_EQ(EAGAIN, task.error);
    EXPECT_EQ(0, Futex(&target, FUTEX_UNLOCK_PI | Flags()));
    EXPECT_EQ(0, Requeue(source, target));
}

TEST_P(RequeuePi, MismatchedOperationsAndDestinationAreRejected) {
    Word source{0}, target{0}, other{0};
    Waiter task(source, target, Flags(), 200);
    EXPECT_TRUE(task.Sleeping());
    errno = 0;
    EXPECT_EQ(-1, Futex(&source, FUTEX_WAKE | Flags(), 1));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Requeue(source, other));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Futex(&source, FUTEX_REQUEUE | Flags(), 0,
                        reinterpret_cast<void*>(1), &other));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(1, Requeue(source, target));
    Acquired(task);
    Waiter ordinary(source, target, Flags(), 200, true);
    EXPECT_TRUE(ordinary.Sleeping());
    errno = 0;
    EXPECT_EQ(-1, Requeue(source, target));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(1, Futex(&source, FUTEX_WAKE | Flags(), 1));
    ordinary.Join();
    EXPECT_EQ(0, ordinary.result);
}

TEST_P(RequeuePi, ErrorAfterCommittedPrefixPreservesOwnership) {
    for (bool held : {false, true}) {
        SCOPED_TRACE(held);
        Word source{0}, target{0};
        if (held) { ASSERT_EQ(0, Futex(&target, FUTEX_LOCK_PI | Flags())); }
        Waiter first(source, target, Flags());
        EXPECT_TRUE(first.Sleeping());
        Waiter ordinary(source, target, Flags(), 1000, true);
        EXPECT_TRUE(ordinary.Sleeping());
        errno = 0;
        EXPECT_EQ(-1, Requeue(source, target, INT_MAX));
        EXPECT_EQ(EINVAL, errno);
        if (held) { EXPECT_EQ(0, Futex(&target, FUTEX_UNLOCK_PI | Flags())); }
        Acquired(first);
        EXPECT_EQ(1, Futex(&source, FUTEX_WAKE | Flags(), 1));
        ordinary.Join();
        EXPECT_EQ(0, ordinary.result);
        EXPECT_EQ(0u, target.load());
    }
}

TEST_P(RequeuePi, OrdinaryRequeueCommitsPrefixBeforeRejectingPiWaiter) {
    Word source{0}, ordinary_target{0}, pi_target{0};
    Waiter ordinary(source, ordinary_target, Flags(), 3000, true);
    EXPECT_TRUE(ordinary.Sleeping());
    Waiter proxy(source, pi_target, Flags());
    EXPECT_TRUE(proxy.Sleeping());
    errno = 0;
    EXPECT_EQ(-1, Futex(&source, FUTEX_REQUEUE | Flags(), 0,
                        reinterpret_cast<void*>(2), &ordinary_target));
    EXPECT_EQ(EINVAL, errno);
    // The ordinary prefix is already committed despite the error. The PI
    // registration remains on the source and requires its declared PI target.
    EXPECT_EQ(1, Futex(&ordinary_target, FUTEX_WAKE | Flags(), 1));
    ordinary.Join();
    EXPECT_EQ(0, ordinary.result) << ordinary.error;
    EXPECT_EQ(1, Requeue(source, pi_target));
    Acquired(proxy);
    EXPECT_EQ(0u, pi_target.load());
}

TEST_P(RequeuePi, InaccessibleTargetIsCheckedEvenWithEmptySource) {
    Word source{0};
    const size_t page = sysconf(_SC_PAGESIZE);
    void* mapping = mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, mapping);
    auto* target = static_cast<Word*>(mapping);
    errno = 0;
    EXPECT_EQ(-1, Futex(&source, FUTEX_CMP_REQUEUE_PI | Flags(), 1,
                        nullptr, target));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, Futex(&source, FUTEX_WAIT_REQUEUE_PI | Flags(), 1,
                        nullptr, target));
    EXPECT_EQ(Flags() & FUTEX_PRIVATE_FLAG ? EAGAIN : EFAULT, errno);
    if (Flags() & FUTEX_PRIVATE_FLAG) {
        errno = 0;
        EXPECT_EQ(-1, Futex(&source, FUTEX_CMP_REQUEUE_PI | Flags(), 1,
                            nullptr, target, 1));
        EXPECT_EQ(EAGAIN, errno);
    }
    EXPECT_EQ(0, munmap(mapping, page));
}

TEST_P(RequeuePi, ReadonlySourceCanWaitAndRequeue) {
    const size_t page = sysconf(_SC_PAGESIZE);
    char path[] = "/tmp/dunitest-requeue-read-XXXXXX";
    int fd = mkstemp(path);
    ASSERT_GE(fd, 0);
    unlink(path);
    ASSERT_EQ(0, ftruncate(fd, page));
    auto* source = static_cast<Word*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                          MAP_SHARED, fd, 0));
    close(fd);
    ASSERT_NE(MAP_FAILED, source);
    source->store(0);
    ASSERT_EQ(0, mprotect(source, page, PROT_READ));
    Word target{0};
    Waiter task(*source, target, Flags());
    EXPECT_EQ(1, AwaitTransfer(*source, target));
    Acquired(task);
    EXPECT_EQ(0, munmap(source, page));
}

TEST_P(RequeuePi, TargetWriteFaultDoesNotConsumeSourceWaiter) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* target = static_cast<Word*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, target);
    target->store(0);
    Word source{0};
    Waiter task(source, *target, Flags());
    EXPECT_TRUE(task.Sleeping());
    EXPECT_EQ(0, mprotect(target, page, PROT_READ));
    errno = 0;
    EXPECT_EQ(-1, Requeue(source, *target));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, mprotect(target, page, PROT_READ | PROT_WRITE));
    EXPECT_EQ(1, Requeue(source, *target));
    Acquired(task);
    EXPECT_EQ(0, munmap(target, page));
}

TEST_P(RequeuePi, OwnerExitAfterProxyRegistrationHandsOffWithOwnerDied) {
    Word source{0}, target{0};
    std::atomic<bool> locked{false}, exit_owner{false};
    long owner_result = -2;
    std::thread owner([&] {
        owner_result = Futex(&target, FUTEX_LOCK_PI | Flags());
        locked.store(true);
        while (!exit_owner.load()) std::this_thread::yield();
        // Once a proxy waiter attaches, kernel PI owner-exit cleanup owns the
        // handoff even for a raw, non-robust userspace lock.
    });
    const int64_t end = Now() + 1000000000;
    while (!locked.load() && Now() < end) usleep(1000);
    EXPECT_TRUE(locked.load());
    Waiter task(source, target, Flags());
    EXPECT_EQ(1, AwaitTransfer(source, target));
    exit_owner.store(true);
    owner.join();
    EXPECT_EQ(0, owner_result);
    Acquired(task);
    EXPECT_TRUE(task.owner_died);
    EXPECT_EQ(0u, target.load());
}

TEST_P(RequeuePi, SignalsBeforeTransferDoNotRenewAbsoluteTimeout) {
    SignalScope handler;
    ASSERT_TRUE(handler.valid);
    Word source{0}, target{0};
    const int64_t start = Now();
    Waiter task(source, target, Flags(), 400);
    EXPECT_TRUE(task.Sleeping());
    while (!task.finished.load() && Now() - start < 600000000) {
        EXPECT_EQ(0, pthread_kill(task.Handle(), SIGUSR1));
        usleep(20000);
    }
    task.Join();
    EXPECT_EQ(-1, task.result);
    EXPECT_EQ(ETIMEDOUT, task.error);
    EXPECT_GE(Now() - start, 300000000);
    EXPECT_LT(Now() - start, 800000000);
    EXPECT_EQ(0, Requeue(source, target));
}

TEST_P(RequeuePi, ProxySelfOwnerDeadlockDoesNotConsumeWaiter) {
    Word source{0}, target{0};
    std::atomic<bool> locked{false}, start_wait{false};
    long result = -2, unlocked = -2;
    int error = 0;
    std::thread waiter([&] {
        if (Futex(&target, FUTEX_LOCK_PI | Flags()) != 0) {
            locked.store(true);
            return;
        }
        locked.store(true);
        while (!start_wait.load()) std::this_thread::yield();
        const timespec deadline = Deadline(500);
        result = Futex(&source, FUTEX_WAIT_REQUEUE_PI | Flags(), 0,
                       &deadline, &target);
        error = errno;
        unlocked = Futex(&target, FUTEX_UNLOCK_PI | Flags());
    });
    const int64_t end = Now() + 1000000000;
    while (!locked.load() && Now() < end) usleep(1000);
    start_wait.store(true);
    long moved;
    do {
        errno = 0;
        moved = Requeue(source, target);
        if (moved != 0) break;
        usleep(1000);
    } while (Now() < end);
    EXPECT_EQ(-1, moved);
    EXPECT_EQ(EDEADLK, errno);
    waiter.join();
    EXPECT_EQ(-1, result);
    EXPECT_EQ(ETIMEDOUT, error);
    EXPECT_EQ(0, unlocked);
    EXPECT_EQ(0u, target.load());
}

TEST_P(RequeuePi, RepeatedHandoffsDoNotLeaveKernelOwnership) {
    Word source{0}, target{0};
    for (int iteration = 0; iteration < 32; ++iteration) {
        SCOPED_TRACE(iteration);
        Waiter task(source, target, Flags());
        EXPECT_EQ(1, AwaitTransfer(source, target));
        Acquired(task);
        EXPECT_EQ(0u, target.load());
        ASSERT_EQ(0, Futex(&target, FUTEX_TRYLOCK_PI | Flags()));
        EXPECT_EQ(0, Futex(&target, FUTEX_UNLOCK_PI | Flags()));
    }
}

TEST(RequeuePiShared, AliasIdentityAndCrossProcessHandoff) {
    char path[] = "/tmp/dunitest-requeue-pi-XXXXXX";
    int fd = mkstemp(path);
    ASSERT_GE(fd, 0);
    unlink(path);
    const size_t page = sysconf(_SC_PAGESIZE);
    ASSERT_EQ(0, ftruncate(fd, page));
    auto* first = static_cast<Word*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                         MAP_SHARED, fd, 0));
    auto* alias = static_cast<Word*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                         MAP_SHARED, fd, 0));
    close(fd);
    ASSERT_NE(MAP_FAILED, first);
    ASSERT_NE(MAP_FAILED, alias);
    first[0].store(0);
    first[1].store(0);
    errno = 0;
    EXPECT_EQ(-1, Futex(first, FUTEX_WAIT_REQUEUE_PI, 0, nullptr, alias));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Futex(first, FUTEX_CMP_REQUEUE_PI, 1, nullptr, alias));
    EXPECT_EQ(EINVAL, errno);
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        const timespec deadline = Deadline();
        long result = Futex(alias, FUTEX_WAIT_REQUEUE_PI, 0, &deadline, alias + 1);
        bool owned = (alias[1].load() & FUTEX_TID_MASK) == syscall(SYS_gettid);
        long released = result == 0 ? Futex(alias + 1, FUTEX_UNLOCK_PI) : -1;
        _exit(result == 0 && owned && released == 0 ? 0 : 1);
    }
    long moved = 0;
    const int64_t end = Now() + 1000000000;
    while (moved == 0 && Now() < end) {
        moved = Futex(first, FUTEX_CMP_REQUEUE_PI, 1, nullptr, first + 1);
        if (moved == 0) usleep(1000);
    }
    EXPECT_EQ(1, moved);
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status));
    if (WIFEXITED(status)) { EXPECT_EQ(0, WEXITSTATUS(status)); }
    EXPECT_EQ(0u, first[1].load());
    EXPECT_EQ(0, munmap(first, page));
    EXPECT_EQ(0, munmap(alias, page));
}

TEST(RequeuePiShared, SourceReplacementRetainsOriginalKeyForTransferAndTimeout) {
    for (bool timeout : {false, true}) {
        SCOPED_TRACE(timeout);
        char path[] = "/tmp/dunitest-requeue-source-XXXXXX";
        int fd = mkstemp(path);
        ASSERT_GE(fd, 0);
        unlink(path);
        const size_t page = sysconf(_SC_PAGESIZE);
        ASSERT_EQ(0, ftruncate(fd, page));
        auto* source = static_cast<Word*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                              MAP_SHARED, fd, 0));
        auto* alias = static_cast<Word*>(mmap(nullptr, page, PROT_READ | PROT_WRITE,
                                             MAP_SHARED, fd, 0));
        close(fd);
        ASSERT_NE(MAP_FAILED, source);
        ASSERT_NE(MAP_FAILED, alias);
        source->store(0);
        Word target{0};
        Waiter task(*source, target, 0, timeout ? 200 : 3000);
        EXPECT_TRUE(task.Sleeping());
        EXPECT_EQ(source, mmap(source, page, PROT_READ | PROT_WRITE,
                               MAP_PRIVATE | MAP_ANONYMOUS | MAP_FIXED, -1, 0));
        if (!timeout) {
            EXPECT_EQ(1, Futex(alias, FUTEX_CMP_REQUEUE_PI, 1, nullptr, &target));
        }
        task.Join();
        if (timeout) {
            EXPECT_EQ(-1, task.result);
            EXPECT_EQ(ETIMEDOUT, task.error);
            EXPECT_EQ(0, Futex(alias, FUTEX_CMP_REQUEUE_PI, 1, nullptr, &target));
        } else {
            EXPECT_EQ(0, task.result);
            EXPECT_EQ(task.tid.load(), task.owner);
            EXPECT_EQ(0, task.unlock);
        }
        EXPECT_EQ(0, Futex(source, FUTEX_WAKE, 1));
        EXPECT_EQ(0u, source->load());
        EXPECT_EQ(0u, target.load());
        EXPECT_EQ(0, munmap(source, page));
        EXPECT_EQ(0, munmap(alias, page));
    }
}

long EffectivePriority(pid_t tid) {
    const std::string path = "/proc/" + std::to_string(tid) + "/stat";
    FILE* file = fopen(path.c_str(), "r");
    if (!file) return LONG_MAX;
    char line[2048]{};
    const bool read = fgets(line, sizeof(line), file) != nullptr;
    fclose(file);
    const char* name = read ? strrchr(line, ')') : nullptr;
    if (!name) return LONG_MAX;
    std::istringstream fields(name + 1);
    char state;
    long value = LONG_MAX;
    fields >> state;
    for (int field = 4; field <= 18; ++field) fields >> value;
    return fields ? value : LONG_MAX;
}

// Registered only by explicit disposable-guest opt-in. The default Linux
// comparison run neither changes host RT scheduling nor reports fake skips.
class RequeuePiRealtime final : public ::testing::Test {
    void TestBody() override {
        for (int flags : {FUTEX_PRIVATE_FLAG, 0}) {
            SCOPED_TRACE(flags);
            Word source{0}, target{0};
            std::atomic<int> sequence{0};
            int normal_position = -1, rt_position = -1;
            ASSERT_EQ(0, Futex(&target, FUTEX_LOCK_PI | flags));
            Waiter normal(source, target, flags, 3000, false, &sequence,
                          &normal_position);
            EXPECT_TRUE(normal.Sleeping());
            Waiter realtime(source, target, flags, 3000, false, &sequence,
                            &rt_position, 50);
            EXPECT_TRUE(realtime.Sleeping());
            sched_param raised{};
            raised.sched_priority = 70;
            EXPECT_EQ(0, sched_setscheduler(normal.tid.load(), SCHED_FIFO, &raised));
            // Source plist priority is the snapshot at enqueue. Raising the
            // first waiter's live priority must not reorder it ahead of the
            // waiter originally queued with FIFO50.
            EXPECT_EQ(1, Futex(&source, FUTEX_CMP_REQUEUE_PI | flags, 1,
                               nullptr, &target));
            EXPECT_LT(EffectivePriority(syscall(SYS_gettid)), 0);
            EXPECT_EQ(0, Futex(&target, FUTEX_UNLOCK_PI | flags));
            realtime.Join();
            EXPECT_EQ(0, realtime.result) << realtime.error;
            EXPECT_EQ(0, rt_position);
            EXPECT_EQ(0, realtime.unlock);
            EXPECT_GE(EffectivePriority(syscall(SYS_gettid)), 0);
            EXPECT_EQ(1, Futex(&source, FUTEX_CMP_REQUEUE_PI | flags, 1,
                               nullptr, &target));
            normal.Join();
            EXPECT_EQ(0, normal.result) << normal.error;
            EXPECT_EQ(1, normal_position);
            EXPECT_EQ(0, normal.unlock);

            ASSERT_EQ(0, Futex(&target, FUTEX_LOCK_PI | flags));
            Waiter expires(source, target, flags, 400, false, nullptr, nullptr, 50);
            EXPECT_TRUE(expires.Sleeping());
            EXPECT_EQ(1, Futex(&source, FUTEX_CMP_REQUEUE_PI | flags, 1,
                               nullptr, &target));
            EXPECT_LT(EffectivePriority(syscall(SYS_gettid)), 0);
            expires.Join();
            EXPECT_EQ(-1, expires.result);
            EXPECT_EQ(ETIMEDOUT, expires.error);
            EXPECT_GE(EffectivePriority(syscall(SYS_gettid)), 0);
            EXPECT_EQ(0, Futex(&target, FUTEX_UNLOCK_PI | flags));
        }
    }
};

class RequeuePiRealtimeMixedSource final : public ::testing::Test {
    void TestBody() override {
        for (int flags : {FUTEX_PRIVATE_FLAG, 0}) {
            SCOPED_TRACE(flags);
            Word source{0}, target{0};
            Waiter proxy(source, target, flags);
            EXPECT_TRUE(proxy.Sleeping());
            Waiter ordinary(source, target, flags, 3000, true, nullptr, nullptr, 50);
            EXPECT_TRUE(ordinary.Sleeping());
            // Both registrations share the same source priority/FIFO order.
            // A higher priority ordinary waiter must be rejected at the head;
            // requeue cannot skip it to grant the lower priority PI waiter.
            errno = 0;
            EXPECT_EQ(-1, Futex(&source, FUTEX_CMP_REQUEUE_PI | flags, 1,
                                nullptr, &target));
            EXPECT_EQ(EINVAL, errno);
            EXPECT_EQ(0u, target.load());
            EXPECT_EQ(1, Futex(&source, FUTEX_WAKE | flags, 1));
            ordinary.Join();
            EXPECT_EQ(0, ordinary.result) << ordinary.error;
            EXPECT_EQ(1, Futex(&source, FUTEX_CMP_REQUEUE_PI | flags, 1,
                               nullptr, &target));
            proxy.Join();
            EXPECT_EQ(0, proxy.result) << proxy.error;
            EXPECT_EQ(proxy.tid.load(), proxy.owner);
            EXPECT_EQ(0, proxy.unlock);
            EXPECT_EQ(0u, target.load());
        }
    }
};

class RequeuePiRealtimeOrdinaryMerge final : public ::testing::Test {
    void TestBody() override {
        for (int flags : {FUTEX_PRIVATE_FLAG, 0}) {
            SCOPED_TRACE(flags);
            Word source{0}, target{0};
            std::atomic<int> sequence{0};
            int old_position = -1, high_position = -1, moved_position = -1;
            Waiter existing(target, target, flags, 3000, true, &sequence,
                            &old_position, 40);
            EXPECT_TRUE(existing.Sleeping());
            Waiter higher(source, target, flags, 3000, true, &sequence,
                          &high_position, 50);
            EXPECT_TRUE(higher.Sleeping());
            Waiter equal(source, target, flags, 3000, true, &sequence,
                         &moved_position, 40);
            EXPECT_TRUE(equal.Sleeping());
            EXPECT_EQ(2, Futex(&source, FUTEX_REQUEUE | flags, 0,
                               reinterpret_cast<void*>(2), &target));
            EXPECT_EQ(1, Futex(&target, FUTEX_WAKE | flags, 1));
            higher.Join();
            EXPECT_EQ(0, higher.result) << higher.error;
            EXPECT_EQ(0, high_position);
            EXPECT_EQ(1, Futex(&target, FUTEX_WAKE | flags, 1));
            existing.Join();
            EXPECT_EQ(0, existing.result) << existing.error;
            EXPECT_EQ(1, old_position);
            EXPECT_EQ(1, Futex(&target, FUTEX_WAKE | flags, 1));
            equal.Join();
            EXPECT_EQ(0, equal.result) << equal.error;
            EXPECT_EQ(2, moved_position);
            EXPECT_EQ(0, Futex(&target, FUTEX_WAKE | flags, 1));
        }
    }
};

void RegisterRealtimeTest() {
    const char* enabled = getenv("DUNITEST_PI_REQUEUE_RT");
    if (enabled && std::string(enabled) == "1") {
        ::testing::RegisterTest("RequeuePiRealtime", "SourceSnapshotDonationAndWithdrawal",
                                nullptr, nullptr, __FILE__, __LINE__,
                                []() -> ::testing::Test* { return new RequeuePiRealtime; });
        ::testing::RegisterTest("RequeuePiRealtime", "OrdinaryWaitSharesSourcePriorityOrder",
                                nullptr, nullptr, __FILE__, __LINE__,
                                []() -> ::testing::Test* {
                                    return new RequeuePiRealtimeMixedSource;
                                });
        ::testing::RegisterTest("RequeuePiRealtime", "OrdinaryRequeuePreservesPriorityAndFifo",
                                nullptr, nullptr, __FILE__, __LINE__,
                                []() -> ::testing::Test* {
                                    return new RequeuePiRealtimeOrdinaryMerge;
                                });
    }
}

INSTANTIATE_TEST_SUITE_P(PrivateAndShared, RequeuePi,
                        ::testing::Values(FUTEX_PRIVATE_FLAG, 0));
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    RegisterRealtimeTest();
    return RUN_ALL_TESTS();
}
