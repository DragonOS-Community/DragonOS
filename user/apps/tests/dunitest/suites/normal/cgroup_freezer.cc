#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/ptrace.h>
#include <sys/epoll.h>
#include <sys/inotify.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <cstdint>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <sstream>
#include <string>
#include <vector>

namespace {
struct ScopedFd {
  explicit ScopedFd(int value) : value(value) {}
  ~ScopedFd() { if (value >= 0) close(value); }
  int value;
};
uint64_t Now() {
  timespec time{};
  clock_gettime(CLOCK_MONOTONIC, &time);
  return uint64_t(time.tv_sec) * 1000000000 + time.tv_nsec;
}

void Pause() {
  timespec time{0, 1000000};
  while (nanosleep(&time, &time) < 0 && errno == EINTR) {}
}

std::string Read(const std::string& path) {
  int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) return "<open failed>";
  std::string result;
  char bytes[1024];
  ssize_t count;
  while ((count = read(fd, bytes, sizeof(bytes))) > 0) result.append(bytes, count);
  close(fd);
  return count < 0 ? "<read failed>" : result;
}

int Write(const std::string& path, const std::string& value) {
  int fd = open(path.c_str(), O_WRONLY | O_CLOEXEC);
  if (fd < 0) return errno;
  ssize_t count = write(fd, value.data(), value.size());
  int error = count < 0 ? errno : (count == ssize_t(value.size()) ? 0 : EIO);
  close(fd);
  return error;
}

int Event(const std::string& path, const char* name) {
  std::istringstream stream(Read(path + "/cgroup.events"));
  std::string key;
  int value;
  while (stream >> key >> value) if (key == name) return value;
  return -1;
}

template <typename Predicate>
bool Until(Predicate predicate, uint64_t milliseconds = 5000) {
  uint64_t deadline = Now() + milliseconds * 1000000;
  do {
    if (predicate()) return true;
    Pause();
  } while (Now() < deadline);
  return predicate();
}

struct Shared {
  uint64_t count = 0;
  uint64_t thread_count = 0;
  unsigned signals = 0;
  unsigned fork_ready = 0;
  int thread_tid = 0;
  unsigned vfork_ready = 0;
  unsigned vfork_child_ran = 0;
  unsigned create_request = 0;
  unsigned create_started = 0;
  unsigned create_done = 0;
  int create_error = 0;
  unsigned exec_ready = 0;
  unsigned exec_request = 0;
};

Shared* signal_shared;
void RecordSignal(int) {
  __atomic_fetch_add(&signal_shared->signals, 1, __ATOMIC_RELEASE);
}
void* ThreadWorker(void* data) {
  auto* shared = static_cast<Shared*>(data);
  __atomic_store_n(&shared->thread_tid, int(syscall(SYS_gettid)), __ATOMIC_RELEASE);
  for (;;) __atomic_fetch_add(&shared->thread_count, 1, __ATOMIC_RELEASE);
  return nullptr;
}

void* CreationRaceThread(void* data) {
  auto* shared = static_cast<Shared*>(data);
  __atomic_fetch_add(&shared->thread_count, 1, __ATOMIC_RELEASE);
  return nullptr;
}

struct ExecThreadArgs {
  Shared* shared;
  char descriptor[16];
};

void* NonLeaderExecThread(void* data) {
  auto* args = static_cast<ExecThreadArgs*>(data);
  __atomic_store_n(&args->shared->exec_ready, 1, __ATOMIC_RELEASE);
  while (!__atomic_load_n(&args->shared->exec_request, __ATOMIC_ACQUIRE)) sched_yield();
  execl("/proc/self/exe", "cgroup_freezer_test", "--freezer-exec-worker",
        args->descriptor, static_cast<char*>(nullptr));
  _exit(92);
}

