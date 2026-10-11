#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <cstdio>
#include <atomic>
#include <cstdlib>
#include <sstream>
#include <string>
#include <thread>
#include <vector>

namespace {

int Write(const std::string& path, const std::string& value) {
  int fd = open(path.c_str(), O_WRONLY | O_CLOEXEC);
  if (fd < 0) return errno;
  ssize_t count;
  do {
    count = write(fd, value.data(), value.size());
  } while (count < 0 && errno == EINTR);
  int error = count < 0 ? errno :
      (count == static_cast<ssize_t>(value.size()) ? 0 : EIO);
  close(fd);
  return error;
}

std::string Read(const std::string& path) {
  int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) return "";
  std::string result;
  char bytes[4096];
  for (;;) {
    ssize_t count = read(fd, bytes, sizeof(bytes));
    if (count < 0 && errno == EINTR) continue;
    if (count <= 0) break;
    result.append(bytes, count);
  }
  close(fd);
  return result;
}

uint64_t Clock(clockid_t id) {
  timespec value{};
  if (clock_gettime(id, &value) != 0) return 0;
  return uint64_t(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}

uint64_t Stat(const std::string& text, const std::string& key) {
  std::istringstream stream(text);
  std::string name;
  uint64_t value;
  while (stream >> name >> value) {
    if (name == key) return value;
  }
  return UINT64_MAX;
}

struct Sample {
  uint64_t cpu_ns;
  uint64_t wall_ns;
};

// A parent-controlled worker, with no unbounded wait in either normal cleanup
// or an assertion failure. Its own safety deadline is independent of CPU time.
class BusyWorker {
 public:
  BusyWorker(const std::string& group, unsigned workers, int cpu) {
    int gate[2];
    if (pipe(gate) != 0) {
      ADD_FAILURE() << strerror(errno);
      return;
    }
    pid_ = fork();
    if (pid_ == 0) {
      close(gate[1]);
      cpu_set_t mask;
      CPU_ZERO(&mask);
      CPU_SET(cpu, &mask);
      char token;
      ssize_t count;
      do { count = read(gate[0], &token, 1); } while (count < 0 && errno == EINTR);
      close(gate[0]);
      if (count != 1) _exit(3);
      if (sched_setaffinity(0, sizeof(mask), &mask) != 0) _exit(2);
      const uint64_t end = Clock(CLOCK_MONOTONIC) + 10000000000ULL;
      auto busy = [end]() {
        volatile unsigned long work = 0;
        do {
          for (unsigned i = 0; i < 10000; ++i) work = work * 33 + i;
        } while (Clock(CLOCK_MONOTONIC) < end);
      };
      std::vector<std::thread> threads;
      for (unsigned i = 1; i < workers; ++i) threads.emplace_back(busy);
      busy();
      for (auto& thread : threads) thread.join();
      _exit(0);
    }
    close(gate[0]);
    if (pid_ < 0) {
      ADD_FAILURE() << strerror(errno);
    } else {
      int migration = Write(group + "/cgroup.procs", std::to_string(pid_));
      EXPECT_EQ(0, migration);
      if (migration == 0) {
        EXPECT_EQ(1, write(gate[1], "R", 1));
      }
    }
    close(gate[1]);
  }

  ~BusyWorker() { Stop(); }
  BusyWorker(const BusyWorker&) = delete;
  BusyWorker& operator=(const BusyWorker&) = delete;
  pid_t pid() const { return pid_; }

  bool Pause() {
    if (pid_ <= 0 || kill(pid_, SIGSTOP) != 0) return false;
    const uint64_t deadline = Clock(CLOCK_MONOTONIC) + 1000000000ULL;
    do {
      int status;
      pid_t result = waitpid(pid_, &status, WUNTRACED | WNOHANG);
      if (result == pid_) return WIFSTOPPED(status);
      if (result < 0 && errno != EINTR) return false;
      usleep(1000);
    } while (Clock(CLOCK_MONOTONIC) < deadline);
    return false;
  }

  bool Resume() { return pid_ > 0 && kill(pid_, SIGCONT) == 0; }

  void Stop() {
    if (pid_ <= 0) return;
    kill(pid_, SIGKILL);
    const uint64_t deadline = Clock(CLOCK_MONOTONIC) + 2000000000ULL;
    do {
      int status;
      pid_t result = waitpid(pid_, &status, WNOHANG);
      if (result == pid_ || (result < 0 && errno == ECHILD)) {
        pid_ = -1;
        return;
      }
      usleep(1000);
    } while (Clock(CLOCK_MONOTONIC) < deadline);
    ADD_FAILURE() << "CPU worker " << pid_ << " was not reaped within 2 seconds";
  }

 private:
  pid_t pid_ = -1;
};

class CgroupCpuTest : public ::testing::Test {
 protected:
  void SetUp() override {
    const char* configured = std::getenv("DUNITEST_CPU_PARENT");
    parent_ = configured ? configured : "/sys/fs/cgroup";
    std::istringstream controllers(Read(parent_ + "/cgroup.subtree_control"));
    std::string token;
    bool enabled = false;
    while (controllers >> token) enabled |= token == "cpu";
    if (!enabled) {
      ASSERT_EQ(0, Write(parent_ + "/cgroup.subtree_control", "+cpu"));
      added_cpu_ = true;
    }
    static unsigned sequence;
    node_ = parent_ + "/dunitest-cpu-" + std::to_string(getpid()) + "-" +
            std::to_string(sequence++);
    ASSERT_EQ(0, mkdir(node_.c_str(), 0755)) << strerror(errno);
    created_ = true;
  }

  void TearDown() override {
    if (child_ > 0) {
      kill(child_, SIGKILL);
      int status;
      EXPECT_TRUE(ReapChild(&status));
    }
    if (created_) {
      for (auto it = children_.rbegin(); it != children_.rend(); ++it) {
        EXPECT_EQ(0, rmdir(it->c_str())) << *it << ": " << strerror(errno);
      }
      EXPECT_EQ(0, rmdir(node_.c_str())) << strerror(errno);
    }
    if (added_cpu_) {
      EXPECT_EQ(0, Write(parent_ + "/cgroup.subtree_control", "-cpu"));
    }
  }

  std::string NewChild(const std::string& name) {
    std::string child = node_ + "/" + name;
    if (mkdir(child.c_str(), 0755) != 0) {
      ADD_FAILURE() << child << ": " << strerror(errno);
      return "";
    }
    children_.push_back(child);
    return child;
  }

  int OneCpu() {
    cpu_set_t mask;
    CPU_ZERO(&mask);
    if (sched_getaffinity(0, sizeof(mask), &mask) != 0) {
      ADD_FAILURE() << strerror(errno);
      return -1;
    }
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
      if (CPU_ISSET(cpu, &mask)) return cpu;
    }
    ADD_FAILURE() << "test process has no allowed CPU";
    return -1;
  }

  uint64_t Usage(const std::string& group) {
    uint64_t value = Stat(Read(group + "/cpu.stat"), "usage_usec");
    EXPECT_NE(UINT64_MAX, value) << group;
    return value;
  }

  bool ReapChild(int* status) {
    const uint64_t deadline = Clock(CLOCK_MONOTONIC) + 2000000000ULL;
    do {
      pid_t result = waitpid(child_, status, WNOHANG);
      if (result == child_) {
        child_ = -1;
        return true;
      }
      if (result < 0 && errno != EINTR) return false;
      usleep(1000);
    } while (Clock(CLOCK_MONOTONIC) < deadline);
    return false;
  }

  // Block the child until migration has committed; the parent stays outside
  // the measured group so it can enforce an independent wall-clock deadline.
  Sample Run(unsigned duration_ms, unsigned workers = 1,
             const std::string& group = "") {
    int start[2], result[2];
    if (pipe(start) != 0) {
      ADD_FAILURE() << strerror(errno);
      return {};
    }
    if (pipe(result) != 0) {
      close(start[0]); close(start[1]);
      ADD_FAILURE() << strerror(errno);
      return {};
    }
    child_ = fork();
    if (child_ == 0) {
      close(start[1]); close(result[0]);
      char ready;
      ssize_t count;
      do { count = read(start[0], &ready, 1); } while (count < 0 && errno == EINTR);
      if (count != 1) _exit(2);
      close(start[0]);
      uint64_t wall = Clock(CLOCK_MONOTONIC);
      uint64_t cpu = Clock(CLOCK_PROCESS_CPUTIME_ID);
      uint64_t end = wall + uint64_t(duration_ms) * 1000000;
      auto work_until_end = [end]() {
        volatile unsigned long work = 0;
        do {
          for (unsigned i = 0; i < 10000; ++i) work = work * 33 + i;
        } while (Clock(CLOCK_MONOTONIC) < end);
      };
      std::vector<std::thread> threads;
      for (unsigned i = 1; i < workers; ++i) threads.emplace_back(work_until_end);
      work_until_end();
      for (auto& thread : threads) thread.join();
      Sample sample{Clock(CLOCK_PROCESS_CPUTIME_ID) - cpu,
                    Clock(CLOCK_MONOTONIC) - wall};
      count = write(result[1], &sample, sizeof(sample));
      _exit(count == sizeof(sample) ? 0 : 3);
    }
    close(start[0]); close(result[1]);
    if (child_ < 0) {
      close(start[1]); close(result[0]);
      ADD_FAILURE() << strerror(errno);
      return {};
    }
    int migration = Write((group.empty() ? node_ : group) + "/cgroup.procs",
                          std::to_string(child_));
    EXPECT_EQ(0, migration);
    if (migration == 0) {
      EXPECT_EQ(1, write(start[1], "R", 1));
    }
    close(start[1]);
    pollfd pfd{result[0], POLLIN, 0};
    int ready;
    do { ready = poll(&pfd, 1, 10000); } while (ready < 0 && errno == EINTR);
    Sample sample{};
    if (ready > 0) {
      EXPECT_EQ(static_cast<ssize_t>(sizeof(sample)), read(result[0], &sample, sizeof(sample)));
    } else {
      ADD_FAILURE() << "CPU worker did not finish within 10 seconds";
      kill(child_, SIGKILL);
    }
    close(result[0]);
    int status = 0;
    EXPECT_TRUE(ReapChild(&status)) << "CPU worker exit was not observed within 2 seconds";
    EXPECT_TRUE(WIFEXITED(status));
    if (WIFEXITED(status)) {
      EXPECT_EQ(0, WEXITSTATUS(status));
    }
    std::printf("CPU_SAMPLE cpu_ns=%llu wall_ns=%llu\n",
                static_cast<unsigned long long>(sample.cpu_ns),
                static_cast<unsigned long long>(sample.wall_ns));
    return sample;
  }

  std::string parent_, node_;
  std::vector<std::string> children_;
  pid_t child_ = -1;
  bool created_ = false;
  bool added_cpu_ = false;
};

