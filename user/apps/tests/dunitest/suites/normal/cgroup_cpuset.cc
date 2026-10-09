#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <unistd.h>
#include <time.h>

#include <algorithm>
#include <cstdio>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <set>
#include <sstream>
#include <string>
#include <vector>

namespace {

std::string Read(const std::string& path) {
  int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) return "<open failed>";
  std::string result;
  char buf[4096];
  ssize_t n;
  while ((n = read(fd, buf, sizeof(buf))) > 0) result.append(buf, n);
  close(fd);
  while (!result.empty() && result.back() == '\n') result.pop_back();
  return n < 0 ? "<read failed>" : result;
}

int Write(const std::string& path, const std::string& value) {
  int fd = open(path.c_str(), O_WRONLY | O_CLOEXEC);
  if (fd < 0) return errno;
  ssize_t n = write(fd, value.data(), value.size());
  int error = n < 0 ? errno : (n == static_cast<ssize_t>(value.size()) ? 0 : EIO);
  close(fd);
  return error;
}

std::set<int> Parse(const std::string& text) {
  std::set<int> result;
  std::istringstream stream(text);
  std::string part;
  while (std::getline(stream, part, ',')) {
    if (part.empty()) continue;
    char* end = nullptr;
    long first = std::strtol(part.c_str(), &end, 10);
    if (end == part.c_str() || first < 0) return {};
    long last = first;
    if (*end == '-') {
      const char* start = end + 1;
      last = std::strtol(start, &end, 10);
      if (end == start) return {};
    }
    if (*end || last < first || last >= CPU_SETSIZE) return {};
    for (int i = first; i <= last; ++i) result.insert(i);
  }
  return result;
}

std::string List(const std::set<int>& cpus) {
  std::string result;
  for (int cpu : cpus) {
    if (!result.empty()) result += ',';
    result += std::to_string(cpu);
  }
  return result;
}

cpu_set_t Mask(const std::set<int>& cpus) {
  cpu_set_t mask;
  CPU_ZERO(&mask);
  for (int cpu : cpus) CPU_SET(cpu, &mask);
  return mask;
}

std::set<int> Set(const cpu_set_t& mask) {
  std::set<int> result;
  for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
    if (CPU_ISSET(cpu, &mask)) result.insert(cpu);
  }
  return result;
}

bool Transfer(int fd, void* bytes, size_t size, bool send) {
  auto* p = static_cast<char*>(bytes);
  while (size) {
    ssize_t n = send ? write(fd, p, size) : read(fd, p, size);
    if (n < 0 && errno == EINTR) continue;
    if (n <= 0) return false;
    p += n;
    size -= n;
  }
  return true;
}

struct Report {
  int error;
  int cpu;
  cpu_set_t affinity;
  char cpu_list[512];
  char mem_list[128];
};

std::string StatusList(const std::string& status, const char* field) {
  size_t pos = status.find(field);
  if (pos == std::string::npos) return "<missing>";
  pos += std::strlen(field);
  while (pos < status.size() && (status[pos] == ' ' || status[pos] == '\t')) ++pos;
  return status.substr(pos, status.find('\n', pos) - pos);
}

Report Inspect() {
  Report report{};
  if (sched_getaffinity(0, sizeof(report.affinity), &report.affinity) != 0) {
    report.error = errno;
    return report;
  }
  for (int i = 0; i < 10000; ++i) {
    report.cpu = sched_getcpu();
    if (report.cpu < 0 || !CPU_ISSET(report.cpu, &report.affinity)) {
      report.error = report.cpu < 0 ? errno : EDOM;
      break;
    }
  }
  std::string status = Read("/proc/self/status");
  std::snprintf(report.cpu_list, sizeof(report.cpu_list), "%s",
                StatusList(status, "Cpus_allowed_list:").c_str());
  std::snprintf(report.mem_list, sizeof(report.mem_list), "%s",
                StatusList(status, "Mems_allowed_list:").c_str());
  return report;
}

void* InspectThread(void* opaque) {
  *static_cast<Report*>(opaque) = Inspect();
  return nullptr;
}

struct LiveThread {
  unsigned request = 0;
  unsigned done = 0;
  Report report{};
};

void* LiveThreadLoop(void* opaque) {
  auto* state = static_cast<LiveThread*>(opaque);
  unsigned previous = 0;
  for (;;) {
    unsigned request = __atomic_load_n(&state->request, __ATOMIC_ACQUIRE);
    if (request != previous) {
      state->report = Inspect();
      previous = request;
      __atomic_store_n(&state->done, previous, __ATOMIC_RELEASE);
    }
    sched_yield();
  }
}