class Freezer : public ::testing::Test {
 protected:
  void SetUp() override {
    const char* supplied = std::getenv("DUNITEST_FREEZER_PARENT");
    parent_ = supplied ? supplied : "/sys/fs/cgroup";
    root_ = parent_ + "/dunitest-freezer-" + std::to_string(getpid()) + "-" +
            std::to_string(sequence_++);
    ASSERT_EQ(0, mkdir(root_.c_str(), 0755)) << strerror(errno);
    dirs_.push_back(root_);
    shared_ = static_cast<Shared*>(mmap(nullptr, sizeof(Shared), PROT_READ | PROT_WRITE,
                                      MAP_SHARED | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, shared_);
    *shared_ = Shared{};
  }

  void TearDown() override {
    for (const auto& dir : dirs_) Write(dir + "/cgroup.freeze", "0");
    if (worker_ > 0) {
      kill(worker_group_ ? -worker_ : worker_, SIGKILL);
      int status = 0;
      bool reaped = Until([&] {
        return waitpid(worker_, &status, WNOHANG) == worker_ &&
               (WIFEXITED(status) || WIFSIGNALED(status));
      });
      EXPECT_TRUE(reaped) << "worker did not exit after SIGKILL";
      worker_ = -1;
      if (worker_group_) {
        EXPECT_TRUE(Until([&] { return Event(root_, "populated") == 0; }))
            << "forked worker family did not leave its cgroup";
      }
    }
    if (shared_ != MAP_FAILED) munmap(shared_, sizeof(Shared));
    for (auto it = dirs_.rbegin(); it != dirs_.rend(); ++it)
      EXPECT_EQ(0, rmdir(it->c_str())) << *it << ": " << strerror(errno);
  }

  std::string Make(const std::string& path) {
    if (mkdir(path.c_str(), 0755) != 0) {
      ADD_FAILURE() << path << ": " << strerror(errno);
      return "";
    }
    dirs_.push_back(path);
    return path;
  }

  uint64_t Count() { return __atomic_load_n(&shared_->count, __ATOMIC_ACQUIRE); }

  void Start(const std::string& group, bool threads = false) {
    worker_ = fork();
    ASSERT_GE(worker_, 0);
    if (worker_ == 0) {
      alarm(30);
      signal_shared = shared_;
      signal(SIGUSR1, RecordSignal);
      if (threads) {
        pthread_t thread;
        if (pthread_create(&thread, nullptr, ThreadWorker, shared_) != 0) _exit(2);
      }
      for (;;) __atomic_fetch_add(&shared_->count, 1, __ATOMIC_RELEASE);
    }
    ASSERT_EQ(0, Write(group + "/cgroup.procs", std::to_string(worker_)));
    ASSERT_TRUE(Until([&] { return Count() > 1000; }));
    if (threads) {
      ASSERT_TRUE(Until([&] {
        return __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE) > 1000;
      }));
    }
  }

  void Frozen(const std::string& group, bool value) {
    ASSERT_TRUE(Until([&] { return Event(group, "frozen") == int(value); }))
        << Read(group + "/cgroup.events");
  }

  void Stationary() {
    uint64_t count = Count();
    // The completed frozen event is the synchronization point. Sampling
    // afterwards detects fake acknowledgements while the worker still runs.
    uint64_t deadline = Now() + 100000000;
    do {
      ASSERT_EQ(count, Count());
      Pause();
    } while (Now() < deadline);
  }

  void Progress() {
    uint64_t count = Count();
    ASSERT_TRUE(Until([&] { return Count() > count + 1000; }));
  }

  std::string parent_, root_;
  std::vector<std::string> dirs_;
  Shared* shared_ = static_cast<Shared*>(MAP_FAILED);
  pid_t worker_ = -1;
  bool worker_group_ = false;
  static unsigned sequence_;
};
unsigned Freezer::sequence_ = 0;

TEST_F(Freezer, EmptyGroupAcknowledgesFreezeAndThaw) {
  EXPECT_EQ("0\n", Read(root_ + "/cgroup.freeze"));
  EXPECT_EQ(0, Event(root_, "frozen"));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  EXPECT_EQ("1\n", Read(root_ + "/cgroup.freeze"));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
}

TEST_F(Freezer, RunningWorkerReallyStopsAndResumes) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
}

TEST_F(Freezer, RejectsNonBooleanRequestsWithoutChangingState) {
  for (const char* value : {"2", "-1", "garbage", "1 0"}) {
    int expected = (!strcmp(value, "2") || !strcmp(value, "-1")) ? ERANGE : EINVAL;
    EXPECT_EQ(expected, Write(root_ + "/cgroup.freeze", value)) << value;
    EXPECT_EQ("0\n", Read(root_ + "/cgroup.freeze"));
    EXPECT_EQ(0, Event(root_, "frozen"));
  }
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", " 1\n"));
  Frozen(root_, true);
}

TEST_F(Freezer, AncestorFreezeIsEffectiveButDoesNotOverwriteChildRequest) {
  std::string child = Make(root_ + "/child");
  Start(child);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  Frozen(child, true);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ("0\n", Read(child + "/cgroup.freeze"));
  Stationary();
  ASSERT_EQ(0, Write(child + "/cgroup.freeze", "1"));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Frozen(child, true);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  ASSERT_EQ(0, Write(child + "/cgroup.freeze", "0"));
  Frozen(child, false);
  Progress();
}