TEST_F(CgroupCpuTest, ActualRuntimeIsAccounted) {
  Sample sample = Run(1000);
  ASSERT_GT(sample.cpu_ns, 0U);
  std::string stats = Read(node_ + "/cpu.stat");
  std::printf("CPU_STAT\n%s", stats.c_str());
  uint64_t usage = Stat(stats, "usage_usec");
  ASSERT_NE(UINT64_MAX, usage);
  EXPECT_GT(usage, 10000U);
  EXPECT_LE(usage, sample.wall_ns / 1000 + 100000);
  uint64_t user = Stat(stats, "user_usec"), system = Stat(stats, "system_usec");
  ASSERT_NE(UINT64_MAX, user);
  ASSERT_NE(UINT64_MAX, system);
  EXPECT_LE(user + system, usage);
  EXPECT_LE(usage - (user + system), 1U);
}

TEST_F(CgroupCpuTest, QuarterCpuQuotaRestrictsRealRuntime) {
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "25000 100000"));
  Sample sample = Run(2000);
  ASSERT_GE(sample.wall_ns, 2000000000ULL);
  // Startup, period alignment and tick granularity need tolerance, but an
  // unthrottled full CPU must not pass a configured quarter-CPU limit.
  EXPECT_LT(sample.cpu_ns, sample.wall_ns * 45 / 100);
  std::string stats = Read(node_ + "/cpu.stat");
  std::printf("CPU_STAT\n%s", stats.c_str());
  uint64_t throttled = Stat(stats, "nr_throttled");
  ASSERT_NE(UINT64_MAX, throttled);
  EXPECT_GT(throttled, 0U);
}