// The worker runs outside gtest and never returns into the parent's fixture.
// Commands make affinity changes in the worker, so the runner's own affinity
// and cgroup membership remain untouched.
[[noreturn]] void Worker(int fd, const std::set<int>& all) {
  alarm(20);
  LiveThread live;
  bool has_live_thread = false;
  for (;;) {
    char command;
    if (!Transfer(fd, &command, sizeof(command), false) || command == 'Q') _exit(0);
    Report report{};
    if (command == 'A' || command == 'S') {
      auto mask = Mask(command == 'A' ? all : std::set<int>{*all.begin()});
      if (sched_setaffinity(0, sizeof(mask), &mask) != 0) report.error = errno;
      if (!report.error) report = Inspect();
    } else if (command == 'F') {
      int pipefd[2];
      if (pipe(pipefd) != 0) {
        report.error = errno;
      } else {
        pid_t child = fork();
        if (child == 0) {
          close(pipefd[0]);
          Report child_report = Inspect();
          bool ok = Transfer(pipefd[1], &child_report, sizeof(child_report), true);
          _exit(ok ? 0 : 1);
        }
        close(pipefd[1]);
        if (child < 0) report.error = errno;
        else if (!Transfer(pipefd[0], &report, sizeof(report), false)) report.error = EIO;
        close(pipefd[0]);
        if (child > 0) {
          int status;
          if (waitpid(child, &status, 0) != child || !WIFEXITED(status) || WEXITSTATUS(status))
            report.error = ECHILD;
        }
      }
    } else if (command == 'G' || command == 'H') {
      if (!has_live_thread) {
        pthread_t thread;
        report.error = pthread_create(&thread, nullptr, LiveThreadLoop, &live);
        has_live_thread = report.error == 0;
      }
      if (has_live_thread) {
        unsigned request = __atomic_add_fetch(&live.request, 1, __ATOMIC_RELEASE);
        while (__atomic_load_n(&live.done, __ATOMIC_ACQUIRE) != request) sched_yield();
        report = live.report;
      }
    } else if (command == 'T') {
      pthread_t thread;
      int error = pthread_create(&thread, nullptr, InspectThread, &report);
      if (error) report.error = error;
      else if ((error = pthread_join(thread, nullptr))) report.error = error;
    } else {
      report = Inspect();
    }
    if (!Transfer(fd, &report, sizeof(report), true)) _exit(1);
  }
}

class Cpuset : public ::testing::Test {
 protected:
  void SetUp() override {
    const char* supplied = std::getenv("DUNITEST_CPUSET_PARENT");
    parent_ = supplied ? supplied : "/sys/fs/cgroup";
    // CI boots may start with no child controllers enabled. Enable only
    // missing prerequisites, and restore only those additions after deleting
    // this fixture's own subtree. Existing enabled controllers are untouched.
    for (const char* controller : {"cpuset", "memory", "cpu"}) {
      std::istringstream controllers(Read(parent_ + "/cgroup.subtree_control"));
      bool enabled = false;
      for (std::string token; controllers >> token;) enabled |= token == controller;
      if (enabled) continue;
      ASSERT_EQ(0, Write(parent_ + "/cgroup.subtree_control", std::string("+") + controller));
      added_controllers_.push_back(controller);
    }
    available_ = Parse(Read(parent_ + "/cpuset.cpus.effective"));
    ASSERT_FALSE(available_.empty());
    mems_ = Parse(Read(parent_ + "/cpuset.mems.effective"));
    ASSERT_FALSE(mems_.empty());
    first_ = *available_.begin();
    second_ = available_.size() > 1 ? *std::next(available_.begin()) : first_;
    pair_ = {first_, second_};
    root_ = parent_ + "/dunitest-cpuset-" + std::to_string(getpid()) + "-" +
            std::to_string(sequence_++);
    ASSERT_EQ(0, mkdir(root_.c_str(), 0755)) << strerror(errno);
    dirs_.push_back(root_);
    ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+cpuset"));
    ASSERT_EQ(0, Write(root_ + "/cpuset.cpus", List(pair_)));
    ASSERT_EQ(0, Write(root_ + "/cpuset.mems", List(mems_)));
    a_ = Make(root_ + "/a");
    b_ = Make(root_ + "/b");
    ASSERT_FALSE(HasFatalFailure());
  }

