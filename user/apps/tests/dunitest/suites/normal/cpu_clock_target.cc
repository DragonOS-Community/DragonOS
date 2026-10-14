#include <gtest/gtest.h>

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <cstdint>
#include <thread>

namespace {

// Linux MAKE_PROCESS_CPUCLOCK / MAKE_THREAD_CPUCLOCK, without signed shifts.
clockid_t CpuClock(pid_t pid, bool thread, unsigned kind) {
  return static_cast<clockid_t>((~static_cast<uint32_t>(pid) << 3) |
                               (thread ? 4U : 0U) | kind);
}

uint64_t Nanos(const timespec& value) {
  return uint64_t(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}

uint64_t WallNow() {
  timespec value{};
  if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) return 0;
  return Nanos(value);
}

void Burn(unsigned milliseconds, bool system = false) {
  const uint64_t deadline = WallNow() + milliseconds * 1000000ULL;
  volatile unsigned long work = 1;
  do {
    for (unsigned i = 0; i < 500; ++i) {
      if (system) syscall(SYS_getpid);
      else work = work * 33 + i;
    }
  } while (WallNow() < deadline);
}

class Child {
 public:
  explicit Child(pid_t pid) : pid_(pid) {}
  ~Child() {
    if (pid_ > 0) { kill(pid_, SIGKILL); Reap(nullptr); }
  }
  bool Reap(int* status) {
    const uint64_t deadline = WallNow() + 3000000000ULL;
    do {
      const pid_t result = waitpid(pid_, status, WNOHANG);
      if (result == pid_) { pid_ = -1; return true; }
      if (result < 0 && errno == ECHILD) { pid_ = -1; return false; }
      if (result < 0 && errno != EINTR) return false;
      usleep(1000);
    } while (WallNow() < deadline);
    return false;
  }
 private:
  pid_t pid_;
};

class KernelTimer {
 public:
  ~KernelTimer() {
    if (id >= 0) { EXPECT_EQ(0, syscall(SYS_timer_delete, id)); }
  }
  int id = -1;
  long Create(clockid_t clock, sigevent* event) {
    // Linux may copy its tentative ID before target validation fails.
    int created = -1;
    const long result = syscall(SYS_timer_create, clock, event, &created);
    if (result == 0) id = created;
    return result;
  }
};

TEST(CpuClockTarget, InvalidTargetsAndClockKindsAreRejected) {
  timespec value{};
  for (bool thread : {false, true}) {
    for (unsigned kind : {0U, 1U, 2U, 3U}) {
      const clockid_t clock = CpuClock(1 << 28, thread, kind);
      errno = 0;
      EXPECT_EQ(-1, clock_gettime(clock, &value));
      EXPECT_EQ(EINVAL, errno);
      errno = 0;
      EXPECT_EQ(-1, clock_getres(clock, &value));
      EXPECT_EQ(EINVAL, errno);
    }
  }
}

TEST(CpuClockTarget, ChildProcessClockDoesNotSampleCaller) {
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) { sleep(2); _exit(0); }
  Child child(pid);
  timespec before{}, after{}, resolution{};
  for (unsigned kind : {0U, 1U, 2U}) {
    const clockid_t clock = CpuClock(pid, false, kind);
    ASSERT_EQ(0, clock_getres(clock, &resolution));
    EXPECT_EQ(0, resolution.tv_sec);
    if (kind == 2) EXPECT_EQ(1, resolution.tv_nsec);
    else EXPECT_GT(resolution.tv_nsec, 1);
    ASSERT_EQ(0, clock_gettime(clock, &before));
    Burn(60);
    ASSERT_EQ(0, clock_gettime(clock, &after));
    EXPECT_GE(Nanos(after), Nanos(before));
    EXPECT_LT(Nanos(after) - Nanos(before), 30000000ULL);
    errno = 0;
    EXPECT_EQ(-1, clock_gettime(CpuClock(pid, true, kind), &after));
    EXPECT_EQ(EINVAL, errno);
  }
  ASSERT_EQ(0, kill(pid, SIGKILL));
  ASSERT_TRUE(child.Reap(nullptr));
  errno = 0;
  EXPECT_EQ(-1, clock_gettime(CpuClock(pid, false, 2), &after));
  EXPECT_EQ(EINVAL, errno);
}

