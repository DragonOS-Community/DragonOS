#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <cstdint>
#include <functional>
#include <thread>

namespace {

uint64_t Now(clockid_t id) {
  timespec ts{};
  if (clock_gettime(id, &ts) != 0) return 0;
  return uint64_t(ts.tv_sec) * 1000000000ULL + ts.tv_nsec;
}

uint64_t Ns(timeval value) {
  return uint64_t(value.tv_sec) * 1000000000ULL + value.tv_usec * 1000ULL;
}

uint64_t Total(const rusage& usage) {
  return Ns(usage.ru_utime) + Ns(usage.ru_stime);
}

// Use CPU time, not elapsed time, but never let a broken clock hang CI.
bool SpinCpu(uint64_t amount) {
  const uint64_t start = Now(CLOCK_THREAD_CPUTIME_ID);
  const uint64_t wall_end = Now(CLOCK_MONOTONIC) + 2000000000ULL;
  volatile unsigned long work = 1;
  while (Now(CLOCK_THREAD_CPUTIME_ID) - start < amount) {
    for (unsigned i = 0; i < 100; ++i) work = work * 33 + i;
    if (Now(CLOCK_MONOTONIC) >= wall_end) return false;
  }
  return true;
}

// Every case runs in a fresh process: CHILDREN starts empty and a failed
// thread join/wait cannot block the runner. No in-guest alarm is required.
void Isolated(const std::function<int()>& body) {
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) _exit(body());
  const uint64_t end = Now(CLOCK_MONOTONIC) + 10000000000ULL;
  int status = 0;
  while (Now(CLOCK_MONOTONIC) < end) {
    const pid_t result = waitpid(child, &status, WNOHANG);
    if (result == child) {
      ASSERT_TRUE(WIFEXITED(status));
      EXPECT_EQ(0, WEXITSTATUS(status));
      return;
    }
    ASSERT_TRUE(result == 0 || (result < 0 && errno == EINTR));
    usleep(1000);
  }
  kill(child, SIGKILL);
  // Cleanup is also bounded if a failed kernel wait cannot reap the child.
  const uint64_t cleanup_end = Now(CLOCK_MONOTONIC) + 2000000000ULL;
  while (Now(CLOCK_MONOTONIC) < cleanup_end) {
    const pid_t result = waitpid(child, &status, WNOHANG);
    if (result == child || (result < 0 && errno == ECHILD)) break;
    usleep(1000);
  }
  FAIL() << "CPU accounting case exceeded its wall-clock watchdog";
}

TEST(RusageCpuRuntime, SubTickThreadAndProcessUsageMatchesRuntime) {
  Isolated([]() {
    for (int who : {RUSAGE_THREAD, RUSAGE_SELF}) {
      const clockid_t clock = who == RUSAGE_THREAD ? CLOCK_THREAD_CPUTIME_ID
                                                 : CLOCK_PROCESS_CPUTIME_ID;
      for (unsigned i = 0; i < 40; ++i) {
        if (!SpinCpu(300000)) return 1;
        const uint64_t before = Now(clock);
        rusage usage{};
        if (getrusage(who, &usage) != 0) return 2;
        const uint64_t after = Now(clock);
        // Two timeval fields lose less than 2us in total. An additional
        // 20us accommodates clock/runtime sampling inside the syscalls,
        // not a tick-sized accounting discrepancy.
        if (Total(usage) + 22000 < before || Total(usage) > after + 20000)
          return 3;
      }
    }
    return 0;
  });
}

TEST(RusageCpuRuntime, UserAndSystemComponentsNeverMoveBackwards) {
  Isolated([]() {
    for (int who : {RUSAGE_THREAD, RUSAGE_SELF}) {
      rusage previous{};
      if (getrusage(who, &previous) != 0) return 1;
      for (unsigned i = 0; i < 80; ++i) {
        if (!SpinCpu(200000)) return 2;
        // Alternate userspace work with actual syscalls, changing the raw
        // user/system classification ratio across scheduler ticks.
        for (unsigned j = 0; j < 100; ++j) syscall(SYS_getpid);
        rusage current{};
        if (getrusage(who, &current) != 0) return 3;
        if (Ns(current.ru_utime) < Ns(previous.ru_utime) ||
            Ns(current.ru_stime) < Ns(previous.ru_stime)) return 4;
        previous = current;
      }
    }
    return 0;
  });
}