  void TearDown() override {
    if (worker_ > 0) {
      char command = 'Q';
      // No SIGPIPE if a failed worker already exited.
      send(socket_, &command, 1, MSG_NOSIGNAL);
      close(socket_);
      kill(worker_, SIGKILL);
      int status;
      while (waitpid(worker_, &status, 0) < 0 && errno == EINTR) {}
    }
    for (auto it = dirs_.rbegin(); it != dirs_.rend(); ++it) {
      EXPECT_EQ(0, rmdir(it->c_str())) << *it << ": " << strerror(errno);
    }
    for (auto it = added_controllers_.rbegin(); it != added_controllers_.rend(); ++it) {
      EXPECT_EQ(0, Write(parent_ + "/cgroup.subtree_control", "-" + *it));
    }
  }

  std::string Make(const std::string& path) {
    if (mkdir(path.c_str(), 0755) != 0) {
      ADD_FAILURE() << "mkdir " << path << ": " << strerror(errno);
      return "";
    }
    dirs_.push_back(path);
    return path;
  }

  void Start(const std::string& group) {
    int sockets[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM | SOCK_CLOEXEC, 0, sockets));
    worker_ = fork();
    ASSERT_GE(worker_, 0);
    if (worker_ == 0) {
      close(sockets[0]);
      Worker(sockets[1], pair_);
    }
    close(sockets[1]);
    socket_ = sockets[0];
    ASSERT_EQ(0, Write(group + "/cgroup.procs", std::to_string(worker_)));
  }

  Report Command(char command) {
    Report report{};
    if (!Transfer(socket_, &command, sizeof(command), true) ||
        !Transfer(socket_, &report, sizeof(report), false)) {
      ADD_FAILURE() << "worker command " << command << " failed: " << strerror(errno);
      report.error = EIO;
    }
    return report;
  }

  void Expect(char command, const std::set<int>& expected) {
    Report report = Command(command);
    ASSERT_EQ(0, report.error) << strerror(report.error);
    EXPECT_EQ(expected, Set(report.affinity));
    EXPECT_TRUE(expected.count(report.cpu)) << "executed on CPU " << report.cpu;
    EXPECT_EQ(expected, Parse(report.cpu_list));
    EXPECT_EQ(mems_, Parse(report.mem_list));
  }

  static unsigned sequence_;
  std::string parent_, root_, a_, b_;
  std::vector<std::string> dirs_;
  std::vector<std::string> added_controllers_;
  std::set<int> available_, pair_, mems_;
  int first_ = -1, second_ = -1;
  pid_t worker_ = -1;
  int socket_ = -1;
};

unsigned Cpuset::sequence_ = 0;

TEST_F(Cpuset, EmptyInheritanceAndAncestorIntersection) {
  EXPECT_EQ("", Read(a_ + "/cpuset.cpus"));
  EXPECT_EQ("", Read(a_ + "/cpuset.mems"));
  EXPECT_EQ(pair_, Parse(Read(a_ + "/cpuset.cpus.effective")));
  EXPECT_EQ(mems_, Parse(Read(a_ + "/cpuset.mems.effective")));
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(second_)));
  ASSERT_EQ(0, Write(root_ + "/cpuset.cpus", std::to_string(first_)));
  // With two CPUs the configured child mask has no overlap with its parent;
  // cgroup v2 then inherits its parent's usable CPUs instead of becoming empty.
  EXPECT_EQ(std::set<int>{first_}, Parse(Read(a_ + "/cpuset.cpus.effective")));
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  Expect('P', {first_});
  ASSERT_EQ(0, Write(root_ + "/cpuset.cpus", List(pair_)));
  Expect('P', {second_});
}

