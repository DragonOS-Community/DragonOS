#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <cstdint>
#include <functional>
#include <thread>

namespace {

uint64_t Ns(const timespec& value) {
  return uint64_t(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}
uint64_t Now(clockid_t clock = CLOCK_MONOTONIC) {
  timespec value{};
  if (clock_gettime(clock, &value) != 0) return 0;
  return Ns(value);
}
clockid_t CpuClock(pid_t pid, bool thread, unsigned kind) {
  return static_cast<clockid_t>((~static_cast<uint32_t>(pid) << 3) |
                               (thread ? 4U : 0U) | kind);
}

// Every delivery test runs in a disposable child. A failed assertion cannot
// unblock a default-fatal pending signal in the test runner, and a broken CPU
// timer cannot hang CI. The wall deadline is independent of the tested clock.
void Isolated(const std::function<void()>& body) {
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) {
    body();
    _exit(testing::Test::HasFailure() ? 1 : 0);
  }
  int status = 0;
  const uint64_t deadline = Now() + 5000000000ULL;
  while (Now() < deadline) {
    const pid_t result = waitpid(pid, &status, WNOHANG);
    if (result == pid) {
      ASSERT_TRUE(WIFEXITED(status));
      EXPECT_EQ(0, WEXITSTATUS(status));
      return;
    }
    if (result < 0 && errno != EINTR) break;
    usleep(1000);
  }
  kill(pid, SIGKILL); // Only this test's positive, owned child PID.
  while (waitpid(pid, &status, 0) < 0 && errno == EINTR) {}
  FAIL() << "isolated timer scenario exceeded its wall watchdog";
}

bool Burn(clockid_t clock, uint64_t duration) {
  const uint64_t cpu = Now(clock) + duration;
  const uint64_t wall = Now() + 2000000000ULL;
  volatile uint64_t work = 1;
  while (Now(clock) < cpu) {
    for (unsigned i = 0; i < 1000; ++i) work = work * 33 + i;
    syscall(SYS_getpid);
    if (Now() >= wall) return false;
  }
  return true;
}

struct Timer {
  int id = -1;
  ~Timer() { if (id >= 0) syscall(SYS_timer_delete, id); }
  long Create(clockid_t clock, int signo = SIGRTMIN, int value = 17,
              pid_t tid = 0) {
    sigevent event{};
    event.sigev_notify = tid ? SIGEV_THREAD_ID : SIGEV_SIGNAL;
    event.sigev_signo = signo;
    event.sigev_value.sival_int = value;
    if (tid) event._sigev_un._tid = tid;
    return syscall(SYS_timer_create, clock, &event, &id);
  }
  long Arm(uint64_t first, uint64_t interval = 0, int flags = 0,
           itimerspec* old = nullptr) {
    itimerspec setting{};
    setting.it_value = {static_cast<time_t>(first / 1000000000ULL),
                        static_cast<long>(first % 1000000000ULL)};
    setting.it_interval = {static_cast<time_t>(interval / 1000000000ULL),
                           static_cast<long>(interval % 1000000000ULL)};
    return syscall(SYS_timer_settime, id, flags, &setting, old);
  }
};

sigset_t Block(int signo = SIGRTMIN) {
  sigset_t set{};
  sigemptyset(&set);
  sigaddset(&set, signo);
  EXPECT_EQ(0, sigprocmask(SIG_BLOCK, &set, nullptr));
  return set;
}
int Take(const sigset_t& set, siginfo_t* info, bool wait = true) {
  const timespec timeout = {wait ? 1 : 0, 0};
  return sigtimedwait(&set, info, &timeout);
}
bool Pending(int signo = SIGRTMIN) {
  sigset_t set{};
  if (sigpending(&set) != 0) return false;
  return sigismember(&set, signo) == 1;
}
bool BurnUntilPending(clockid_t clock) {
  const uint64_t deadline = Now() + 2000000000ULL;
  while (!Pending() && Now() < deadline) {
    if (!Burn(clock, 2000000)) return false;
  }
  return Pending();
}

class CpuTimerDeliveryKinds : public testing::TestWithParam<clockid_t> {};

TEST_P(CpuTimerDeliveryKinds, PeriodicDequeueReportsOverrunAndReloads) {
  const clockid_t clock = GetParam();
  Isolated([&] {
    const auto signals = Block();
    Timer timer;
    ASSERT_EQ(0, timer.Create(clock));
    ASSERT_EQ(0, timer.Arm(5000000, 10000000));
    ASSERT_TRUE(BurnUntilPending(clock));
    ASSERT_TRUE(Burn(clock, 40000000));
    siginfo_t info{};
    ASSERT_EQ(SIGRTMIN, Take(signals, &info));
    EXPECT_EQ(SI_TIMER, info.si_code);
    EXPECT_EQ(17, info.si_value.sival_int);
    EXPECT_GE(info.si_overrun, 3);
    EXPECT_EQ(info.si_overrun, syscall(SYS_timer_getoverrun, timer.id));
    itimerspec current{};
    ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
    EXPECT_GT(Ns(current.it_value), 0ULL);
    EXPECT_LE(Ns(current.it_value), 10000000ULL);
    ASSERT_TRUE(BurnUntilPending(clock));
    ASSERT_EQ(SIGRTMIN, Take(signals, &info));
    EXPECT_EQ(info.si_overrun, syscall(SYS_timer_getoverrun, timer.id));
  });
}

TEST_P(CpuTimerDeliveryKinds, SettimeOldValueProjectsExpiredPeriodicDeadline) {
  const clockid_t clock = GetParam();
  Isolated([&] {
    Block();
    Timer timer;
    ASSERT_EQ(0, timer.Create(clock));
    ASSERT_EQ(0, timer.Arm(5000000, 200000000));
    ASSERT_TRUE(BurnUntilPending(clock));
    itimerspec current{}, old{};
    ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
    EXPECT_EQ(1ULL, Ns(current.it_value)); // gettime does not reload.
    ASSERT_EQ(0, timer.Arm(0, 0, 0, &old));
    EXPECT_GT(Ns(old.it_value), 1ULL); // settime does reload its old value.
    EXPECT_LE(Ns(old.it_value), 200000000ULL);
    EXPECT_EQ(200000000ULL, Ns(old.it_interval));
  });
}

INSTANTIATE_TEST_SUITE_P(AllKinds, CpuTimerDeliveryKinds, testing::Values(
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID,
    CpuClock(0, false, 0), CpuClock(0, false, 1), CpuClock(0, false, 2),
    CpuClock(0, true, 0), CpuClock(0, true, 1), CpuClock(0, true, 2)));

TEST(CpuTimerDelivery, IgnoredPeriodicTimerBecomesDeliverable) {
  Isolated([] {
    struct sigaction action{};
    action.sa_handler = SIG_IGN;
    sigemptyset(&action.sa_mask);
    ASSERT_EQ(0, sigaction(SIGRTMIN, &action, nullptr));
    Timer timer;
    ASSERT_EQ(0, timer.Create(CLOCK_PROCESS_CPUTIME_ID));
    ASSERT_EQ(0, timer.Arm(5000000, 10000000));
    ASSERT_TRUE(Burn(CLOCK_PROCESS_CPUTIME_ID, 40000000));
    EXPECT_FALSE(Pending());
    const auto signals = Block();
    action.sa_handler = SIG_DFL;
    ASSERT_EQ(0, sigaction(SIGRTMIN, &action, nullptr));
    ASSERT_TRUE(BurnUntilPending(CLOCK_PROCESS_CPUTIME_ID));
    siginfo_t info{};
    ASSERT_EQ(SIGRTMIN, Take(signals, &info));
    EXPECT_EQ(SI_TIMER, info.si_code);
    EXPECT_EQ(info.si_overrun, syscall(SYS_timer_getoverrun, timer.id));
  });
}

TEST(CpuTimerDelivery, DistinctTimersSharingSignoKeepTheirOwnSigvals) {
  Isolated([] {
    const auto signals = Block();
    Timer first, second;
    ASSERT_EQ(0, first.Create(CLOCK_PROCESS_CPUTIME_ID, SIGRTMIN, 11));
    ASSERT_EQ(0, second.Create(CLOCK_PROCESS_CPUTIME_ID, SIGRTMIN, 22));
    ASSERT_EQ(0, first.Arm(5000000));
    ASSERT_EQ(0, second.Arm(5000000));
    ASSERT_TRUE(Burn(CLOCK_PROCESS_CPUTIME_ID, 40000000));
    siginfo_t a{}, b{};
    ASSERT_EQ(SIGRTMIN, Take(signals, &a));
    ASSERT_EQ(SIGRTMIN, Take(signals, &b));
    EXPECT_EQ(SI_TIMER, a.si_code);
    EXPECT_EQ(SI_TIMER, b.si_code);
    EXPECT_TRUE((a.si_value.sival_int == 11 && b.si_value.sival_int == 22) ||
                (a.si_value.sival_int == 22 && b.si_value.sival_int == 11));
    errno = 0;
    EXPECT_EQ(-1, Take(signals, &a, false));
    EXPECT_EQ(EAGAIN, errno);
  });
}

TEST(CpuTimerDelivery, InvalidSettingLeavesStateAndUnknownFlagsAreAccepted) {
  Isolated([] {
    Block();
    Timer timer;
    ASSERT_EQ(0, timer.Create(CLOCK_PROCESS_CPUTIME_ID));
    ASSERT_EQ(0, timer.Arm(1000000000, 2000000000, 0x100));
    itimerspec invalid{}, old{}, current{};
    invalid.it_value.tv_nsec = 1000000000;
    old.it_value.tv_sec = 123;
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_timer_settime, timer.id, 0, &invalid, &old));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(123, old.it_value.tv_sec);
    ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
    EXPECT_GT(Ns(current.it_value), 800000000ULL);
    EXPECT_EQ(2000000000ULL, Ns(current.it_interval));
    // Unknown bits do not cancel TIMER_ABSTIME's meaning.
    ASSERT_EQ(0, timer.Arm(Now(CLOCK_PROCESS_CPUTIME_ID) + 1000000000ULL,
                           0, 0x101));
    ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
    EXPECT_GT(Ns(current.it_value), 800000000ULL);
  });
}