TEST_F(CgroupCpuTest, SmallPeriodQuotaUsesPreciseEvents) {
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "1000 2000"));
  Sample sample = Run(1200);
  ASSERT_GE(sample.wall_ns, 1200000000ULL);
  EXPECT_GT(sample.cpu_ns, sample.wall_ns * 20 / 100);
  EXPECT_LT(sample.cpu_ns, sample.wall_ns * 75 / 100);
  std::string stats = Read(node_ + "/cpu.stat");
  std::printf("CPU_STAT\n%s", stats.c_str());
  EXPECT_GT(Stat(stats, "nr_throttled"), 0U);
}

TEST_F(CgroupCpuTest, SmallPeriodWorkerWakesProcessCpuTimeSleeper) {
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "1000 2000"));
  int gate[2], result[2];
  ASSERT_EQ(0, pipe(gate));
  if (pipe(result) != 0) {
    close(gate[0]); close(gate[1]);
    FAIL() << strerror(errno);
  }
  struct SleepResult {
    int relative_error;
    int absolute_error;
    uint64_t relative_elapsed;
    uint64_t absolute_now;
    uint64_t absolute_target;
  };
  child_ = fork();
  if (child_ == 0) {
    close(gate[1]); close(result[0]);
    char token;
    if (read(gate[0], &token, 1) != 1) _exit(2);
    close(gate[0]);
    std::atomic<bool> stop{false};
    const uint64_t deadline = Clock(CLOCK_MONOTONIC) + 5000000000ULL;
    std::thread worker([&]() {
      volatile unsigned long work = 1;
      while (!stop.load() && Clock(CLOCK_MONOTONIC) < deadline) {
        for (unsigned i = 0; i < 10000; ++i) work = work * 33 + i;
      }
    });
    SleepResult observed{};
    timespec request{0, 30000000};
    uint64_t before = Clock(CLOCK_PROCESS_CPUTIME_ID);
    observed.relative_error = clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, 0,
                                              &request, nullptr);
    observed.relative_elapsed = Clock(CLOCK_PROCESS_CPUTIME_ID) - before;
    observed.absolute_target = Clock(CLOCK_PROCESS_CPUTIME_ID) + 30000000ULL;
    request.tv_sec = observed.absolute_target / 1000000000ULL;
    request.tv_nsec = observed.absolute_target % 1000000000ULL;
    observed.absolute_error = clock_nanosleep(CLOCK_PROCESS_CPUTIME_ID, TIMER_ABSTIME,
                                              &request, nullptr);
    observed.absolute_now = Clock(CLOCK_PROCESS_CPUTIME_ID);
    stop.store(true);
    worker.join();
    const ssize_t count = write(result[1], &observed, sizeof(observed));
    _exit(count == sizeof(observed) ? 0 : 3);
  }
  close(gate[0]); close(result[1]);
  if (child_ < 0) {
    close(gate[1]); close(result[0]);
    FAIL() << strerror(errno);
  }
  int migration = Write(node_ + "/cgroup.procs", std::to_string(child_));
  EXPECT_EQ(0, migration);
  if (migration == 0) {
    EXPECT_EQ(1, write(gate[1], "R", 1));
  }
  close(gate[1]);
  // The watchdog is in the unlimited parent, not the measured CPU clock domain.
  pollfd descriptor{result[0], POLLIN, 0};
  int ready;
  do { ready = poll(&descriptor, 1, 5000); } while (ready < 0 && errno == EINTR);
  SleepResult observed{};
  ssize_t count = ready > 0 ? read(result[0], &observed, sizeof(observed)) : -1;
  close(result[0]);
  if (count != sizeof(observed)) kill(child_, SIGKILL);
  int status = 0;
  ASSERT_TRUE(ReapChild(&status));
  ASSERT_EQ(static_cast<ssize_t>(sizeof(observed)), count)
      << "process CPU-time waiter did not complete within 5 seconds";
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  EXPECT_EQ(0, observed.relative_error);
  EXPECT_EQ(0, observed.absolute_error);
  EXPECT_GE(observed.relative_elapsed, 30000000ULL);
  EXPECT_GE(observed.absolute_now, observed.absolute_target);
}

