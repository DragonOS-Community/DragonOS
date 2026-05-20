#include <errno.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include "gtest/gtest.h"

namespace {

uint64_t RusageCpuUsec(const struct rusage& ru) {
  return static_cast<uint64_t>(ru.ru_utime.tv_sec) * 1000000ULL +
         static_cast<uint64_t>(ru.ru_utime.tv_usec) +
         static_cast<uint64_t>(ru.ru_stime.tv_sec) * 1000000ULL +
         static_cast<uint64_t>(ru.ru_stime.tv_usec);
}

uint64_t MonotonicUsec() {
  struct timespec ts;
  if (clock_gettime(CLOCK_MONOTONIC, &ts) != 0) {
    return 0;
  }
  return static_cast<uint64_t>(ts.tv_sec) * 1000000ULL +
         static_cast<uint64_t>(ts.tv_nsec) / 1000ULL;
}

uint64_t BurnCpuForUsec(uint64_t usec) {
  const uint64_t start = MonotonicUsec();
  volatile uint64_t sink = 0;
  while (MonotonicUsec() - start < usec) {
    for (int i = 0; i < 10000; ++i) {
      sink += static_cast<uint64_t>(i);
    }
  }
  return sink;
}

void BusyForUsec(uint64_t usec) {
  uint64_t sink = BurnCpuForUsec(usec);
  _exit(static_cast<int>(sink & 0));
}

void* ThreadBurn(void* arg) {
  BurnCpuForUsec(reinterpret_cast<uintptr_t>(arg));
  return nullptr;
}

}  // namespace

TEST(WaitRusage, WNowaitDoesNotReapAndWait4AccountsChildUsage) {
  struct rusage before {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &before)) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    BusyForUsec(500000);
  }

  siginfo_t si {};
  struct rusage nowait_usage {};
  ASSERT_EQ(0, syscall(SYS_waitid, P_PID, child, &si, WEXITED | WNOWAIT,
                       &nowait_usage))
      << strerror(errno);
  EXPECT_EQ(SIGCHLD, si.si_signo);
  EXPECT_EQ(CLD_EXITED, si.si_code);
  EXPECT_EQ(0, si.si_status);
  EXPECT_EQ(child, si.si_pid);
  EXPECT_GT(RusageCpuUsec(nowait_usage), 0u);

  struct rusage after_nowait {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &after_nowait)) << strerror(errno);
  EXPECT_EQ(RusageCpuUsec(before), RusageCpuUsec(after_nowait));

  int status = 0;
  struct rusage waited_usage {};
  ASSERT_EQ(child, wait4(child, &status, 0, &waited_usage)) << strerror(errno);
  EXPECT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
  EXPECT_GT(RusageCpuUsec(waited_usage), 0u);

  struct rusage after_wait {};
  ASSERT_EQ(0, getrusage(RUSAGE_CHILDREN, &after_wait)) << strerror(errno);
  EXPECT_GE(RusageCpuUsec(after_wait) - RusageCpuUsec(before),
            RusageCpuUsec(waited_usage));

  errno = 0;
  EXPECT_EQ(-1, wait4(child, nullptr, WNOHANG, nullptr));
  EXPECT_EQ(ECHILD, errno);
}

TEST(WaitRusage, Wait4IncludesExitedThreadCpuTime) {
  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    pthread_t worker {};
    if (pthread_create(&worker, nullptr, ThreadBurn,
                       reinterpret_cast<void*>(static_cast<uintptr_t>(600000))) != 0) {
      _exit(2);
    }
    if (pthread_join(worker, nullptr) != 0) {
      _exit(3);
    }
    _exit(0);
  }

  int status = 0;
  struct rusage usage {};
  ASSERT_EQ(child, wait4(child, &status, 0, &usage)) << strerror(errno);
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  EXPECT_GE(RusageCpuUsec(usage), 100000u);
}

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