TEST_F(Cpuset, ListSyntaxAndInvalidWritesPreserveState) {
  const auto possible = Parse(Read("/sys/devices/system/cpu/possible"));
  ASSERT_FALSE(possible.empty());
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", "ALL"));
  EXPECT_EQ(possible, Parse(Read(a_ + "/cpuset.cpus")));
  const auto highest = *possible.rbegin();
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", "N"));
  EXPECT_EQ(std::set<int>{highest}, Parse(Read(a_ + "/cpuset.cpus")));
  const std::string cpu = std::to_string(first_);
  for (const std::string& input : {cpu + "," + cpu, ", " + cpu + ",,",
                                  cpu + "-" + cpu + ":1/1"}) {
    ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", input)) << input;
    EXPECT_EQ(cpu, Read(a_ + "/cpuset.cpus"));
  }
  for (const char* input : {"garbage", "1-0", "0-0:1/0", "0-0:2/1", "0:1/1"}) {
    EXPECT_EQ(EINVAL, Write(a_ + "/cpuset.cpus", input)) << input;
    EXPECT_EQ(cpu, Read(a_ + "/cpuset.cpus"));
  }
  EXPECT_EQ(EOVERFLOW, Write(a_ + "/cpuset.cpus", "4294967296"));
  EXPECT_EQ(cpu, Read(a_ + "/cpuset.cpus"));
  EXPECT_EQ(ERANGE, Write(a_ + "/cpuset.cpus", "1048576"));
  EXPECT_EQ(cpu, Read(a_ + "/cpuset.cpus"));
  EXPECT_EQ(EINVAL, Write(a_ + "/cpuset.mems", "bogus"));
  ASSERT_EQ(0, Write(a_ + "/cpuset.mems", List(mems_)));
  EXPECT_EQ(mems_, Parse(Read(a_ + "/cpuset.mems")));
  if (Parse(Read("/sys/devices/system/node/possible")) == std::set<int>{0}) {
    int error = Write(a_ + "/cpuset.mems", "1");
    // Linux !CONFIG_NUMA uses MAX_NUMNODES=1 and rejects during parsing;
    // NUMA-enabled kernels can parse node 1 then reject the nonexistent node.
    EXPECT_TRUE(error == EINVAL || error == ERANGE) << "errno=" << error;
    EXPECT_EQ(mems_, Parse(Read(a_ + "/cpuset.mems")));
  }
}

TEST_F(Cpuset, AffinityIntersectionRestorationAndMigration) {
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  Expect('A', pair_);
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(first_)));
  Expect('P', {first_});
  EXPECT_EQ(ENOSPC, Write(a_ + "/cpuset.cpus", "\n"));
  EXPECT_EQ(std::to_string(first_), Read(a_ + "/cpuset.cpus"));
  ASSERT_EQ(0, Write(a_ + "/cpuset.mems", List(mems_)));
  EXPECT_EQ(ENOSPC, Write(a_ + "/cpuset.mems", "\n"));
  EXPECT_EQ(mems_, Parse(Read(a_ + "/cpuset.mems")));
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", List(pair_)));
  Expect('P', pair_);
  Expect('S', {first_});
  ASSERT_EQ(0, Write(b_ + "/cpuset.cpus", std::to_string(second_)));
  ASSERT_EQ(0, Write(b_ + "/cgroup.procs", std::to_string(worker_)));
  Expect('P', {second_});
  if (first_ != second_) {
    EXPECT_EQ(EINVAL, Command('S').error);
  }
  ASSERT_EQ(0, Write(a_ + "/cgroup.procs", std::to_string(worker_)));
  Expect('P', {first_});
}

TEST_F(Cpuset, ForkAndPthreadInheritCurrentConstraint) {
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(first_)));
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  Expect('A', {first_});
  Expect('F', {first_});
  Expect('T', {first_});
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(second_)));
  Expect('F', {second_});
  Expect('T', {second_});
}

TEST_F(Cpuset, ProcessMigrationMovesExistingThreadGroup) {
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(first_)));
  ASSERT_EQ(0, Write(b_ + "/cpuset.cpus", std::to_string(second_)));
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  Expect('A', {first_});
  Expect('G', {first_});
  ASSERT_EQ(0, Write(b_ + "/cgroup.procs", std::to_string(worker_)));
  Expect('P', {second_});
  Expect('H', {second_});
  Expect('F', {second_});
  Expect('T', {second_});
}

struct RunningState {
  unsigned started;
  unsigned phase;
  unsigned observed;
  int expected_cpu;
  int error_cpu;
};