TEST_F(CgroupCpuTest, MultipleWorkersShareOneQuota) {
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "25000 100000"));
  Sample sample = Run(1500, 2);
  EXPECT_LT(sample.cpu_ns, sample.wall_ns * 45 / 100);
  EXPECT_GT(sample.cpu_ns, sample.wall_ns / 20);
}

TEST_F(CgroupCpuTest, AncestorQuotaConstrainsLargerChildQuota) {
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  std::string leaf = NewChild("child");
  ASSERT_FALSE(leaf.empty());
  EXPECT_EQ(0, Write(node_ + "/cpu.max", "25000 100000"));
  EXPECT_EQ(0, Write(leaf + "/cpu.max", "100000 100000"));
  Sample sample = Run(1500, 1, leaf);
  EXPECT_LT(sample.cpu_ns, sample.wall_ns * 45 / 100);
  EXPECT_GT(Stat(Read(leaf + "/cpu.stat.local"), "throttled_usec"), 0U);
  int removed = rmdir(leaf.c_str());
  EXPECT_EQ(0, removed);
  if (removed == 0) children_.pop_back();
}

TEST_F(CgroupCpuTest, ConfigurationBoundsAndIdleWeightAliases) {
  EXPECT_EQ(EINVAL, Write(node_ + "/cpu.max", "0 100000"));
  EXPECT_EQ(EINVAL, Write(node_ + "/cpu.max", "999 100000"));
  EXPECT_EQ(EINVAL, Write(node_ + "/cpu.max", "1000 999"));
  EXPECT_EQ(EINVAL, Write(node_ + "/cpu.max", "1000 1000001"));
  EXPECT_EQ(ERANGE, Write(node_ + "/cpu.weight", "0"));
  EXPECT_EQ(ERANGE, Write(node_ + "/cpu.weight.nice", "20"));
  ASSERT_EQ(0, Write(node_ + "/cpu.weight.nice", "5"));
  EXPECT_EQ("5\n", Read(node_ + "/cpu.weight.nice"));
  EXPECT_NE("100\n", Read(node_ + "/cpu.weight"));
  ASSERT_EQ(0, Write(node_ + "/cpu.idle", "1"));
  EXPECT_EQ(EINVAL, Write(node_ + "/cpu.weight", "200"));
  EXPECT_EQ(EINVAL, Write(node_ + "/cpu.weight.nice", "0"));
  ASSERT_EQ(0, Write(node_ + "/cpu.idle", "0"));
  EXPECT_EQ("100\n", Read(node_ + "/cpu.weight"));
  EXPECT_EQ("0\n", Read(node_ + "/cpu.weight.nice"));
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "1000 2000"));
  EXPECT_EQ(EINVAL, Write(node_ + "/cpu.max.burst", "1001"));
  EXPECT_EQ(0, Write(node_ + "/cpu.max.burst", "1000"));
  EXPECT_EQ("1000\n", Read(node_ + "/cpu.max.burst"));
  EXPECT_EQ(0, Write(node_ + "/cpu.max", "max"));
  EXPECT_EQ("max 2000\n", Read(node_ + "/cpu.max"));
}

