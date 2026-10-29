#include <gtest/gtest.h>

#include <errno.h>
#include <poll.h>
#include <signal.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <cstdlib>
#include <cstring>
#include <thread>

namespace {

uint64_t Now(clockid_t clock) {
  timespec value{};
  if (clock_gettime(clock, &value) != 0) return 0;
  return uint64_t(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}

void BusyFor(uint64_t nanoseconds) {
  const uint64_t end = Now(CLOCK_MONOTONIC) + nanoseconds;
  volatile unsigned long work = 1;
  do {
    for (unsigned i = 0; i < 100; ++i) work = work * 33 + i;
  } while (Now(CLOCK_MONOTONIC) < end);
}

// Neither a broken CPU clock nor a failed exec may hang the test runner.
bool ReapBounded(pid_t child, int* status) {
  uint64_t end = Now(CLOCK_MONOTONIC) + 5000000000ULL;
  do {
    pid_t result = waitpid(child, status, WNOHANG);
    if (result == child) return true;
    if (result < 0 && errno != EINTR) return false;
    usleep(1000);
  } while (Now(CLOCK_MONOTONIC) < end);
  kill(child, SIGKILL);
  end = Now(CLOCK_MONOTONIC) + 2000000000ULL;
  do {
    pid_t result = waitpid(child, status, WNOHANG);
    if (result == child || (result < 0 && errno == ECHILD)) return false;
    usleep(1000);
  } while (Now(CLOCK_MONOTONIC) < end);
  return false;
}

TEST(CpuClockRuntime, CurrentThreadAndProcessIncludeSubTickExecution) {
  const uint64_t thread_start = Now(CLOCK_THREAD_CPUTIME_ID);
  const uint64_t process_start = Now(CLOCK_PROCESS_CPUTIME_ID);
  for (unsigned i = 0; i < 100; ++i) BusyFor(200000);
  const uint64_t thread_delta = Now(CLOCK_THREAD_CPUTIME_ID) - thread_start;
  const uint64_t process_delta = Now(CLOCK_PROCESS_CPUTIME_ID) - process_start;
  EXPECT_GT(thread_delta, 12000000U);
  EXPECT_GT(process_delta, 12000000U);
  // A tick crossing can hide one stale read. Most sub-tick intervals must
  // expose completed execution, not only the intervals containing a tick.
  unsigned visible_intervals = 0;
  for (unsigned i = 0; i < 20; ++i) {
    const uint64_t before = Now(CLOCK_THREAD_CPUTIME_ID);
    BusyFor(500000);
    if (Now(CLOCK_THREAD_CPUTIME_ID) - before > 100000) ++visible_intervals;
  }
  EXPECT_GE(visible_intervals, 15U);
}

TEST(CpuClockRuntime, SleepingDoesNotChargeWallTime) {
  const uint64_t thread_start = Now(CLOCK_THREAD_CPUTIME_ID);
  const uint64_t process_start = Now(CLOCK_PROCESS_CPUTIME_ID);
  usleep(80000);
  EXPECT_LT(Now(CLOCK_THREAD_CPUTIME_ID) - thread_start, 20000000U);
  EXPECT_LT(Now(CLOCK_PROCESS_CPUTIME_ID) - process_start, 20000000U);
}

TEST(CpuClockRuntime, CurrentProcessSchedAliasesMatchStaticRuntime) {
  clockid_t explicit_process;
  ASSERT_EQ(clock_getcpuclockid(getpid(), &explicit_process), 0);
  // glibc uses MAKE_PROCESS_CPUCLOCK(0, CPUCLOCK_SCHED) for nanosleep.
  for (clockid_t alias : {clockid_t(-6), explicit_process}) {
    const uint64_t before = Now(CLOCK_PROCESS_CPUTIME_ID);
    const uint64_t encoded = Now(alias);
    const uint64_t after = Now(CLOCK_PROCESS_CPUTIME_ID);
    EXPECT_GE(encoded, before);
    EXPECT_LE(encoded, after);
  }
}

TEST(CpuClockRuntime, ExitedThreadRuntimeRemainsInProcessHistory) {
  const uint64_t process_start = Now(CLOCK_PROCESS_CPUTIME_ID);
  std::atomic<uint64_t> completed{0};
  std::thread worker([&]() {
    const uint64_t start = Now(CLOCK_THREAD_CPUTIME_ID);
    BusyFor(60000000);
    completed.store(Now(CLOCK_THREAD_CPUTIME_ID) - start);
  });
  worker.join();
  const uint64_t after_join = Now(CLOCK_PROCESS_CPUTIME_ID);
  ASSERT_GT(completed.load(), 10000000U);
  EXPECT_GE(after_join - process_start, completed.load() * 9 / 10);
  usleep(20000);
  EXPECT_GE(Now(CLOCK_PROCESS_CPUTIME_ID), after_join);
}

void InterruptCpuSleep(int) {}

TEST(CpuClockRuntime, SingleThreadCpuSleepDoesNotGenerateItsOwnProgress) {
  int result[2];
  ASSERT_EQ(0, pipe(result));
  const pid_t child = fork();
  if (child < 0) {
    close(result[0]); close(result[1]);
    FAIL() << strerror(errno);
  }
  if (child == 0) {
    close(result[0]);
    struct sigaction action{};
    action.sa_handler = InterruptCpuSleep;
    sigemptyset(&action.sa_mask);
    // No SA_RESTART: the parent signal must interrupt CPU-time waiting.
    if (sigaction(SIGUSR1, &action, nullptr) != 0) _exit(120);
    if (write(result[1], "R", 1) != 1) _exit(121);
    const timespec request{0, 20000000};
    const int error = clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, 0, &request, nullptr);
    const ssize_t count = write(result[1], &error, sizeof(error));
    _exit(count == sizeof(error) ? 0 : 122);
  }
  close(result[1]);
  pollfd descriptor{result[0], POLLIN, 0};
  int ready = poll(&descriptor, 1, 1000);
  char token = 0;
  ssize_t count = ready > 0 ? read(result[0], &token, 1) : -1;
  bool completed_early = false;
  int error = -1;
  if (count == 1 && token == 'R') {
    // The only runnable thread in the child is about to sleep. Its CPU clock
    // must stop; runtime notifications must not repeatedly wake themselves.
    ready = poll(&descriptor, 1, 200);
    completed_early = ready > 0 && (descriptor.revents & POLLIN);
    if (!completed_early) kill(child, SIGUSR1);
    if (!completed_early) ready = poll(&descriptor, 1, 2000);
    count = ready > 0 ? read(result[0], &error, sizeof(error)) : -1;
  }
  close(result[0]);
  if (count != sizeof(error)) kill(child, SIGKILL);
  int status = 0;
  ASSERT_TRUE(ReapBounded(child, &status));
  ASSERT_EQ(static_cast<ssize_t>(sizeof(error)), count);
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  EXPECT_FALSE(completed_early);
  EXPECT_EQ(EINTR, error);
}

TEST(CpuClockRuntime, LastWorkerExitDoesNotLoseProcessCpuDeadlineWakeup) {
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    const clockid_t alias = -6;
    const uint64_t target = Now(alias) + 20000000ULL;
    std::atomic<bool> go{false};
    std::thread worker([&]() {
      while (!go.load()) std::this_thread::yield();
      const uint64_t safety = Now(CLOCK_MONOTONIC) + 2000000000ULL;
      while (Now(CLOCK_PROCESS_CPUTIME_ID) < target &&
             Now(CLOCK_MONOTONIC) < safety) {
        BusyFor(200000);
      }
      // No work follows the deadline: settling this thread's exit must not
      // discard the runtime progress or the sleeping peer's notification.
    });
    const timespec request{static_cast<time_t>(target / 1000000000ULL),
                           static_cast<long>(target % 1000000000ULL)};
    go.store(true);
    const int error = clock_nanosleep(alias, TIMER_ABSTIME,
                                      &request, nullptr);
    const uint64_t observed = Now(CLOCK_PROCESS_CPUTIME_ID);
    worker.join();
    _exit(error == 0 && observed >= target &&
          Now(CLOCK_PROCESS_CPUTIME_ID) >= observed ? 0 : 120);
  }
  int status = 0;
  ASSERT_TRUE(ReapBounded(child, &status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(CpuClockRuntime, ConcurrentProcessCpuSleepersObserveTheirOwnDeadlines) {
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    const uint64_t begin = Now(CLOCK_PROCESS_CPUTIME_ID);
    std::atomic<unsigned> ready{0};
    std::atomic<bool> stop{false};
    int errors[3] = {-1, -1, -1};
    uint64_t observed[3] = {0, 0, 0};
    std::thread sleepers[3];
    for (unsigned i = 0; i < 3; ++i) {
      sleepers[i] = std::thread([&, i]() {
        const uint64_t target = begin + (10 + 20 * i) * 1000000ULL;
        const timespec request{static_cast<time_t>(target / 1000000000ULL),
                               static_cast<long>(target % 1000000000ULL)};
        ready.fetch_add(1);
        errors[i] = clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, TIMER_ABSTIME,
                                    &request, nullptr);
        observed[i] = Now(CLOCK_PROCESS_CPUTIME_ID);
      });
    }
    const uint64_t safety = Now(CLOCK_MONOTONIC) + 2000000000ULL;
    std::thread worker([&]() {
      while (ready.load() < 3 && Now(CLOCK_MONOTONIC) < safety) {
        std::this_thread::yield();
      }
      while (!stop.load() && Now(CLOCK_MONOTONIC) < safety) BusyFor(200000);
    });
    for (auto& sleeper : sleepers) sleeper.join();
    stop.store(true);
    worker.join();
    for (unsigned i = 0; i < 3; ++i) {
      // Notification of an earlier waiter is not completion of a later one.
      // Return order itself is not guaranteed: scheduling can delay a waiter.
      if (errors[i] != 0 || observed[i] < begin + (10 + 20 * i) * 1000000ULL) {
        _exit(120 + i);
      }
    }
    _exit(0);
  }
  int status = 0;
  ASSERT_TRUE(ReapBounded(child, &status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(CpuClockRuntime, NonLeaderExecPreservesProcessRuntime) {
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    // Charge the old leader, then replace the image from another thread.
    BusyFor(50000000);
    const uint64_t minimum = Now(CLOCK_PROCESS_CPUTIME_ID);
    std::thread executor([minimum]() {
      char amount[32];
      snprintf(amount, sizeof(amount), "%llu",
               static_cast<unsigned long long>(minimum));
      char* const arguments[] = {const_cast<char*>("/proc/self/exe"),
                                const_cast<char*>("--cpu-clock-exec"), amount, nullptr};
      execv("/proc/self/exe", arguments);
      _exit(120);
    });
    executor.join();
    _exit(121);
  }
  int status = 0;
  ASSERT_TRUE(ReapBounded(child, &status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

}  // namespace

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "--cpu-clock-exec") == 0) {
    const uint64_t minimum = strtoull(argv[2], nullptr, 10);
    return minimum > 10000000 && Now(CLOCK_PROCESS_CPUTIME_ID) >= minimum ? 0 : 122;
  }
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