TEST_F(Freezer, ChildThawCannotOverrideFrozenAncestor) {
  std::string child = Make(root_ + "/child");
  Start(child);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_EQ(0, Write(child + "/cgroup.freeze", "0"));
  Frozen(child, true);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(child, false);
  Progress();
}

TEST_F(Freezer, NewDescendantInheritsEffectiveFreeze) {
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  std::string child = Make(root_ + "/child");
  EXPECT_EQ("0\n", Read(child + "/cgroup.freeze"));
  Frozen(child, true);
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(child, false);
}

TEST_F(Freezer, MigrationIntoAndOutOfFrozenGroupChangesExecution) {
  std::string frozen = Make(root_ + "/frozen");
  std::string running = Make(root_ + "/running");
  Start(running);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(frozen + "/cgroup.freeze", "1"));
  Frozen(frozen, true);
  ASSERT_EQ(0, Write(frozen + "/cgroup.procs", std::to_string(worker_)));
  Frozen(frozen, true);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  ASSERT_EQ(0, Write(running + "/cgroup.procs", std::to_string(worker_)));
  Progress();
  EXPECT_EQ(1, Event(frozen, "frozen"));
}

TEST_F(Freezer, SigkillExitsWithoutThaw) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, kill(worker_, SIGKILL));
  int status = 0;
  ASSERT_TRUE(Until([&] { return waitpid(worker_, &status, WNOHANG) == worker_; }));
  worker_ = -1;
  EXPECT_TRUE(WIFSIGNALED(status));
  EXPECT_EQ(SIGKILL, WTERMSIG(status));
  Frozen(root_, true);
  EXPECT_EQ(0, Event(root_, "populated"));
}

TEST_F(Freezer, JobControlStopRemainsStoppedAfterThaw) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, kill(worker_, SIGSTOP));
  int status = 0;
  ASSERT_TRUE(Until([&] { return waitpid(worker_, &status, WNOHANG | WUNTRACED) == worker_; }));
  ASSERT_TRUE(WIFSTOPPED(status));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  ASSERT_EQ(0, kill(worker_, SIGCONT));
  Progress();
}

TEST_F(Freezer, SigcontDoesNotRunFrozenWorker) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, kill(worker_, SIGSTOP));
  int status = 0;
  ASSERT_TRUE(Until([&] { return waitpid(worker_, &status, WNOHANG | WUNTRACED) == worker_; }));
  ASSERT_TRUE(WIFSTOPPED(status));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_EQ(0, kill(worker_, SIGCONT));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
}

TEST_F(Freezer, EventsPollReportsCompletionAndThaw) {
  ScopedFd owned(open((root_ + "/cgroup.events").c_str(), O_RDONLY | O_CLOEXEC));
  int fd = owned.value;
  ASSERT_GE(fd, 0);
  char bytes[256];
  EXPECT_GT(read(fd, bytes, sizeof(bytes)), 0);
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  pollfd event{fd, POLLPRI | POLLERR, 0};
  EXPECT_EQ(1, poll(&event, 1, 5000));
  EXPECT_NE(0, event.revents & POLLPRI);
  EXPECT_EQ(0, lseek(fd, 0, SEEK_SET));
  EXPECT_GT(read(fd, bytes, sizeof(bytes)), 0);
  Frozen(root_, true);
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  event.revents = 0;
  EXPECT_EQ(1, poll(&event, 1, 5000));
  EXPECT_NE(0, event.revents & POLLPRI);
  Frozen(root_, false);
}

TEST_F(Freezer, AllThreadsFreezeAndResumeTogether) {
  Start(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  uint64_t thread_count = __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE);
  Stationary();
  EXPECT_EQ(thread_count, __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
  EXPECT_TRUE(Until([&] {
    return __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE) > thread_count + 1000;
  }));
}

TEST_F(Freezer, OrdinarySignalRemainsPendingUntilThaw) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, kill(worker_, SIGUSR1));
  Stationary();
  EXPECT_EQ(0u, __atomic_load_n(&shared_->signals, __ATOMIC_ACQUIRE));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  EXPECT_TRUE(Until([&] {
    return __atomic_load_n(&shared_->signals, __ATOMIC_ACQUIRE) == 1;
  }));
  Progress();
}