TEST_F(CgroupCpuTest, SiblingWeightsApplyToGroupsNotThreadCounts) {
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  const std::string low = NewChild("weight-low"), high = NewChild("weight-high");
  ASSERT_FALSE(low.empty()); ASSERT_FALSE(high.empty());
  ASSERT_EQ(0, Write(low + "/cpu.weight", "100"));
  ASSERT_EQ(0, Write(high + "/cpu.weight", "300"));
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker one(low, 1, cpu), many(high, 3, cpu);
  ASSERT_GT(one.pid(), 0); ASSERT_GT(many.pid(), 0);
  usleep(250000);
  uint64_t low_start = Usage(low), high_start = Usage(high);
  usleep(1200000);
  uint64_t low_time = Usage(low) - low_start;
  uint64_t high_time = Usage(high) - high_start;
  std::printf("GROUP_WEIGHTS one_thread_usec=%llu three_threads_usec=%llu\n",
              static_cast<unsigned long long>(low_time),
              static_cast<unsigned long long>(high_time));
  ASSERT_GT(low_time, 100000U);
  // The high group gets approximately 3x, not 9x from its three tasks.
  EXPECT_GT(high_time, low_time * 2);
  EXPECT_LT(high_time, low_time * 5);
}

TEST_F(CgroupCpuTest, RemovingQuotaRestoresRunningGroupImmediately) {
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "25000 100000"));
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker worker(node_, 1, cpu); ASSERT_GT(worker.pid(), 0);
  usleep(150000);
  uint64_t start = Usage(node_);
  usleep(400000);
  uint64_t limited = Usage(node_) - start;
  EXPECT_GT(limited, 20000U);
  EXPECT_LT(limited, 220000U);
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "max"));
  start = Usage(node_);
  usleep(400000);
  uint64_t unlimited = Usage(node_) - start;
  EXPECT_GT(unlimited, 220000U);
  EXPECT_GT(unlimited, limited * 2);
}

TEST_F(CgroupCpuTest, IdleGroupYieldsButRunsWithoutCompetition) {
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  const std::string idle = NewChild("idle"), normal = NewChild("normal");
  ASSERT_FALSE(idle.empty()); ASSERT_FALSE(normal.empty());
  ASSERT_EQ(0, Write(idle + "/cpu.idle", "1"));
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker idle_worker(idle, 1, cpu), normal_worker(normal, 1, cpu);
  usleep(200000);
  uint64_t idle_start = Usage(idle), normal_start = Usage(normal);
  usleep(600000);
  uint64_t idle_time = Usage(idle) - idle_start;
  uint64_t normal_time = Usage(normal) - normal_start;
  ASSERT_GT(normal_time, 250000U);
  EXPECT_LT(idle_time, normal_time / 5);
  normal_worker.Stop();
  idle_start = Usage(idle);
  usleep(300000);
  EXPECT_GT(Usage(idle) - idle_start, 150000U);
}

TEST_F(CgroupCpuTest, DisableAndReenableRebindExistingRunnableTasks) {
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  const std::string leaf = NewChild("rebind"); ASSERT_FALSE(leaf.empty());
  ASSERT_EQ(0, Write(leaf + "/cpu.max", "25000 100000"));
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker worker(leaf, 1, cpu); ASSERT_GT(worker.pid(), 0);
  usleep(150000);
  uint64_t initial = Usage(leaf);
  usleep(400000);
  EXPECT_LT(Usage(leaf) - initial, 220000U);
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "-cpu"));
  EXPECT_EQ(UINT64_MAX, Stat(Read(leaf + "/cpu.stat"), "nr_periods"));
  EXPECT_TRUE(Read(leaf + "/cpu.stat.local").empty());
  initial = Usage(leaf);
  usleep(400000);
  EXPECT_GT(Usage(leaf) - initial, 220000U);
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  EXPECT_EQ("max 100000\n", Read(leaf + "/cpu.max"));
  ASSERT_EQ(0, Write(leaf + "/cpu.max", "25000 100000"));
  initial = Usage(leaf);
  usleep(400000);
  EXPECT_LT(Usage(leaf) - initial, 220000U);
  EXPECT_GT(Stat(Read(leaf + "/cpu.stat"), "nr_throttled"), 0U);
}