TEST(CpuTimerDelivery, ResetWithOldPendingSignalRearmsNewGeneration) {
  Isolated([] {
    const auto signals = Block();
    Timer timer;
    ASSERT_EQ(0, timer.Create(CLOCK_PROCESS_CPUTIME_ID));
    ASSERT_EQ(0, timer.Arm(5000000, 10000000));
    ASSERT_TRUE(BurnUntilPending(CLOCK_PROCESS_CPUTIME_ID));
    ASSERT_EQ(0, timer.Arm(5000000, 10000000));
    ASSERT_TRUE(Burn(CLOCK_PROCESS_CPUTIME_ID, 30000000));
    siginfo_t info{};
    ASSERT_EQ(SIGRTMIN, Take(signals, &info));
    EXPECT_GE(info.si_overrun, 1);
    // send_sigqueue coalesces the old pending entry once. Linux reports that
    // extra occurrence in si_overrun, not in the new arm's it_overrun_last.
    EXPECT_EQ(info.si_overrun - 1, syscall(SYS_timer_getoverrun, timer.id));
    ASSERT_TRUE(BurnUntilPending(CLOCK_PROCESS_CPUTIME_ID));
    ASSERT_EQ(SIGRTMIN, Take(signals, &info));
    EXPECT_EQ(SI_TIMER, info.si_code);
    EXPECT_EQ(info.si_overrun, syscall(SYS_timer_getoverrun, timer.id));
  });
}