TEST_F(Freezer, BlockedReadCannotReturnToUserspaceUntilThaw) {
  int pipefd[2];
  ASSERT_EQ(0, pipe(pipefd));
  ScopedFd read_end(pipefd[0]), write_end(pipefd[1]);
  worker_ = fork();
  ASSERT_GE(worker_, 0);
  if (worker_ == 0) {
    close(pipefd[1]);
    alarm(30);
    char byte;
    ssize_t count;
    do { count = read(pipefd[0], &byte, 1); } while (count < 0 && errno == EINTR);
    if (count != 1) _exit(2);
    for (;;) __atomic_fetch_add(&shared_->count, 1, __ATOMIC_RELEASE);
  }
  close(read_end.value);
  read_end.value = -1;
  ASSERT_EQ(0, Write(root_ + "/cgroup.procs", std::to_string(worker_)));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ(1, write(pipefd[1], "X", 1));
  close(write_end.value);
  write_end.value = -1;
  Stationary();
  EXPECT_EQ(0u, Count());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
}

TEST_F(Freezer, ForkedChildInheritsMembershipAndBothTasksFreeze) {
  worker_ = fork();
  ASSERT_GE(worker_, 0);
  if (worker_ == 0) {
    alarm(30);
    if (setpgid(0, 0) != 0 ||
        Write(root_ + "/cgroup.procs", std::to_string(getpid())) != 0) _exit(2);
    pid_t child = fork();
    if (child < 0) _exit(3);
    if (child == 0) {
      for (;;) __atomic_fetch_add(&shared_->thread_count, 1, __ATOMIC_RELEASE);
    }
    __atomic_store_n(&shared_->fork_ready, 1, __ATOMIC_RELEASE);
    for (;;) __atomic_fetch_add(&shared_->count, 1, __ATOMIC_RELEASE);
  }
  ASSERT_EQ(0, setpgid(worker_, worker_));
  worker_group_ = true;
  ASSERT_TRUE(Until([&] {
    return __atomic_load_n(&shared_->fork_ready, __ATOMIC_ACQUIRE) == 1;
  }));
  ASSERT_TRUE(Until([&] {
    return Count() > 1000 &&
           __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE) > 1000;
  }));
  std::istringstream tasks(Read(root_ + "/cgroup.procs"));
  int pid, count = 0;
  while (tasks >> pid) ++count;
  EXPECT_EQ(2, count);
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  uint64_t child_count = __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE);
  Stationary();
  EXPECT_EQ(child_count, __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
  EXPECT_TRUE(Until([&] {
    return __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE) > child_count + 1000;
  }));
}

TEST_F(Freezer, RepeatedTransitionsDoNotLoseWakeups) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  for (int iteration = 0; iteration < 20; ++iteration) {
    ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
    Frozen(root_, true);
    ASSERT_FALSE(HasFatalFailure());
    uint64_t count = Count();
    ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
    EXPECT_EQ(count, Count());
    ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
    Frozen(root_, false);
    Progress();
    ASSERT_FALSE(HasFatalFailure());
  }
}

long Trace(long command, pid_t task) {
  return syscall(SYS_ptrace, command, task, 0, 0);
}

TEST_F(Freezer, PtraceStopRemainsStoppedAfterThaw) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Trace(PTRACE_SEIZE, worker_));
  ASSERT_EQ(0, Trace(PTRACE_INTERRUPT, worker_));
  int status = 0;
  ASSERT_TRUE(Until([&] { return waitpid(worker_, &status, WNOHANG) == worker_; }));
  ASSERT_TRUE(WIFSTOPPED(status));
  EXPECT_EQ(PTRACE_EVENT_STOP, unsigned(status) >> 16);
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  ASSERT_EQ(0, Trace(PTRACE_CONT, worker_));
  Progress();
  ASSERT_EQ(0, Trace(PTRACE_INTERRUPT, worker_));
  ASSERT_TRUE(Until([&] { return waitpid(worker_, &status, WNOHANG) == worker_; }));
  ASSERT_EQ(0, Trace(PTRACE_DETACH, worker_));
}

TEST_F(Freezer, PtraceInterruptWorksWhileFrozenButContinueCannotEscape) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Trace(PTRACE_SEIZE, worker_));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_EQ(0, Trace(PTRACE_INTERRUPT, worker_));
  int status = 0;
  ASSERT_TRUE(Until([&] { return waitpid(worker_, &status, WNOHANG) == worker_; }));
  ASSERT_TRUE(WIFSTOPPED(status));
  EXPECT_EQ(PTRACE_EVENT_STOP, unsigned(status) >> 16);
  Frozen(root_, true);
  ASSERT_EQ(0, Trace(PTRACE_CONT, worker_));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
  ASSERT_EQ(0, Trace(PTRACE_INTERRUPT, worker_));
  ASSERT_TRUE(Until([&] { return waitpid(worker_, &status, WNOHANG) == worker_; }));
  ASSERT_EQ(0, Trace(PTRACE_DETACH, worker_));
}

