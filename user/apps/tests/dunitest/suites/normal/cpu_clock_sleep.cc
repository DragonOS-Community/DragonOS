#include <gtest/gtest.h>

#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <cstdint>
#include <thread>

namespace {

constexpr uint64_t kMillisecond = 1000000ULL;

clockid_t CpuClock(pid_t pid, bool thread, unsigned kind) {
  return static_cast<clockid_t>((~static_cast<uint32_t>(pid) << 3) |
                               (thread ? 4U : 0U) | kind);
}

uint64_t Ns(const timespec& value) {
  return uint64_t(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}

timespec Spec(uint64_t ns) {
  return timespec{time_t(ns / 1000000000ULL), long(ns % 1000000000ULL)};
}

uint64_t ReadClock(clockid_t clock) {
  timespec now{};
  if (clock_gettime(clock, &now) != 0) _exit(110);
  return Ns(now);
}

uint64_t WallNow() { return ReadClock(CLOCK_MONOTONIC); }

// Failures terminate the isolated scenario, never leave a joinable thread
// hanging in the test runner. Its parent owns an independent wall watchdog.
#define CHECK_SCENARIO(condition)                                      \
  do {                                                               \
    if (!(condition)) {                                              \
      fprintf(stderr, "CPU sleep scenario line %d: %s (errno=%d)\n",  \
              __LINE__, #condition, errno);                           \
      _exit(111);                                                    \
    }                                                                \
  } while (0)

template <class F>
void BoundedScenario(F scenario) {
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    CHECK_SCENARIO(setpgid(0, 0) == 0);
    scenario();
    _exit(0);
  }
  // Descendants belong to this scenario's group, including a stopped CPU
  // waiter. The watchdog must not leave it orphaned on a failed assertion.
  const int group_result = setpgid(child, child);
  if (group_result != 0 && errno != ESRCH) {
    kill(child, SIGKILL);
    while (waitpid(child, nullptr, 0) < 0 && errno == EINTR) {}
    FAIL() << "failed to isolate scenario process group: " << errno;
  }
  int status = 0;
  const uint64_t watchdog = WallNow() + 8000 * kMillisecond;
  pid_t result = 0;
  do {
    result = waitpid(child, &status, WNOHANG);
    if (result == child || (result < 0 && errno != EINTR)) break;
    usleep(1000);
  } while (WallNow() < watchdog);
  kill(-child, SIGKILL);
  if (result != child) {
    kill(child, SIGKILL);
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    FAIL() << "CPU sleep exceeded its independent wall-clock watchdog";
  }
  ASSERT_TRUE(WIFEXITED(status)) << status;
  EXPECT_EQ(0, WEXITSTATUS(status));
}

void UserWork() {
  volatile uint64_t value = 1;
  for (unsigned i = 0; i < 20000; ++i) value = value * 33 + i;
}

class Worker {
 public:
  explicit Worker(bool burn)
      : thread_([this, burn] {
          tid.store(syscall(SYS_gettid));
          const uint64_t bound = WallNow() + 6000 * kMillisecond;
          while (!stop.load() && WallNow() < bound) {
            if (burn) UserWork();
            else usleep(10000);
          }
        }) {
    const uint64_t ready = WallNow() + 2000 * kMillisecond;
    while (!tid.load() && WallNow() < ready) usleep(1000);
    CHECK_SCENARIO(tid.load() > 0);
  }
  ~Worker() { stop.store(true); thread_.join(); }
  std::atomic<pid_t> tid{0};
  std::atomic<bool> stop{false};
 private:
  std::thread thread_;
};

void SleepMatrix(pid_t target, bool per_thread) {
  for (unsigned kind : {0U, 1U, 2U}) {
    const clockid_t clock = CpuClock(target, per_thread, kind);
    for (bool absolute : {false, true}) {
      const uint64_t before = ReadClock(clock);
      const uint64_t amount = 25 * kMillisecond;
      const timespec request = Spec(absolute ? before + amount : amount);
      const uint64_t wall = WallNow();
      CHECK_SCENARIO(clock_nanosleep(clock, absolute ? TIMER_ABSTIME : 0,
                                    &request, nullptr) == 0);
      CHECK_SCENARIO(ReadClock(clock) >= before + amount);
      CHECK_SCENARIO(WallNow() - wall < 4000 * kMillisecond);
    }
  }
}

TEST(CpuClockSleep, CurrentProcessProfVirtSchedRelativeAndAbsolute) {
  BoundedScenario([] {
    Worker worker(true);
    SleepMatrix(0, false);
  });
}

TEST(CpuClockSleep, OtherThreadProfVirtSchedRelativeAndAbsolute) {
  BoundedScenario([] {
    Worker worker(true);
    SleepMatrix(worker.tid.load(), true);
  });
}

TEST(CpuClockSleep, RemoteProcessProfVirtSchedRelativeAndAbsolute) {
  BoundedScenario([] {
    int ready[2];
    CHECK_SCENARIO(pipe(ready) == 0);
    const pid_t target = fork();
    CHECK_SCENARIO(target >= 0);
    if (target == 0) {
      close(ready[0]);
      CHECK_SCENARIO(write(ready[1], "r", 1) == 1);
      close(ready[1]);
      const uint64_t bound = WallNow() + 5000 * kMillisecond;
      while (WallNow() < bound) UserWork();
      _exit(0);
    }
    close(ready[1]);
    char byte;
    CHECK_SCENARIO(read(ready[0], &byte, 1) == 1);
    close(ready[0]);
    SleepMatrix(target, false);
    CHECK_SCENARIO(kill(target, SIGKILL) == 0);
    CHECK_SCENARIO(waitpid(target, nullptr, 0) == target);
  });
}

volatile sig_atomic_t signals_seen = 0;
void OnSignal(int) { ++signals_seen; }

void InstallHandler() {
  struct sigaction action{};
  action.sa_handler = OnSignal;
  action.sa_flags = SA_RESTART;
  sigemptyset(&action.sa_mask);
  CHECK_SCENARIO(sigaction(SIGUSR1, &action, nullptr) == 0);
  sigset_t set;
  sigemptyset(&set);
  sigaddset(&set, SIGUSR1);
  CHECK_SCENARIO(pthread_sigmask(SIG_UNBLOCK, &set, nullptr) == 0);
}

void InterruptedSleeps(bool absolute) {
  InstallHandler();
  Worker target(false);
  const pthread_t waiter = pthread_self();
  for (unsigned kind : {0U, 1U, 2U}) {
    const clockid_t clock = CpuClock(target.tid.load(), true, kind);
    const uint64_t before = ReadClock(clock);
    const uint64_t amount = 500 * kMillisecond;
    const timespec request = Spec(absolute ? before + amount : amount);
    timespec remaining{17, 1234567};
    const sig_atomic_t prior_signals = signals_seen;
    // Send after a substantial wall interval while the measured thread is
    // blocked. A wall-clock implementation would deduct ~150ms, unlike CPU.
    std::thread sender([&] {
      const timespec deadline = Spec(WallNow() + 150 * kMillisecond);
      CHECK_SCENARIO(clock_nanosleep(CLOCK_MONOTONIC, TIMER_ABSTIME,
                                    &deadline, nullptr) == 0);
      CHECK_SCENARIO(pthread_kill(waiter, SIGUSR1) == 0);
    });
    const int result = clock_nanosleep(clock, absolute ? TIMER_ABSTIME : 0,
                                     &request, &remaining);
    sender.join();
    // SA_RESTART must not silently restart clock_nanosleep after a handler.
    CHECK_SCENARIO(result == EINTR);
    CHECK_SCENARIO(signals_seen > prior_signals);
    CHECK_SCENARIO(ReadClock(clock) - before < 50 * kMillisecond);
    if (absolute) {
      CHECK_SCENARIO(remaining.tv_sec == 17 && remaining.tv_nsec == 1234567);
    } else {
      CHECK_SCENARIO(Ns(remaining) <= amount);
      CHECK_SCENARIO(Ns(remaining) >= amount - 50 * kMillisecond);
    }
  }
}

TEST(CpuClockSleep, RelativeInterruptUsesCpuRemainderAndIgnoresSaRestart) {
  BoundedScenario([] { InterruptedSleeps(false); });
}

TEST(CpuClockSleep, AbsoluteInterruptDoesNotModifyRemainder) {
  BoundedScenario([] { InterruptedSleeps(true); });
}

TEST(CpuClockSleep, SignalAtCpuDeadlineCannotLeaveAnExpiredWaiterBlocked) {
  BoundedScenario([] {
    InstallHandler();
    const pthread_t waiter = pthread_self();
    for (unsigned iteration = 0; iteration < 12; ++iteration) {
      std::atomic<pid_t> tid{0};
      std::atomic<uint64_t> deadline{0};
      std::atomic<bool> done{false};
      std::thread worker([&] {
        tid.store(syscall(SYS_gettid));
        const uint64_t ready = WallNow() + 2000 * kMillisecond;
        while (!deadline.load() && WallNow() < ready) usleep(1000);
        CHECK_SCENARIO(deadline.load() != 0);
        const uint64_t bound = WallNow() + 3000 * kMillisecond;
        while (ReadClock(CLOCK_THREAD_CPUTIME_ID) < deadline.load() &&
               WallNow() < bound) UserWork();
        CHECK_SCENARIO(ReadClock(CLOCK_THREAD_CPUTIME_ID) >= deadline.load());
        CHECK_SCENARIO(pthread_kill(waiter, SIGUSR1) == 0);
        while (!done.load() && WallNow() < bound) usleep(1000);
      });
      const uint64_t ready = WallNow() + 2000 * kMillisecond;
      while (!tid.load() && WallNow() < ready) usleep(1000);
      CHECK_SCENARIO(tid.load() > 0);
      const clockid_t clock = CpuClock(tid.load(), true, 2);
      const uint64_t expiry = ReadClock(clock) + 8 * kMillisecond;
      deadline.store(expiry);
      const timespec request = Spec(expiry);
      timespec remaining{0, 0};
      const int result = clock_nanosleep(clock, TIMER_ABSTIME, &request,
                                       &remaining);
      // Both orders are legal while the tick/task-work expiry races the
      // signal. Neither may consume the absolute remainder or hang forever.
      CHECK_SCENARIO(result == 0 || result == EINTR);
      CHECK_SCENARIO(remaining.tv_sec == 0 && remaining.tv_nsec == 0);
      done.store(true);
      worker.join();
    }
  });
}

bool WaitSleeping(pid_t pid) {
  const uint64_t bound = WallNow() + 2000 * kMillisecond;
  char path[64];
  snprintf(path, sizeof(path), "/proc/%d/stat", pid);
  do {
    FILE* file = fopen(path, "r");
    if (file) {
      char line[512];
      if (fgets(line, sizeof(line), file)) {
        const char* end = strrchr(line, ')');
        const bool sleeping = end && end[1] == ' ' && end[2] == 'S';
        fclose(file);
        if (sleeping) return true;
      } else fclose(file);
    }
    usleep(1000);
  } while (WallNow() < bound);
  return false;
}

void BurnCpu(uint64_t amount) {
  const uint64_t deadline = ReadClock(CLOCK_THREAD_CPUTIME_ID) + amount;
  while (ReadClock(CLOCK_THREAD_CPUTIME_ID) < deadline) UserWork();
}

TEST(CpuClockSleep, StopContinueKernelRestartPreservesOriginalCpuDeadline) {
  // SIGSTOP/SIGCONT without a handler exercises the real restart-block path;
  // calling restart_syscall after a handled EINTR would not do so.
  BoundedScenario([] {
    int ready[2];
    CHECK_SCENARIO(pipe(ready) == 0);
    const pid_t target = getpid();
    const pid_t waiter = fork();
    CHECK_SCENARIO(waiter >= 0);
    if (waiter == 0) {
      close(ready[0]);
      CHECK_SCENARIO(write(ready[1], "r", 1) == 1);
      close(ready[1]);
      const timespec request = Spec(200 * kMillisecond);
      CHECK_SCENARIO(clock_nanosleep(CpuClock(target, false, 2), 0,
                                    &request, nullptr) == 0);
      _exit(0);
    }
    close(ready[1]);
    char byte;
    CHECK_SCENARIO(read(ready[0], &byte, 1) == 1);
    close(ready[0]);
    CHECK_SCENARIO(WaitSleeping(waiter));
    CHECK_SCENARIO(kill(waiter, SIGSTOP) == 0);
    int status = 0;
    CHECK_SCENARIO(waitpid(waiter, &status, WUNTRACED) == waiter);
    CHECK_SCENARIO(WIFSTOPPED(status));
    BurnCpu(80 * kMillisecond);
    CHECK_SCENARIO(kill(waiter, SIGCONT) == 0);
    BurnCpu(140 * kMillisecond);
    const uint64_t bound = WallNow() + 1000 * kMillisecond;
    pid_t result;
    do {
      result = waitpid(waiter, &status, WNOHANG);
      if (result == waiter) break;
      CHECK_SCENARIO(result == 0 || errno == EINTR);
      usleep(1000);
    } while (WallNow() < bound);
    CHECK_SCENARIO(result == waiter);
    CHECK_SCENARIO(WIFEXITED(status) && WEXITSTATUS(status) == 0);
  });
}

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