class RunningMapping {
 public:
  RunningMapping() {
    void* mapping = mmap(nullptr, sizeof(RunningState), PROT_READ | PROT_WRITE,
                         MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    state = mapping == MAP_FAILED ? nullptr : static_cast<RunningState*>(mapping);
    if (state) std::memset(state, 0, sizeof(*state));
  }
  ~RunningMapping() { if (state) munmap(state, sizeof(*state)); }
  RunningState* state;
};

bool AwaitPhase(unsigned* value, unsigned expected) {
  timespec start{}, now{};
  clock_gettime(CLOCK_MONOTONIC, &start);
  do {
    if (__atomic_load_n(value, __ATOMIC_ACQUIRE) == expected) return true;
    sched_yield();
    clock_gettime(CLOCK_MONOTONIC, &now);
  } while (now.tv_sec - start.tv_sec < 3);
  return false;
}

TEST_F(Cpuset, RunningTaskHasMigratedWhenConfigurationWriteReturns) {
  RunningMapping mapping;
  ASSERT_NE(nullptr, mapping.state) << strerror(errno);
  auto* state = mapping.state;
  worker_ = fork();
  ASSERT_GE(worker_, 0);
  if (worker_ == 0) {
    alarm(20);
    __atomic_store_n(&state->started, 1, __ATOMIC_RELEASE);
    // This task deliberately stays CPU-bound. Unlike the command worker,
    // it cannot satisfy a placement update merely by already being asleep.
    for (;;) {
      unsigned phase = __atomic_load_n(&state->phase, __ATOMIC_ACQUIRE);
      if (!phase) continue;
      int expected = __atomic_load_n(&state->expected_cpu, __ATOMIC_RELAXED);
      int cpu = sched_getcpu();
      if (__atomic_load_n(&state->phase, __ATOMIC_ACQUIRE) != phase) continue;
      if (cpu != expected)
        __atomic_store_n(&state->error_cpu, cpu < 0 ? -1 : cpu + 1, __ATOMIC_RELEASE);
      __atomic_store_n(&state->observed, phase, __ATOMIC_RELEASE);
    }
  }
  ASSERT_TRUE(AwaitPhase(&state->started, 1));
  ASSERT_EQ(0, Write(a_ + "/cgroup.procs", std::to_string(worker_)));
  for (unsigned phase = 1; phase <= 8; ++phase) {
    int cpu = phase % 2 ? first_ : second_;
    __atomic_store_n(&state->phase, 0, __ATOMIC_RELEASE);
    ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(cpu)));
    __atomic_store_n(&state->expected_cpu, cpu, __ATOMIC_RELAXED);
    __atomic_store_n(&state->phase, phase, __ATOMIC_RELEASE);
    ASSERT_TRUE(AwaitPhase(&state->observed, phase));
    EXPECT_EQ(0, __atomic_load_n(&state->error_cpu, __ATOMIC_ACQUIRE))
        << "task still executed on an excluded CPU after configuration completed";
  }
}

TEST_F(Cpuset, DisableInvalidatesOldFileDescriptionPermanently) {
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(first_)));
  int fd = open((a_ + "/cpuset.cpus").c_str(), O_RDWR | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  Start(a_);
  if (HasFatalFailure()) { close(fd); return; }
  Expect('A', {first_});
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "-cpuset"));
  Expect('P', pair_);
  char buf[64];
  errno = 0;
  EXPECT_EQ(-1, pread(fd, buf, sizeof(buf), 0));
  EXPECT_EQ(ENODEV, errno);
  errno = 0;
  EXPECT_EQ(-1, pwrite(fd, "\n", 1, 0));
  EXPECT_EQ(ENODEV, errno);
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+cpuset"));
  EXPECT_EQ("", Read(a_ + "/cpuset.cpus"));
  Expect('P', pair_);
  errno = 0;
  EXPECT_EQ(-1, pread(fd, buf, sizeof(buf), 0));
  EXPECT_EQ(ENODEV, errno);
  errno = 0;
  EXPECT_EQ(-1, pwrite(fd, "\n", 1, 0));
  EXPECT_EQ(ENODEV, errno);
  close(fd);
}

TEST_F(Cpuset, CpusetInternalProcessesAndControllerDisableBusy) {
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  // cpuset is a threaded controller in Linux 6.6.139. With no domain
  // controllers enabled, a prospective thread root permits internal tasks.
  ASSERT_EQ(0, Write(a_ + "/cgroup.subtree_control", "+cpuset"));
  EXPECT_EQ("domain threaded", Read(a_ + "/cgroup.type"));
  auto child = Make(a_ + "/domain-child");
  ASSERT_FALSE(child.empty());
  EXPECT_EQ("domain invalid", Read(child + "/cgroup.type"));
  EXPECT_EQ(EOPNOTSUPP, Write(child + "/cgroup.subtree_control", "+cpuset"));
  EXPECT_EQ(EOPNOTSUPP, Write(child + "/cgroup.procs", std::to_string(worker_)));
  ASSERT_EQ(0, Write(b_ + "/cgroup.subtree_control", "+cpuset"));
  ASSERT_EQ(0, Write(b_ + "/cgroup.procs", std::to_string(worker_)));
  EXPECT_EQ(EBUSY, Write(root_ + "/cgroup.subtree_control", "-cpuset"));
  Expect('P', pair_);
}