TEST_F(Freezer, VforkParentSafeWaitAndChildFreezeComplete) {
  int pipefd[2];
  ASSERT_EQ(0, pipe(pipefd));
  ScopedFd reader(pipefd[0]), writer(pipefd[1]);
  worker_ = fork();
  ASSERT_GE(worker_, 0);
  if (worker_ == 0) {
    close(writer.value);
    alarm(30);
    if (setpgid(0, 0) != 0 ||
        Write(root_ + "/cgroup.procs", std::to_string(getpid())) != 0) _exit(2);
    pid_t child = vfork();
    if (child == 0) {
      __atomic_store_n(&shared_->vfork_ready, 1, __ATOMIC_RELEASE);
      char byte;
      ssize_t count;
      do { count = read(reader.value, &byte, 1); } while (count < 0 && errno == EINTR);
      if (count != 1) _exit(3);
      __atomic_store_n(&shared_->vfork_child_ran, 1, __ATOMIC_RELEASE);
      _exit(0);
    }
    if (child < 0) _exit(4);
    int status;
    while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    for (;;) __atomic_fetch_add(&shared_->count, 1, __ATOMIC_RELEASE);
  }
  ASSERT_EQ(0, setpgid(worker_, worker_));
  worker_group_ = true;
  ASSERT_TRUE(Until([&] {
    return __atomic_load_n(&shared_->vfork_ready, __ATOMIC_ACQUIRE) == 1;
  }));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(1, write(writer.value, "X", 1));
  Stationary();
  EXPECT_EQ(0u, __atomic_load_n(&shared_->vfork_child_ran, __ATOMIC_ACQUIRE));
  EXPECT_EQ(0u, Count());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
  EXPECT_EQ(1u, __atomic_load_n(&shared_->vfork_child_ran, __ATOMIC_ACQUIRE));
}

struct CloneArgs {
  uint64_t flags, pidfd, child_tid, parent_tid, exit_signal, stack, stack_size;
  uint64_t tls, set_tid, set_tid_size, cgroup;
};

TEST_F(Freezer, CloneIntoFrozenCgroupCannotExecuteFirstUserInstruction) {
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ScopedFd group(open(root_.c_str(), O_DIRECTORY | O_RDONLY | O_CLOEXEC));
  ASSERT_GE(group.value, 0);
  CloneArgs args{};
  args.flags = uint64_t{1} << 33;
  args.exit_signal = SIGCHLD;
  args.cgroup = group.value;
  worker_ = syscall(SYS_clone3, &args, sizeof(args));
  if (worker_ == 0) {
    for (;;) __atomic_fetch_add(&shared_->count, 1, __ATOMIC_RELEASE);
  }
  ASSERT_GT(worker_, 0) << strerror(errno);
  Frozen(root_, true);
  ASSERT_FALSE(HasFatalFailure());
  Stationary();
  EXPECT_EQ(0u, Count());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
}

TEST_F(Freezer, ThreadMigrationFreezesOnlyMovedThread) {
  std::string running = Make(root_ + "/running");
  std::string frozen = Make(root_ + "/frozen");
  ASSERT_EQ(0, Write(running + "/cgroup.type", "threaded"));
  ASSERT_EQ(0, Write(frozen + "/cgroup.type", "threaded"));
  Start(running, true);
  ASSERT_FALSE(HasFatalFailure());
  int tid = __atomic_load_n(&shared_->thread_tid, __ATOMIC_ACQUIRE);
  ASSERT_GT(tid, 0);
  ASSERT_EQ(0, Write(frozen + "/cgroup.freeze", "1"));
  ASSERT_EQ(0, Write(frozen + "/cgroup.threads", std::to_string(tid)));
  Frozen(frozen, true);
  ASSERT_FALSE(HasFatalFailure());
  uint64_t count = __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE);
  Progress();
  uint64_t deadline = Now() + 100000000;
  do {
    ASSERT_EQ(count, __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE));
    Pause();
  } while (Now() < deadline);
  ASSERT_EQ(0, Write(running + "/cgroup.threads", std::to_string(tid)));
  ASSERT_TRUE(Until([&] {
    return __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE) > count + 1000;
  }));
}