TEST(CpuClockTarget, SameGroupThreadIsQueryableWhileAlive) {
  std::atomic<pid_t> tid{0};
  std::atomic<bool> stop{false};
  std::thread worker([&] {
    tid.store(syscall(SYS_gettid));
    const uint64_t deadline = WallNow() + 2000000000ULL;
    while (!stop.load() && WallNow() < deadline) usleep(1000);
  });
  const uint64_t deadline = WallNow() + 1000000000ULL;
  while (!tid.load() && WallNow() < deadline) usleep(1000);
  const pid_t target = tid.load();
  EXPECT_GT(target, 0);
  if (target > 0) {
    for (unsigned kind : {0U, 1U, 2U}) {
      timespec value{};
      EXPECT_EQ(0, clock_gettime(CpuClock(target, true, kind), &value));
      EXPECT_EQ(0, clock_getres(CpuClock(target, true, kind), &value));
      errno = 0;
      EXPECT_EQ(-1, clock_gettime(CpuClock(target, false, kind), &value));
      EXPECT_EQ(EINVAL, errno);
    }
  }
  stop.store(true);
  worker.join();
  timespec value{};
  // CLONE_CHILD_CLEARTID wakes pthread_join before release_task/RCU removal.
  const uint64_t retired = WallNow() + 3000000000ULL;
  int result;
  do {
    errno = 0;
    result = clock_gettime(CpuClock(target, true, 2), &value);
    if (result < 0) break;
    usleep(1000);
  } while (WallNow() < retired);
  EXPECT_EQ(-1, result);
  EXPECT_EQ(EINVAL, errno);
}

TEST(CpuClockTarget, NonLeaderSelfProcessAliasOnlyWorksForGettime) {
  std::thread worker([] {
    const pid_t tid = syscall(SYS_gettid);
    for (unsigned kind : {0U, 1U, 2U}) {
      const clockid_t clock = CpuClock(tid, false, kind);
      timespec value{};
      EXPECT_EQ(0, clock_gettime(clock, &value));
      errno = 0;
      EXPECT_EQ(-1, clock_getres(clock, &value));
      EXPECT_EQ(EINVAL, errno);
      sigevent event{};
      event.sigev_notify = SIGEV_NONE;
      KernelTimer timer;
      errno = 0;
      EXPECT_EQ(-1, timer.Create(clock, &event));
      EXPECT_EQ(EINVAL, errno);
    }
  });
  worker.join();
}

TEST(CpuClockTarget, ProfIncludesSystemTimeButVirtDoesNot) {
  timespec prof_before{}, virt_before{}, prof_after{}, virt_after{};
  ASSERT_EQ(0, clock_gettime(CpuClock(0, true, 0), &prof_before));
  ASSERT_EQ(0, clock_gettime(CpuClock(0, true, 1), &virt_before));
  Burn(150, true);
  ASSERT_EQ(0, clock_gettime(CpuClock(0, true, 1), &virt_after));
  ASSERT_EQ(0, clock_gettime(CpuClock(0, true, 0), &prof_after));
  const uint64_t prof = Nanos(prof_after) - Nanos(prof_before);
  const uint64_t virt = Nanos(virt_after) - Nanos(virt_before);
  EXPECT_GT(prof, virt + 1000000ULL);
}

TEST(CpuClockTarget, SettimeIsForbiddenAfterTargetValidation) {
  const timespec value{};
  for (unsigned kind : {0U, 1U, 2U}) {
    errno = 0;
    EXPECT_EQ(-1, clock_settime(CpuClock(0, true, kind), &value));
    EXPECT_EQ(EPERM, errno);
    errno = 0;
    EXPECT_EQ(-1, clock_settime(CpuClock(1 << 28, true, kind), &value));
    EXPECT_EQ(EINVAL, errno);
  }
}

TEST(CpuClockTarget, EncodedThreadSleepRejectsSelfButWaitsForSibling) {
  // A wrong self target can sleep forever, so keep the whole scenario in a
  // child with a wall-clock watchdog outside the CPU clock being measured.
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) {
    const timespec request{0, 20000000};
    const pid_t self = syscall(SYS_gettid);
    for (unsigned kind : {0U, 1U, 2U}) {
      for (pid_t target : {pid_t(0), self}) {
        if (clock_nanosleep(CpuClock(target, true, kind), 0, &request, nullptr)
            != EINVAL) _exit(120);
      }
    }
    std::atomic<pid_t> tid{0};
    std::atomic<bool> stop{false};
    std::thread worker([&] {
      tid.store(syscall(SYS_gettid));
      const uint64_t deadline = WallNow() + 1500000000ULL;
      while (!stop.load() && WallNow() < deadline) Burn(5);
    });
    const uint64_t ready = WallNow() + 1000000000ULL;
    while (!tid.load() && WallNow() < ready) usleep(1000);
    int result = EINVAL;
    if (tid.load()) result = clock_nanosleep(CpuClock(tid.load(), true, 2),
                                           0, &request, nullptr);
    stop.store(true);
    worker.join();
    _exit(result == 0 ? 0 : 121);
  }
  Child child(pid);
  int status = 0;
  ASSERT_TRUE(child.Reap(&status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

class CpuTimerLifecycle : public testing::TestWithParam<clockid_t> {};

TEST_P(CpuTimerLifecycle, ArmQueryDisarmDelete) {
  sigevent event{};
  event.sigev_notify = SIGEV_NONE;
  KernelTimer timer;
  ASSERT_EQ(0, timer.Create(GetParam(), &event));
  itimerspec value{}, current{};
  ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
  EXPECT_EQ(0ULL, Nanos(current.it_value));
  value.it_value.tv_sec = 10;
  value.it_interval.tv_sec = 2;
  ASSERT_EQ(0, syscall(SYS_timer_settime, timer.id, 0, &value, nullptr));
  ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
  EXPECT_GT(Nanos(current.it_value), 9000000000ULL);
  EXPECT_EQ(2000000000ULL, Nanos(current.it_interval));
  value = {};
  ASSERT_EQ(0, syscall(SYS_timer_settime, timer.id, 0, &value, nullptr));
  ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
  EXPECT_EQ(0ULL, Nanos(current.it_value));
  ASSERT_EQ(0, syscall(SYS_timer_delete, timer.id));
  const int deleted = timer.id;
  timer.id = -1;
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_timer_gettime, deleted, &current));
  EXPECT_EQ(EINVAL, errno);
}