TEST_F(Cpuset, InvalidDomainAllowsDisableAndNoopEnable) {
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+cpu"));
  ASSERT_EQ(0, Write(a_ + "/cgroup.subtree_control", "+cpuset +cpu"));
  auto child = Make(a_ + "/domain-child");
  ASSERT_FALSE(child.empty());
  ASSERT_EQ(0, Write(child + "/cgroup.subtree_control", "+cpuset +cpu"));
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ("domain invalid", Read(child + "/cgroup.type"));
  ASSERT_EQ(0, Write(child + "/cgroup.subtree_control", "+cpuset"));
  ASSERT_EQ(0, Write(child + "/cgroup.subtree_control", "-cpuset"));
  EXPECT_EQ("cpu", Read(child + "/cgroup.subtree_control"));
}

TEST_F(Cpuset, DomainControllerStillRejectsInternalProcesses) {
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+memory"));
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ(EBUSY, Write(a_ + "/cgroup.subtree_control", "+cpuset +memory"));
  ASSERT_EQ(0, Write(b_ + "/cgroup.subtree_control", "+cpuset +memory"));
  EXPECT_EQ(EBUSY, Write(b_ + "/cgroup.procs", std::to_string(worker_)));
  Expect('P', pair_);
}

TEST_F(Cpuset, PopulatedDomainChildPreventsInternalCompetition) {
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  // A cpuset-only parent can become a thread root only if it has no
  // populated domain children. The source child is still populated when
  // migration admission runs, so moving this worker into the parent fails.
  EXPECT_EQ(EBUSY, Write(root_ + "/cgroup.procs", std::to_string(worker_)));
  EXPECT_EQ("domain", Read(root_ + "/cgroup.type"));
  Expect('P', pair_);
}

// Keep the clone3 ABI local: older libc headers may lack struct clone_args.
struct CloneArgs {
  uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size;
  uint64_t tls, set_tid, set_tid_size, cgroup;
};

TEST_F(Cpuset, CloneIntoCgroupUsesDestinationBeforeFirstRun) {
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(second_)));
  int fd = open(a_.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  int pipes[2];
  ASSERT_EQ(0, pipe(pipes));
  CloneArgs args{};
  args.flags = uint64_t{1} << 33;  // CLONE_INTO_CGROUP
  args.exit_signal = SIGCHLD;
  args.cgroup = fd;
#ifdef SYS_clone3
  pid_t child = syscall(SYS_clone3, &args, sizeof(args));
#else
  errno = ENOSYS;
  pid_t child = -1;
#endif
  int error = errno;
  if (child == 0) {
    close(pipes[0]);
    Report report = Inspect();
    bool ok = Transfer(pipes[1], &report, sizeof(report), true);
    _exit(ok ? 0 : 1);
  }
  close(pipes[1]);
  close(fd);
  if (child < 0) {
    close(pipes[0]);
    FAIL() << "clone3(CLONE_INTO_CGROUP): " << strerror(error)
           << "; kernel support and permission are required";
  }
  Report report{};
  EXPECT_TRUE(Transfer(pipes[0], &report, sizeof(report), false));
  close(pipes[0]);
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));
  ASSERT_EQ(0, report.error);
  EXPECT_EQ(std::set<int>{second_}, Set(report.affinity));
  EXPECT_EQ(second_, report.cpu);
}

TEST_F(Cpuset, CloneIntoSameCgroupStillChecksWritePermission) {
  int fd = open(a_.c_str(), O_RDONLY | O_DIRECTORY | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    alarm(20);
    if (Write(a_ + "/cgroup.procs", "0") != 0 || setgid(65534) != 0 || setuid(65534) != 0) _exit(2);
    CloneArgs args{};
    args.flags = uint64_t{1} << 33;
    args.exit_signal = SIGCHLD;
    args.cgroup = fd;
#ifdef SYS_clone3
    pid_t result = syscall(SYS_clone3, &args, sizeof(args));
    int error = errno;
    if (result == 0) _exit(3);
    if (result > 0) { int status; waitpid(result, &status, 0); _exit(4); }
    _exit(error == EACCES ? 0 : 5);
#else
    _exit(6);
#endif
  }
  close(fd);
  int status = 0;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

}  // namespace

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