TEST_F(Freezer, EventsInotifyAndEpollReportFreezeCompletion) {
  ScopedFd file(open((root_ + "/cgroup.events").c_str(), O_RDONLY | O_CLOEXEC));
  ScopedFd notifications(inotify_init1(IN_NONBLOCK | IN_CLOEXEC));
  ScopedFd epoll(epoll_create1(EPOLL_CLOEXEC));
  ASSERT_GE(file.value, 0);
  ASSERT_GE(notifications.value, 0);
  ASSERT_GE(epoll.value, 0);
  int watch = inotify_add_watch(notifications.value, (root_ + "/cgroup.events").c_str(), IN_MODIFY);
  ASSERT_GE(watch, 0);
  alignas(inotify_event) char bytes[512];
  ASSERT_GT(read(file.value, bytes, sizeof(bytes)), 0);
  epoll_event entry{};
  entry.events = EPOLLPRI;
  entry.data.fd = file.value;
  ASSERT_EQ(0, epoll_ctl(epoll.value, EPOLL_CTL_ADD, file.value, &entry));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  epoll_event result{};
  ASSERT_EQ(1, epoll_wait(epoll.value, &result, 1, 5000));
  EXPECT_NE(0u, result.events & EPOLLPRI);
  pollfd notification{notifications.value, POLLIN, 0};
  ASSERT_EQ(1, poll(&notification, 1, 5000));
  ssize_t size = read(notifications.value, bytes, sizeof(bytes));
  ASSERT_GE(size, ssize_t(sizeof(inotify_event)));
  auto* event = reinterpret_cast<const inotify_event*>(bytes);
  EXPECT_EQ(watch, event->wd);
  EXPECT_NE(0u, event->mask & IN_MODIFY);
  Frozen(root_, true);
}

TEST_F(Freezer, RemovedDomainEventsDescriptorReportsOffline) {
  std::string child = Make(root_ + "/removed");
  ASSERT_FALSE(HasFatalFailure());
  ScopedFd file(open((child + "/cgroup.events").c_str(), O_RDONLY | O_CLOEXEC));
  ScopedFd epoll(epoll_create1(EPOLL_CLOEXEC));
  ASSERT_GE(file.value, 0);
  ASSERT_GE(epoll.value, 0);
  char bytes[128];
  ASSERT_GT(read(file.value, bytes, sizeof(bytes)), 0);
  epoll_event entry{};
  entry.events = EPOLLPRI;
  entry.data.fd = file.value;
  ASSERT_EQ(0, epoll_ctl(epoll.value, EPOLL_CTL_ADD, file.value, &entry));
  epoll_event result{};
  ASSERT_EQ(0, epoll_wait(epoll.value, &result, 1, 0));
  ASSERT_EQ(0, rmdir(child.c_str()));
  dirs_.pop_back();
  // Linux does not notify an already sleeping epoll observer on removal.
  // A fresh poll revalidates the retained descriptor and sees it offline.
  pollfd probe{file.value, POLLPRI, 0};
  ASSERT_EQ(1, poll(&probe, 1, 0));
  EXPECT_NE(0, probe.revents & POLLPRI);
  EXPECT_NE(0, probe.revents & POLLERR);
  EXPECT_EQ(-1, read(file.value, bytes, sizeof(bytes)));
  EXPECT_EQ(ENODEV, errno);
}

TEST_F(Freezer, RemovedThreadedEventsDescriptorReportsOffline) {
  ASSERT_EQ(0, Write(root_ + "/cgroup.type", "threaded"));
  ScopedFd file(open((root_ + "/cgroup.events").c_str(), O_RDONLY | O_CLOEXEC));
  ScopedFd epoll(epoll_create1(EPOLL_CLOEXEC));
  ASSERT_GE(file.value, 0);
  ASSERT_GE(epoll.value, 0);
  char bytes[128];
  ASSERT_GT(read(file.value, bytes, sizeof(bytes)), 0);
  epoll_event entry{};
  entry.events = EPOLLPRI;
  entry.data.fd = file.value;
  ASSERT_EQ(0, epoll_ctl(epoll.value, EPOLL_CTL_ADD, file.value, &entry));
  epoll_event result{};
  ASSERT_EQ(0, epoll_wait(epoll.value, &result, 1, 0));
  ASSERT_EQ(0, rmdir(root_.c_str()));
  dirs_.pop_back();
  pollfd probe{file.value, POLLPRI, 0};
  ASSERT_EQ(1, poll(&probe, 1, 0));
  EXPECT_NE(0, probe.revents & POLLPRI);
  EXPECT_NE(0, probe.revents & POLLERR);
  EXPECT_EQ(-1, read(file.value, bytes, sizeof(bytes)));
  EXPECT_EQ(ENODEV, errno);
}

uint64_t CpuTicks(pid_t task) {
  std::string status = Read("/proc/" + std::to_string(task) + "/stat");
  size_t close = status.rfind(')');
  if (close == std::string::npos) return UINT64_MAX;
  std::istringstream fields(status.substr(close + 2));
  std::string field;
  uint64_t total = 0;
  for (int index = 3; index <= 15; ++index) {
    if (!(fields >> field)) return UINT64_MAX;
    if (index == 14 || index == 15) total += std::strtoull(field.c_str(), nullptr, 10);
  }
  return total;
}