TEST_P(CpuTimerLifecycle, NoneExpiresOnceEvenWithPeriodicInterval) {
  // Unlike wall-clock SIGEV_NONE timers, cpu_timer_fire() clears expires
  // without rearming. The configured interval is still visible to gettime.
  sigevent event{};
  event.sigev_notify = SIGEV_NONE;
  KernelTimer timer;
  ASSERT_EQ(0, timer.Create(GetParam(), &event));
  itimerspec value{}, current{};
  value.it_value.tv_nsec = 10000000;
  value.it_interval.tv_nsec = 3000000;
  ASSERT_EQ(0, syscall(SYS_timer_settime, timer.id, 0, &value, nullptr));
  const uint64_t deadline = WallNow() + 1500000000ULL;
  do {
    Burn(5);
    ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
    if (Nanos(current.it_value) == 0) break;
  } while (WallNow() < deadline);
  EXPECT_EQ(0ULL, Nanos(current.it_value));
  EXPECT_EQ(3000000ULL, Nanos(current.it_interval));
  Burn(20);
  ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
  EXPECT_EQ(0ULL, Nanos(current.it_value));
  EXPECT_EQ(3000000ULL, Nanos(current.it_interval));
}

INSTANTIATE_TEST_SUITE_P(AllKinds, CpuTimerLifecycle, testing::Values(
    CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID,
    CpuClock(0, false, 0), CpuClock(0, false, 1), CpuClock(0, false, 2),
    CpuClock(0, true, 0), CpuClock(0, true, 1), CpuClock(0, true, 2)));

TEST(CpuClockTarget, TimerIdIsSharedByOwnerGroupButNotInheritedByFork) {
  sigevent event{};
  event.sigev_notify = SIGEV_NONE;
  KernelTimer timer;
  ASSERT_EQ(0, timer.Create(CLOCK_PROCESS_CPUTIME_ID, &event));
  const int id = timer.id;
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) {
    itimerspec value{};
    errno = 0;
    const long result = syscall(SYS_timer_gettime, id, &value);
    _exit(result == -1 && errno == EINVAL ? 0 : 120);
  }
  Child child(pid);
  int status = 0;
  ASSERT_TRUE(child.Reap(&status));
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  std::thread sibling([&] {
    itimerspec value{};
    EXPECT_EQ(0, syscall(SYS_timer_gettime, id, &value));
    value.it_value.tv_sec = 10;
    EXPECT_EQ(0, syscall(SYS_timer_settime, id, 0, &value, nullptr));
    EXPECT_EQ(0, syscall(SYS_timer_delete, id));
  });
  sibling.join();
  timer.id = -1;
  itimerspec value{};
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_timer_gettime, id, &value));
  EXPECT_EQ(EINVAL, errno);
}

TEST(CpuClockTarget, CreatorThreadExitDoesNotDeleteGroupOwnedTimer) {
  for (clockid_t clock : {CLOCK_PROCESS_CPUTIME_ID, CLOCK_THREAD_CPUTIME_ID}) {
    KernelTimer timer;
    std::thread creator([&] {
      sigevent event{};
      event.sigev_notify = SIGEV_NONE;
      ASSERT_EQ(0, timer.Create(clock, &event));
      itimerspec value{};
      value.it_value.tv_sec = 10;
      value.it_interval.tv_sec = 2;
      EXPECT_EQ(0, syscall(SYS_timer_settime, timer.id, 0, &value, nullptr));
    });
    creator.join();
    ASSERT_GE(timer.id, 0);
    itimerspec value{};
    ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &value));
    if (clock == CLOCK_PROCESS_CPUTIME_ID) {
      EXPECT_GT(Nanos(value.it_value), 9000000000ULL);
      EXPECT_EQ(2000000000ULL, Nanos(value.it_interval));
      value.it_value.tv_sec = 10;
      ASSERT_EQ(0, syscall(SYS_timer_settime, timer.id, 0, &value, nullptr));
    } else {
      // pthread_join may precede release_task; wait for definitive retirement.
      const uint64_t deadline = WallNow() + 1500000000ULL;
      do {
        ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &value));
        if (Nanos(value.it_value) == 0) break;
        usleep(1000);
      } while (WallNow() < deadline);
      EXPECT_EQ(0ULL, Nanos(value.it_value));
      value.it_value.tv_sec = 10;
      errno = 0;
      EXPECT_EQ(-1, syscall(SYS_timer_settime, timer.id, 0, &value, nullptr));
      EXPECT_EQ(ESRCH, errno);
    }
  }
}