TEST(CpuTimerDelivery, RemoteProcessClockNotifiesTimerOwner) {
  Isolated([] {
    const auto signals = Block();
    int ready[2];
    ASSERT_EQ(0, pipe(ready));
    const pid_t target = fork();
    ASSERT_GE(target, 0);
    if (target == 0) {
      close(ready[1]);
      char byte;
      if (read(ready[0], &byte, 1) != 1) _exit(20);
      if (!Burn(CLOCK_PROCESS_CPUTIME_ID, 40000000)) _exit(21);
      if (Pending()) _exit(22); // The measured process is not the signal owner.
      _exit(0);
    }
    close(ready[0]);
    // No assertion can bypass cleanup of the measured child.
    Timer timer;
    const long created = timer.Create(CpuClock(target, false, 2));
    EXPECT_EQ(0, created);
    if (created == 0) { EXPECT_EQ(0, timer.Arm(10000000)); }
    EXPECT_EQ(1, write(ready[1], "x", 1));
    close(ready[1]);
    siginfo_t info{};
    EXPECT_EQ(SIGRTMIN, Take(signals, &info));
    EXPECT_EQ(SI_TIMER, info.si_code);
    int status = 0;
    const uint64_t deadline = Now() + 2000000000ULL;
    pid_t result;
    do { result = waitpid(target, &status, WNOHANG); if (!result) usleep(1000); }
    while (!result && Now() < deadline);
    if (!result) { kill(target, SIGKILL); waitpid(target, &status, 0); }
    EXPECT_EQ(target, result);
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
  });
}