TEST_F(Freezer, PendingOrdinarySignalDoesNotSpinFrozenTaskOnCpu) {
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  ASSERT_EQ(0, kill(worker_, SIGUSR1));
  uint64_t before = CpuTicks(worker_);
  ASSERT_NE(UINT64_MAX, before);
  uint64_t deadline = Now() + 500000000;
  while (Now() < deadline) Pause();
  uint64_t after = CpuTicks(worker_);
  ASSERT_NE(UINT64_MAX, after);
  // At most one accounting quantum for the signal wake and re-parking;
  // sustained scheduling/spinning must not consume the observation window.
  EXPECT_LE(after - before, 1u);
  EXPECT_EQ(0u, __atomic_load_n(&shared_->signals, __ATOMIC_ACQUIRE));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Progress();
}

TEST_F(Freezer, ElevenHundredLevelHierarchyPropagatesAndDeletesIteratively) {
  std::string last = root_;
  for (int level = 0; level < 1100; ++level) {
    last = Make(last + "/a");
    ASSERT_FALSE(HasFatalFailure()) << "level " << level;
  }
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
  Frozen(root_, true);
  Frozen(last, true);
  EXPECT_EQ("0\n", Read(last + "/cgroup.freeze"));
  ASSERT_EQ(0, Write(last + "/cgroup.freeze", "1"));
  ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
  Frozen(root_, false);
  Frozen(last, true);
  ASSERT_EQ(0, Write(last + "/cgroup.freeze", "0"));
  Frozen(last, false);
  // TearDown removes every level deepest first, retaining full test depth.
}

TEST_F(Freezer, ThreadCreationRacingGroupStopStillCompletesFreeze) {
  worker_ = fork();
  ASSERT_GE(worker_, 0);
  if (worker_ == 0) {
    alarm(120);
    unsigned completed = 0;
    for (;;) {
      __atomic_fetch_add(&shared_->count, 1, __ATOMIC_RELEASE);
      unsigned request = __atomic_load_n(&shared_->create_request, __ATOMIC_ACQUIRE);
      if (request == completed) continue;
      __atomic_store_n(&shared_->create_started, request, __ATOMIC_RELEASE);
      pthread_t threads[8];
      unsigned created = 0;
      int error = 0;
      for (; created < 8; ++created) {
        error = pthread_create(&threads[created], nullptr, CreationRaceThread, shared_);
        if (error) break;
      }
      for (unsigned index = 0; index < created; ++index) {
        int join_error = pthread_join(threads[index], nullptr);
        if (!error) error = join_error;
      }
      __atomic_store_n(&shared_->create_error, error, __ATOMIC_RELEASE);
      completed = request;
      __atomic_store_n(&shared_->create_done, completed, __ATOMIC_RELEASE);
    }
  }
  ASSERT_EQ(0, Write(root_ + "/cgroup.procs", std::to_string(worker_)));
  ASSERT_TRUE(Until([&] { return Count() > 1000; }));
  for (unsigned iteration = 1; iteration <= 32; ++iteration) {
    __atomic_store_n(&shared_->create_request, iteration, __ATOMIC_RELEASE);
    uint64_t deadline = Now() + 5000000000;
    while (__atomic_load_n(&shared_->create_started, __ATOMIC_ACQUIRE) != iteration &&
           Now() < deadline) sched_yield();
    ASSERT_EQ(iteration, __atomic_load_n(&shared_->create_started, __ATOMIC_ACQUIRE));
    // The handshake starts a batch of eight real CLONE_THREAD creations.
    // SIGSTOP races that batch; the exact born-stopped kernel branch is not
    // deterministically selected by a userspace test, even over 32 rounds.
    ASSERT_EQ(0, kill(worker_, SIGSTOP));
    int status = 0;
    ASSERT_TRUE(Until([&] {
      return waitpid(worker_, &status, WNOHANG | WUNTRACED) == worker_;
    }));
    ASSERT_TRUE(WIFSTOPPED(status));
    ASSERT_EQ(SIGSTOP, WSTOPSIG(status));
    // A completed group-stop must not leave a stopped creator holding the
    // cgroup mutation lock. Both reads and writes below must remain usable.
    EXPECT_NE(std::string::npos,
              Read(root_ + "/cgroup.procs").find(std::to_string(worker_)));
    EXPECT_EQ(1, Event(root_, "populated"));
    ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
    Frozen(root_, true);
    ASSERT_FALSE(HasFatalFailure()) << "iteration " << iteration;
    uint64_t thread_count = __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE);
    ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
    Frozen(root_, false);
    ASSERT_FALSE(HasFatalFailure());
    Stationary();
    EXPECT_EQ(thread_count, __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE));
    ASSERT_EQ(0, kill(worker_, SIGCONT));
    ASSERT_TRUE(Until([&] {
      return __atomic_load_n(&shared_->create_done, __ATOMIC_ACQUIRE) == iteration;
    }));
    ASSERT_EQ(0, __atomic_load_n(&shared_->create_error, __ATOMIC_ACQUIRE));
    EXPECT_EQ(iteration * 8, __atomic_load_n(&shared_->thread_count, __ATOMIC_ACQUIRE));
    Progress();
  }
}