TEST(CpuClockTarget, TimerOutlivesTargetButCannotBeRearmed) {
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) { sleep(2); _exit(0); }
  Child child(pid);
  sigevent event{};
  event.sigev_notify = SIGEV_NONE;
  KernelTimer timer;
  ASSERT_EQ(0, timer.Create(CpuClock(pid, false, 2), &event));
  itimerspec value{}, current{};
  value.it_value.tv_sec = 10;
  ASSERT_EQ(0, syscall(SYS_timer_settime, timer.id, 0, &value, nullptr));
  ASSERT_EQ(0, kill(pid, SIGKILL));
  ASSERT_TRUE(child.Reap(nullptr));
  ASSERT_EQ(0, syscall(SYS_timer_gettime, timer.id, &current));
  EXPECT_EQ(0ULL, Nanos(current.it_value));
  errno = 0;
  EXPECT_EQ(-1, syscall(SYS_timer_settime, timer.id, 0, &value, nullptr));
  EXPECT_EQ(ESRCH, errno);
}

TEST(CpuClockTarget, TimerSignalExpiresOnCpuRatherThanWallTime) {
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) {
    sigset_t mask;
    sigemptyset(&mask);
    sigaddset(&mask, SIGUSR1);
    if (sigprocmask(SIG_BLOCK, &mask, nullptr) != 0) _exit(120);
    sigevent event{};
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 0x80;
    KernelTimer timer;
    if (timer.Create(CLOCK_THREAD_CPUTIME_ID, &event) != 0) _exit(121);
    itimerspec value{};
    value.it_value.tv_nsec = 30000000;
    if (syscall(SYS_timer_settime, timer.id, 0, &value, nullptr) != 0) _exit(122);
    usleep(80000);
    siginfo_t info{};
    const timespec zero{};
    if (sigtimedwait(&mask, &info, &zero) != -1 || errno != EAGAIN) _exit(123);
    const uint64_t deadline = WallNow() + 1500000000ULL;
    int received = -1;
    do {
      Burn(5);
      received = sigtimedwait(&mask, &info, &zero);
      if (received == SIGUSR1) break;
      if (errno != EAGAIN && errno != EINTR) _exit(124);
    } while (WallNow() < deadline);
    if (received != SIGUSR1 || info.si_code != SI_TIMER ||
        info.si_value.sival_int != 0x80) _exit(125);
    if (syscall(SYS_timer_delete, timer.id) != 0) _exit(126);
    timer.id = -1;
    _exit(0);
  }
  Child child(pid);
  int status = 0;
  ASSERT_TRUE(child.Reap(&status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(CpuClockTarget, DynamicTargetUsesVisiblePidNamespace) {
  const pid_t pid = fork();
  ASSERT_GE(pid, 0);
  if (pid == 0) {
    const pid_t supervisor = getpid();
    if (unshare(CLONE_NEWPID) != 0) _exit(errno == EPERM ? 77 : 120);
    const pid_t nested = fork();
    if (nested < 0) _exit(121);
    if (nested == 0) {
      timespec value{};
      if (getpid() != 1) _exit(122);
      if (clock_gettime(CpuClock(1, false, 2), &value) != 0) _exit(123);
      // The outer supervisor PID is not a visible target in this namespace.
      errno = 0;
      if (clock_gettime(CpuClock(supervisor, false, 2), &value) != -1 ||
          errno != EINVAL) _exit(124);
      _exit(0);
    }
    Child child(nested);
    int status = 0;
    if (!child.Reap(&status) || !WIFEXITED(status)) _exit(125);
    _exit(WEXITSTATUS(status));
  }
  Child child(pid);
  int status = 0;
  ASSERT_TRUE(child.Reap(&status));
  ASSERT_TRUE(WIFEXITED(status));
  if (WEXITSTATUS(status) == 77) GTEST_SKIP() << "CLONE_NEWPID requires permission";
  EXPECT_EQ(0, WEXITSTATUS(status));
}

}  // namespace

int main(int argc, char** argv) {
  testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