TEST(CpuTimerDelivery, ThreadIdNotificationIsOnlyConsumedByChosenThread) {
  Isolated([] {
    const auto signals = Block();
    std::atomic<pid_t> tid{0};
    std::atomic<bool> start{false};
    std::atomic<int> delivered{0};
    std::thread worker([&] {
      tid.store(syscall(SYS_gettid));
      const uint64_t deadline = Now() + 2000000000ULL;
      while (!start.load() && Now() < deadline) usleep(1000);
      if (!start.load() || !Burn(CLOCK_THREAD_CPUTIME_ID, 30000000)) return;
      siginfo_t info{};
      if (Take(signals, &info) == SIGRTMIN && info.si_code == SI_TIMER &&
          info.si_value.sival_int == 29) delivered.store(1);
    });
    const uint64_t deadline = Now() + 1000000000ULL;
    while (!tid.load() && Now() < deadline) usleep(1000);
    EXPECT_GT(tid.load(), 0);
    Timer timer;
    if (tid.load() > 0) {
      const long created = timer.Create(CpuClock(tid.load(), true, 2),
                                        SIGRTMIN, 29, tid.load());
      EXPECT_EQ(0, created);
      if (created == 0) { EXPECT_EQ(0, timer.Arm(5000000)); }
    }
    start.store(true);
    worker.join();
    EXPECT_EQ(1, delivered.load());
    siginfo_t info{};
    errno = 0;
    EXPECT_EQ(-1, Take(signals, &info, false));
    EXPECT_EQ(EAGAIN, errno);
  });
}

TEST(CpuTimerDelivery, ExecDeletesTimerIdsAndOldPendingTimerSignals) {
  Isolated([] {
    Block();
    Timer timer;
    ASSERT_EQ(0, timer.Create(CLOCK_PROCESS_CPUTIME_ID));
    ASSERT_EQ(0, timer.Arm(5000000, 10000000));
    ASSERT_TRUE(BurnUntilPending(CLOCK_PROCESS_CPUTIME_ID));
    char id[32];
    snprintf(id, sizeof(id), "%d", timer.id);
    execl("/proc/self/exe", "cpu_timer_delivery_test",
          "--cpu-timer-exec-check", id, nullptr);
    FAIL() << "exec: " << strerror(errno);
  });
}

volatile sig_atomic_t handler_tid = 0;
void RecordTid(int) { handler_tid = syscall(SYS_gettid); }