TEST(RusageCpuRuntime, ExitedThreadHistoryIsRetainedExactlyOnce) {
  Isolated([]() {
    uint64_t worker_runtime = 0;
    bool worker_ok = false;
    const uint64_t start = Now(CLOCK_PROCESS_CPUTIME_ID);
    std::thread worker([&]() {
      const uint64_t thread_start = Now(CLOCK_THREAD_CPUTIME_ID);
      worker_ok = SpinCpu(40000000);
      worker_runtime = Now(CLOCK_THREAD_CPUTIME_ID) - thread_start;
    });
    worker.join();
    if (!worker_ok || worker_runtime < 40000000) return 1;
    const uint64_t before = Now(CLOCK_PROCESS_CPUTIME_ID);
    rusage usage{};
    if (getrusage(RUSAGE_SELF, &usage) != 0) return 2;
    const uint64_t after = Now(CLOCK_PROCESS_CPUTIME_ID);
    if (before - start < worker_runtime || Total(usage) + 22000 < before ||
        Total(usage) > after + 20000) return 3;
    // Repeated post-exit reads must not re-add the retired thread.
    for (unsigned i = 0; i < 20; ++i) {
      const uint64_t lower = Now(CLOCK_PROCESS_CPUTIME_ID);
      rusage current{};
      if (getrusage(RUSAGE_SELF, &current) != 0) return 4;
      const uint64_t upper = Now(CLOCK_PROCESS_CPUTIME_ID);
      if (Total(current) < Total(usage) || Total(current) + 22000 < lower ||
          Total(current) > upper + 20000) return 5;
      usage = current;
    }
    return 0;
  });
}

TEST(RusageCpuRuntime, WaitUsageAndChildrenAccumulateOnlyOnReap) {
  Isolated([]() {
    for (bool use_waitid : {false, true}) {
      rusage initial{};
      if (getrusage(RUSAGE_CHILDREN, &initial) != 0) return 1;
      int report[2];
      if (pipe(report) != 0) return 2;
      const pid_t child = fork();
      if (child < 0) return 3;
      if (child == 0) {
        close(report[0]);
        if (!SpinCpu(30000000)) _exit(100);
        const uint64_t runtime = Now(CLOCK_PROCESS_CPUTIME_ID);
        _exit(write(report[1], &runtime, sizeof(runtime)) == sizeof(runtime)
                  ? 0 : 101);
      }
      close(report[1]);
      uint64_t recorded = 0;
      const ssize_t count = read(report[0], &recorded, sizeof(recorded));
      close(report[0]);
      if (count != sizeof(recorded)) return 4;
      siginfo_t information{};
      rusage observed{};
      // Linux's raw waitid syscall has a fifth rusage argument, unlike libc.
      if (syscall(SYS_waitid, P_PID, child, &information,
                  WEXITED | WNOWAIT, &observed) != 0) return 5;
      if (information.si_pid != child || information.si_code != CLD_EXITED ||
          information.si_status != 0 || Total(observed) + 2000 < recorded)
        return 6;
      rusage before_reap{};
      if (getrusage(RUSAGE_CHILDREN, &before_reap) != 0 ||
          Total(before_reap) != Total(initial)) return 7;
      rusage reaped{};
      if (use_waitid) {
        if (syscall(SYS_waitid, P_PID, child, &information, WEXITED,
                    &reaped) != 0) return 8;
      } else {
        int status = 0;
        if (wait4(child, &status, 0, &reaped) != child ||
            !WIFEXITED(status) || WEXITSTATUS(status) != 0) return 9;
      }
      if (Total(reaped) + 2000 < recorded) return 10;
      rusage after_reap{};
      if (getrusage(RUSAGE_CHILDREN, &after_reap) != 0) return 11;
      const uint64_t delta = Total(after_reap) - Total(initial);
      // Each user/system conversion truncates independently, including the
      // cumulative CHILDREN snapshot: at most 4us difference, never a tick.
      if (delta + 4000 < Total(reaped) || delta > Total(reaped) + 4000)
        return 12;
      errno = 0;
      if (syscall(SYS_waitid, P_PID, child, &information, WEXITED | WNOHANG,
                  &observed) != -1 || errno != ECHILD) return 13;
      rusage repeated{};
      if (getrusage(RUSAGE_CHILDREN, &repeated) != 0 ||
          Total(repeated) != Total(after_reap)) return 14;
    }
    return 0;
  });
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