TEST_F(Freezer, NonLeaderExecRacingFreezeCannotEscapeAfterAcknowledgement) {
  for (unsigned iteration = 0; iteration < 32; ++iteration) {
    __atomic_store_n(&shared_->exec_ready, 0, __ATOMIC_RELEASE);
    __atomic_store_n(&shared_->exec_request, 0, __ATOMIC_RELEASE);
    int pipefd[2];
    ASSERT_EQ(0, pipe(pipefd));
    ScopedFd reader(pipefd[0]), writer(pipefd[1]);
    ASSERT_EQ(0, fcntl(reader.value, F_SETFL, O_NONBLOCK));
    worker_ = fork();
    ASSERT_GE(worker_, 0);
    if (worker_ == 0) {
      close(reader.value);
      alarm(120);
      ExecThreadArgs args{};
      args.shared = shared_;
      std::snprintf(args.descriptor, sizeof(args.descriptor), "%d", writer.value);
      pthread_t thread;
      if (pthread_create(&thread, nullptr, NonLeaderExecThread, &args) != 0) _exit(91);
      for (;;) sched_yield();
    }
    close(writer.value);
    writer.value = -1;
    ASSERT_EQ(0, Write(root_ + "/cgroup.procs", std::to_string(worker_)));
    ASSERT_TRUE(Until([&] {
      return __atomic_load_n(&shared_->exec_ready, __ATOMIC_ACQUIRE) == 1;
    }));
    // Release a real nonleader exec while requesting freeze. This stresses
    // PID exchange/publication; userspace cannot force the exact lock race.
    __atomic_store_n(&shared_->exec_request, 1, __ATOMIC_RELEASE);
    ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "1"));
    Frozen(root_, true);
    ASSERT_FALSE(HasFatalFailure()) << "iteration " << iteration;
    char bytes[4096];
    bool drained = false;
    for (unsigned attempt = 0; attempt < 64; ++attempt) {
      ssize_t size = read(reader.value, bytes, sizeof(bytes));
      if (size < 0 && errno == EINTR) continue;
      if (size < 0 && errno == EAGAIN) {
        drained = true;
        break;
      }
      ASSERT_GT(size, 0) << "exec worker unexpectedly exited";
    }
    ASSERT_TRUE(drained) << "worker continued filling pipe after frozen=1";
    pollfd ready{reader.value, POLLIN, 0};
    EXPECT_EQ(0, poll(&ready, 1, 100)) << "exec escaped frozen cgroup";
    ASSERT_EQ(0, Write(root_ + "/cgroup.freeze", "0"));
    Frozen(root_, false);
    ASSERT_EQ(1, poll(&ready, 1, 5000));
    const ssize_t resumed = read(reader.value, bytes, sizeof(bytes));
    const int read_error = errno;
    ASSERT_GT(resumed, 0) << "iteration=" << iteration << " revents=" << ready.revents
                          << " errno=" << read_error << " (" << strerror(read_error) << ")";
    ASSERT_EQ(0, kill(worker_, SIGKILL));
    int status = 0;
    ASSERT_TRUE(Until([&] {
      return waitpid(worker_, &status, WNOHANG) == worker_ &&
             (WIFEXITED(status) || WIFSIGNALED(status));
    }));
    worker_ = -1;
    ASSERT_TRUE(WIFSIGNALED(status));
    ASSERT_EQ(SIGKILL, WTERMSIG(status));
  }
}
}  // namespace

int main(int argc, char** argv) {
  if (argc == 3 && !std::strcmp(argv[1], "--freezer-exec-worker")) {
    int fd = std::atoi(argv[2]);
    const char marker[] = "exec-user-work\n";
    for (;;) {
      ssize_t size = write(fd, marker, sizeof(marker) - 1);
      if (size < 0 && errno == EINTR) continue;
      if (size != ssize_t(sizeof(marker) - 1)) _exit(93);
    }
  }
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