TEST_F(CgroupCpuTest, DisabledChildAndRootKeepBasicAccountingAfterRemoval) {
  const std::string leaf = NewChild("accounting-only"); ASSERT_FALSE(leaf.empty());
  EXPECT_EQ(-1, access((leaf + "/cpu.max").c_str(), F_OK));
  uint64_t ancestor_before = Usage(node_), root_before = Usage(parent_);
  Sample sample = Run(400, 1, leaf);
  ASSERT_GT(sample.wall_ns, 0U);
  uint64_t leaf_usage = Usage(leaf), ancestor_after = Usage(node_);
  EXPECT_GT(leaf_usage, 50000U);
  EXPECT_GE(ancestor_after - ancestor_before, leaf_usage);
  EXPECT_GT(Usage(parent_), root_before);
  EXPECT_EQ(UINT64_MAX, Stat(Read(leaf + "/cpu.stat"), "nr_periods"));
  ASSERT_EQ(0, rmdir(leaf.c_str()));
  children_.pop_back();
  EXPECT_GE(Usage(node_), ancestor_after);
}

TEST_F(CgroupCpuTest, DisabledAndMigratedSleepingTaskUsesNewCpuCssOnWake) {
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  const std::string source = NewChild("sleep-source"), target = NewChild("sleep-target");
  ASSERT_FALSE(source.empty()); ASSERT_FALSE(target.empty());
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker worker(source, 1, cpu); ASSERT_GT(worker.pid(), 0);
  usleep(100000);
  ASSERT_TRUE(worker.Pause());
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "-cpu"));
  ASSERT_EQ(0, Write(target + "/cgroup.procs", std::to_string(worker.pid())));
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  ASSERT_EQ(0, Write(target + "/cpu.max", "25000 100000"));
  uint64_t source_before = Usage(source), target_before = Usage(target);
  ASSERT_TRUE(worker.Resume());
  usleep(600000);
  uint64_t target_runtime = Usage(target) - target_before;
  EXPECT_GT(target_runtime, 30000U);
  EXPECT_LT(target_runtime, 310000U);
  EXPECT_EQ(source_before, Usage(source));
  EXPECT_GT(Stat(Read(target + "/cpu.stat"), "nr_throttled"), 0U);
}

TEST_F(CgroupCpuTest, ThreadedLeafQuotaConstrainsItsSelectedThread) {
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  const std::string leaf = NewChild("threaded-quota"); ASSERT_FALSE(leaf.empty());
  ASSERT_EQ(0, Write(leaf + "/cgroup.type", "threaded"));
  ASSERT_EQ(0, Write(leaf + "/cpu.max", "25000 100000"));
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker worker(node_, 1, cpu); ASSERT_GT(worker.pid(), 0);
  ASSERT_EQ(0, Write(leaf + "/cgroup.threads", std::to_string(worker.pid())));
  usleep(150000);
  uint64_t initial = Usage(leaf);
  usleep(600000);
  uint64_t runtime = Usage(leaf) - initial;
  EXPECT_GT(runtime, 30000U);
  EXPECT_LT(runtime, 310000U);
  EXPECT_GT(Stat(Read(leaf + "/cpu.stat"), "nr_throttled"), 0U);
  ASSERT_EQ(0, Write(node_ + "/cgroup.threads", std::to_string(worker.pid())));
  initial = Usage(node_);
  usleep(400000);
  EXPECT_GT(Usage(node_) - initial, 220000U);
}