TEST(CpuTimerDelivery, GroupSignalSelectsUnblockedWorkerNotBlockedLeader) {
  Isolated([] {
    const auto signals = Block();
    struct sigaction action{};
    action.sa_handler = RecordTid;
    sigemptyset(&action.sa_mask);
    ASSERT_EQ(0, sigaction(SIGRTMIN, &action, nullptr));
    handler_tid = 0;
    std::atomic<pid_t> tid{0};
    std::atomic<bool> ready{false}, stop{false};
    std::thread worker([&] {
      tid.store(syscall(SYS_gettid));
      if (sigprocmask(SIG_UNBLOCK, &signals, nullptr) != 0) return;
      ready.store(true);
      const uint64_t deadline = Now() + 2000000000ULL;
      while (!stop.load() && Now() < deadline) {
        if (!Burn(CLOCK_THREAD_CPUTIME_ID, 2000000)) break;
      }
    });
    const uint64_t ready_deadline = Now() + 1000000000ULL;
    while (!ready.load() && Now() < ready_deadline) usleep(1000);
    EXPECT_TRUE(ready.load());
    Timer timer;
    const long created = timer.Create(CLOCK_PROCESS_CPUTIME_ID);
    EXPECT_EQ(0, created);
    if (created == 0) { EXPECT_EQ(0, timer.Arm(5000000)); }
    const uint64_t deadline = Now() + 1500000000ULL;
    while (!handler_tid && Now() < deadline) usleep(1000);
    stop.store(true);
    worker.join();
    EXPECT_EQ(tid.load(), handler_tid);
    EXPECT_FALSE(Pending());
  });
}

TEST(CpuTimerDelivery, DefaultFatalTimerTerminatesEntireThreadGroup) {
  Isolated([] {
    const pid_t target = fork();
    ASSERT_GE(target, 0);
    if (target == 0) {
      struct sigaction action{};
      action.sa_handler = SIG_DFL;
      sigemptyset(&action.sa_mask);
      sigaction(SIGTERM, &action, nullptr);
      sigset_t set{};
      sigemptyset(&set);
      sigaddset(&set, SIGTERM);
      sigprocmask(SIG_UNBLOCK, &set, nullptr);
      std::thread worker([] {
        const uint64_t deadline = Now() + 2500000000ULL;
        while (Now() < deadline) Burn(CLOCK_THREAD_CPUTIME_ID, 2000000);
      });
      Timer timer;
      if (timer.Create(CLOCK_PROCESS_CPUTIME_ID, SIGTERM) != 0 ||
          timer.Arm(10000000) != 0) _exit(81);
      worker.join();
      _exit(82); // No fatal signal delivered within the CPU/wall budget.
    }
    int status = 0;
    const uint64_t deadline = Now() + 3000000000ULL;
    pid_t result;
    do { result = waitpid(target, &status, WNOHANG); if (!result) usleep(1000); }
    while (!result && Now() < deadline);
    if (!result) { kill(target, SIGKILL); waitpid(target, &status, 0); }
    ASSERT_EQ(target, result);
    ASSERT_TRUE(WIFSIGNALED(status));
    EXPECT_EQ(SIGTERM, WTERMSIG(status));
  });
}

TEST(CpuTimerDelivery, CreatorThreadExitPreservesGroupSignalTimer) {
  Isolated([] {
    const auto signals = Block();
    Timer timer;
    std::thread creator([&] {
      EXPECT_EQ(0, timer.Create(CLOCK_PROCESS_CPUTIME_ID));
      if (timer.id >= 0) { EXPECT_EQ(0, timer.Arm(20000000)); }
    });
    creator.join();
    ASSERT_GE(timer.id, 0);
    ASSERT_TRUE(BurnUntilPending(CLOCK_PROCESS_CPUTIME_ID));
    siginfo_t info{};
    ASSERT_EQ(SIGRTMIN, Take(signals, &info));
    EXPECT_EQ(SI_TIMER, info.si_code);
    EXPECT_EQ(17, info.si_value.sival_int);
  });
}

} // namespace

int main(int argc, char** argv) {
  if (argc == 3 && strcmp(argv[1], "--cpu-timer-exec-check") == 0) {
    itimerspec current{};
    errno = 0;
    if (syscall(SYS_timer_gettime, atoi(argv[2]), &current) != -1 ||
        errno != EINVAL || Pending()) return 30;
    return 0;
  }
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