TEST_F(CgroupCpuTest, BurstAccumulatesIdleCreditAndRecordsActualConsumption) {
  // Linux refills once per period, caps the bank at quota+burst, and reports
  // burst consumption at the following refill, not at the moment of execution.
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "50000 200000"));
  ASSERT_EQ(0, Write(node_ + "/cpu.max.burst", "50000"));
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker worker(node_, 1, cpu); ASSERT_GT(worker.pid(), 0);
  usleep(20000);
  ASSERT_TRUE(worker.Pause());
  for (unsigned round = 0; round < 2; ++round) {
    // Cover the idle timer shutdown interval to build a full credit bank.
    usleep(700000);
    // Restart the production period event with a short runnable pulse, then
    // observe its next refill while stopped. The actual burst sample starts
    // just after that boundary; it cannot borrow from two ordinary quotas.
    ASSERT_TRUE(worker.Resume());
    usleep(10000);
    ASSERT_TRUE(worker.Pause());
    uint64_t period_before = Stat(Read(node_ + "/cpu.stat"), "nr_periods");
    ASSERT_NE(UINT64_MAX, period_before);
    uint64_t period = period_before;
    uint64_t boundary_deadline = Clock(CLOCK_MONOTONIC) + 600000000ULL;
    do {
      period = Stat(Read(node_ + "/cpu.stat"), "nr_periods");
      if (period != UINT64_MAX && period > period_before) break;
      usleep(1000);
    } while (Clock(CLOCK_MONOTONIC) < boundary_deadline);
    ASSERT_NE(UINT64_MAX, period);
    ASSERT_GT(period, period_before) << "idle/runnable pulse did not arm a period event";
    uint64_t usage_before = Usage(node_);
    std::string before = Read(node_ + "/cpu.stat");
    uint64_t bursts_before = Stat(before, "nr_bursts");
    uint64_t burst_time_before = Stat(before, "burst_usec");
    ASSERT_NE(UINT64_MAX, bursts_before);
    ASSERT_NE(UINT64_MAX, burst_time_before);
    ASSERT_TRUE(worker.Resume());
    usleep(90000);
    ASSERT_TRUE(worker.Pause());
    uint64_t consumed = Usage(node_) - usage_before;
    EXPECT_EQ(period, Stat(Read(node_ + "/cpu.stat"), "nr_periods"))
        << "burst measurement crossed an ordinary quota refill";
    std::printf("CPU_BURST round=%u usage_usec=%llu\n", round,
                static_cast<unsigned long long>(consumed));
    // Without accumulated burst a 50ms quota cannot sustain this interval.
    EXPECT_GT(consumed, 70000U);
    EXPECT_LT(consumed, 115000U);
    usleep(300000);
    std::string after = Read(node_ + "/cpu.stat");
    uint64_t bursts_after = Stat(after, "nr_bursts");
    uint64_t burst_time_after = Stat(after, "burst_usec");
    ASSERT_NE(UINT64_MAX, bursts_after);
    ASSERT_NE(UINT64_MAX, burst_time_after);
    EXPECT_GT(bursts_after, bursts_before);
    EXPECT_GT(burst_time_after, burst_time_before + 20000U);
  }
}

TEST_F(CgroupCpuTest, FiniteQuotaAndPeriodUpdatesResetRunningCredits) {
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "75000 100000"));
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker worker(node_, 1, cpu); ASSERT_GT(worker.pid(), 0);
  usleep(200000);
  uint64_t before = Usage(node_);
  usleep(400000);
  uint64_t large = Usage(node_) - before;
  EXPECT_GT(large, 210000U);
  EXPECT_LT(large, 360000U);

  // A sharply reduced quota with a long new period exposes stale local
  // 5ms grants and stale old-period events without averaging them away.
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "1000 1000000"));
  before = Usage(node_);
  uint64_t reset_periods = Stat(Read(node_ + "/cpu.stat"), "nr_periods");
  ASSERT_NE(UINT64_MAX, reset_periods);
  usleep(150000);
  EXPECT_LT(Usage(node_) - before, 4000U);
  EXPECT_EQ(reset_periods, Stat(Read(node_ + "/cpu.stat"), "nr_periods"));

  // Old credits cannot remain usable after reducing the finite quota.
  ASSERT_EQ(0, Write(node_ + "/cpu.max", "10000 100000"));
  before = Usage(node_);
  usleep(400000);
  uint64_t small = Usage(node_) - before;
  EXPECT_GT(small, 15000U);
  EXPECT_LT(small, 80000U);

  ASSERT_EQ(0, Write(node_ + "/cpu.max", "60000 100000"));
  before = Usage(node_);
  usleep(400000);
  uint64_t expanded = Usage(node_) - before;
  EXPECT_GT(expanded, 160000U);
  EXPECT_LT(expanded, 310000U);
  EXPECT_GT(expanded, small * 2);

  ASSERT_EQ(0, Write(node_ + "/cpu.max", "10000 20000"));
  EXPECT_EQ("10000 20000\n", Read(node_ + "/cpu.max"));
  before = Usage(node_);
  uint64_t periods_before = Stat(Read(node_ + "/cpu.stat"), "nr_periods");
  ASSERT_NE(UINT64_MAX, periods_before);
  usleep(400000);
  uint64_t changed_period = Usage(node_) - before;
  EXPECT_GT(changed_period, 140000U);
  EXPECT_LT(changed_period, 260000U);
  uint64_t periods_after = Stat(Read(node_ + "/cpu.stat"), "nr_periods");
  ASSERT_NE(UINT64_MAX, periods_after);
  ASSERT_GE(periods_after, periods_before);
  EXPECT_GE(periods_after - periods_before, 15U);
}

TEST_F(CgroupCpuTest, CreateChildAndMigrateRunnableTaskWhileAncestorIsThrottled) {
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  const std::string limited = NewChild("limited"), free = NewChild("free");
  ASSERT_FALSE(limited.empty()); ASSERT_FALSE(free.empty());
  ASSERT_EQ(0, Write(limited + "/cgroup.subtree_control", "+cpu"));
  const std::string original = NewChild("limited/original");
  ASSERT_FALSE(original.empty());
  ASSERT_EQ(0, Write(limited + "/cpu.max", "1000 1000000"));
  int cpu = OneCpu(); ASSERT_GE(cpu, 0);
  BusyWorker worker(original, 1, cpu); ASSERT_GT(worker.pid(), 0);
  const uint64_t deadline = Clock(CLOCK_MONOTONIC) + 500000000ULL;
  bool frozen_now = false;
  do {
    uint64_t local = Stat(Read(limited + "/cpu.stat.local"), "throttled_usec");
    uint64_t usage = Usage(limited);
    usleep(10000);
    uint64_t next_local = Stat(Read(limited + "/cpu.stat.local"), "throttled_usec");
    if (local != UINT64_MAX && next_local != UINT64_MAX &&
        next_local > local && Usage(limited) - usage < 3000U) {
      frozen_now = true;
      break;
    }
  } while (Clock(CLOCK_MONOTONIC) < deadline);
  // nr_throttled is a completed-period counter, not the current frozen state.
  ASSERT_TRUE(frozen_now);

  const std::string added = NewChild("limited/added-while-throttled");
  ASSERT_FALSE(added.empty());
  ASSERT_EQ(0, Write(added + "/cgroup.procs", std::to_string(worker.pid())));
  uint64_t before = Usage(added);
  usleep(100000);
  // A new child must inherit the frozen ancestor rather than briefly escaping
  // its budget until the next local tick.
  EXPECT_LT(Usage(added) - before, 5000U);
  uint64_t frozen = Stat(Read(added + "/cpu.stat.local"), "throttled_usec");
  ASSERT_NE(UINT64_MAX, frozen);
  EXPECT_GT(frozen, 0U);
  ASSERT_EQ(0, Write(free + "/cgroup.procs", std::to_string(worker.pid())));
  before = Usage(free);
  usleep(200000);
  EXPECT_GT(Usage(free) - before, 100000U);
  worker.Stop();
  // TearDown removes added/original first, then their limited parent. This
  // also checks retirement of an empty queue with inherited throttle state.
}

TEST_F(CgroupCpuTest, CpuPlacementAndGroupMigrationCanRace) {
  cpu_set_t allowed;
  CPU_ZERO(&allowed);
  ASSERT_EQ(0, sched_getaffinity(0, sizeof(allowed), &allowed));
  std::vector<int> cpus;
  for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
    if (CPU_ISSET(cpu, &allowed)) cpus.push_back(cpu);
  }
  if (cpus.size() < 2) GTEST_SKIP() << "requires two allowed CPUs";
  ASSERT_EQ(0, Write(node_ + "/cgroup.subtree_control", "+cpu"));
  const std::string first = NewChild("migration-first");
  const std::string second = NewChild("migration-second");
  ASSERT_FALSE(first.empty()); ASSERT_FALSE(second.empty());
  BusyWorker worker(first, 1, cpus[0]);
  ASSERT_GT(worker.pid(), 0);
  std::atomic<int> placement_error{0};
  std::thread placement([&]() {
    for (unsigned i = 0; i < 40; ++i) {
      cpu_set_t mask;
      CPU_ZERO(&mask);
      CPU_SET(cpus[i % 2], &mask);
      if (sched_setaffinity(worker.pid(), sizeof(mask), &mask) != 0) {
        placement_error.store(errno);
        break;
      }
      usleep(1000);
    }
  });
  for (unsigned i = 0; i < 40; ++i) {
    EXPECT_EQ(0, Write((i % 2 ? first : second) + "/cgroup.procs",
                       std::to_string(worker.pid())));
    usleep(1000);
  }
  placement.join();
  EXPECT_EQ(0, placement_error.load());
  ASSERT_EQ(0, Write(second + "/cgroup.procs", std::to_string(worker.pid())));
  uint64_t old_usage = Usage(first), new_usage = Usage(second);
  usleep(200000);
  EXPECT_EQ(old_usage, Usage(first));
  EXPECT_GT(Usage(second) - new_usage, 100000U);
  worker.Stop();
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
