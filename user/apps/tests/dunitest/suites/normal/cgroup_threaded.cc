#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/sched.h>
#include <linux/futex.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <sys/socket.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <time.h>

#include <algorithm>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <set>
#include <sstream>
#include <string>
#include <vector>

namespace {

std::string Read(const std::string& path, int* error = nullptr) {
  int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
  if (fd < 0) {
    if (error) *error = errno;
    return "";
  }
  std::string text;
  char bytes[4096];
  ssize_t n;
  while ((n = read(fd, bytes, sizeof(bytes))) > 0) text.append(bytes, n);
  if (error) *error = n < 0 ? errno : 0;
  close(fd);
  while (!text.empty() && text.back() == '\n') text.pop_back();
  return text;
}

int WriteFd(int fd, const std::string& value) {
  ssize_t n = write(fd, value.data(), value.size());
  return n < 0 ? errno : (n == static_cast<ssize_t>(value.size()) ? 0 : EIO);
}

int Write(const std::string& path, const std::string& value) {
  int fd = open(path.c_str(), O_WRONLY | O_CLOEXEC);
  if (fd < 0) return errno;
  int error = WriteFd(fd, value);
  close(fd);
  return error;
}

std::set<pid_t> Members(const std::string& path) {
  std::istringstream stream(Read(path));
  std::set<pid_t> result;
  pid_t pid;
  while (stream >> pid) result.insert(pid);
  return result;
}

bool Transfer(int fd, void* data, size_t size, bool send_bytes) {
  auto* bytes = static_cast<char*>(data);
  while (size) {
    pollfd pfd{fd, static_cast<short>(send_bytes ? POLLOUT : POLLIN), 0};
    int ready;
    do {
      ready = poll(&pfd, 1, 10000);
    } while (ready < 0 && errno == EINTR);
    if (ready <= 0) return false;
    ssize_t n = send_bytes ? send(fd, bytes, size, MSG_NOSIGNAL)
                          : read(fd, bytes, size);
    if (n < 0 && errno == EINTR) continue;
    if (n <= 0) return false;
    bytes += n;
    size -= n;
  }
  return true;
}

struct Request {
  char operation;
  char path[512];
};

struct Reply {
  int error;
  pid_t tid;
  int cpu;
  cpu_set_t affinity;
  char cgroup[1024];
};

Reply Inspect() {
  Reply reply{};
  reply.tid = syscall(SYS_gettid);
  if (sched_getaffinity(0, sizeof(reply.affinity), &reply.affinity)) reply.error = errno;
  for (int i = 0; i < 10000 && !reply.error; ++i) {
    reply.cpu = sched_getcpu();
    if (reply.cpu < 0 || !CPU_ISSET(reply.cpu, &reply.affinity)) reply.error = EDOM;
  }
  std::string path = "/proc/self/task/" + std::to_string(reply.tid) + "/cgroup";
  std::snprintf(reply.cgroup, sizeof(reply.cgroup), "%s", Read(path).c_str());
  return reply;
}

void* ThreadLoop(void* opaque) {
  int fd = *static_cast<int*>(opaque);
  Reply reply = Inspect();
  if (!Transfer(fd, &reply, sizeof(reply), true)) _exit(91);
  Request request{};
  while (Transfer(fd, &request, sizeof(request), false)) {
    if (request.operation == 'Q') return nullptr;
    if (request.operation == 'M') {
      reply = Inspect();
      reply.error = Write(std::string(request.path) + "/cgroup.threads", "0");
    } else if (request.operation == 'F') {
      int pair[2];
      if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair)) _exit(92);
      pid_t child = fork();
      int fork_error = child < 0 ? errno : 0;
      if (child == 0) {
        close(pair[0]);
        Reply inherited = Inspect();
        bool ok = Transfer(pair[1], &inherited, sizeof(inherited), true);
        _exit(ok ? 0 : 93);
      }
      close(pair[1]);
      reply = {};
      if (child < 0) reply.error = fork_error;
      else if (!Transfer(pair[0], &reply, sizeof(reply), false)) reply.error = EIO;
      close(pair[0]);
      int status;
      if (child > 0 && (waitpid(child, &status, 0) != child || status != 0)) reply.error = ECHILD;
    } else if (request.operation == 'E') {
      std::string descriptor = std::to_string(fd);
      execl("/proc/self/exe", "cgroup_threaded_test", "--threaded-exec",
            descriptor.c_str(), nullptr);
      reply = {};
      reply.error = errno;
    } else {
      reply = Inspect();
    }
    if (!Transfer(fd, &reply, sizeof(reply), true)) _exit(94);
  }
  return nullptr;
}

class ThreadedCgroup : public ::testing::Test {
 protected:
  void SetUp() override {
    const char* supplied = std::getenv("DUNITEST_CGROUP_PARENT");
    parent_ = supplied ? supplied : "/sys/fs/cgroup";
    root_ = parent_ + "/dunitest-threaded-" + std::to_string(getpid()) + "-" +
            std::to_string(sequence_++);
    ASSERT_EQ(0, mkdir(root_.c_str(), 0755)) << strerror(errno);
    dirs_.push_back(root_);
  }

  void TearDown() override {
    if (worker_ > 0) {
      kill(worker_, SIGKILL);
      int status;
      while (waitpid(worker_, &status, 0) < 0 && errno == EINTR) {}
    }
    for (int fd : sockets_) if (fd >= 0) close(fd);
    for (auto it = dirs_.rbegin(); it != dirs_.rend(); ++it) {
      // A joined thread may still be in the final kernel exit path. Retry
      // only EBUSY with a real monotonic deadline, not an arbitrary sleep.
      timespec start{};
      clock_gettime(CLOCK_MONOTONIC, &start);
      for (;;) {
        if (rmdir(it->c_str()) == 0 || errno == ENOENT) break;
        int error = errno;
        timespec now{};
        clock_gettime(CLOCK_MONOTONIC, &now);
        if (error != EBUSY || now.tv_sec - start.tv_sec >= 10) {
          ADD_FAILURE() << "rmdir " << *it << ": " << strerror(error);
          break;
        }
        sched_yield();
      }
    }
    for (const auto& controller : added_) {
      EXPECT_EQ(0, Write(parent_ + "/cgroup.subtree_control", "-" + controller));
    }
    for (const auto& path : scratch_dirs_) EXPECT_EQ(0, rmdir(path.c_str()));
  }

  std::string Make(const std::string& parent, const char* name) {
    std::string path = parent + "/" + name;
    if (mkdir(path.c_str(), 0755)) {
      ADD_FAILURE() << path << ": " << strerror(errno);
      return "";
    }
    dirs_.push_back(path);
    return path;
  }

  void Enable(const char* controller) {
    std::istringstream stream(Read(parent_ + "/cgroup.subtree_control"));
    bool present = false;
    for (std::string token; stream >> token;) present |= token == controller;
    if (!present) {
      ASSERT_EQ(0, Write(parent_ + "/cgroup.subtree_control", std::string("+") + controller));
      added_.push_back(controller);
    }
  }

  void Topology() {
    a_ = Make(root_, "a");
    b_ = Make(root_, "b");
    ASSERT_EQ(0, Write(a_ + "/cgroup.type", "threaded"));
    ASSERT_EQ(0, Write(b_ + "/cgroup.type", "threaded"));
  }

  void Start(const std::string& group) {
    int pairs[3][2];
    for (auto& pair : pairs) ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
    worker_ = fork();
    ASSERT_GT(worker_, -1);
    if (worker_ == 0) {
      alarm(60);
      for (auto& pair : pairs) close(pair[0]);
      char ready;
      if (!Transfer(pairs[0][1], &ready, 1, false)) _exit(90);
      pthread_t threads[2];
      for (int i = 0; i < 2; ++i) {
        if (pthread_create(&threads[i], nullptr, ThreadLoop, &pairs[i + 1][1])) _exit(95);
      }
      ThreadLoop(&pairs[0][1]);
      for (pthread_t thread : threads) pthread_join(thread, nullptr);
      _exit(0);
    }
    for (int i = 0; i < 3; ++i) {
      close(pairs[i][1]);
      sockets_[i] = pairs[i][0];
    }
    ASSERT_EQ(0, Write(group + "/cgroup.procs", std::to_string(worker_)));
    char ready = 'R';
    ASSERT_TRUE(Transfer(sockets_[0], &ready, 1, true));
    for (int i = 0; i < 3; ++i) {
      Reply reply{};
      ASSERT_TRUE(Transfer(sockets_[i], &reply, sizeof(reply), false));
      tids_[i] = reply.tid;
    }
  }

  Reply Command(int thread, char operation = 'I', const std::string& path = "") {
    Request request{};
    request.operation = operation;
    std::snprintf(request.path, sizeof(request.path), "%s", path.c_str());
    Reply reply{};
    if (!Transfer(sockets_[thread], &request, sizeof(request), true) ||
        !Transfer(sockets_[thread], &reply, sizeof(reply), false)) {
      ADD_FAILURE() << "worker communication failed";
      reply.error = EIO;
    }
    return reply;
  }

  void Move(int thread, const std::string& group) {
    ASSERT_EQ(0, Write(group + "/cgroup.threads", std::to_string(tids_[thread])));
  }

  static unsigned sequence_;
  std::string parent_, root_, a_, b_;
  std::vector<std::string> dirs_, added_;
  std::vector<std::string> scratch_dirs_;
  pid_t worker_ = -1;
  int sockets_[3] = {-1, -1, -1};
  pid_t tids_[3] = {};
};

unsigned ThreadedCgroup::sequence_ = 0;

TEST_F(ThreadedCgroup, TypeConversionInvalidDescendantAndIdempotence) {
  a_ = Make(root_, "a");
  b_ = Make(a_, "b");
  EXPECT_EQ("domain", Read(a_ + "/cgroup.type"));
  EXPECT_EQ(EINVAL, Write(a_ + "/cgroup.type", "domain"));
  ASSERT_EQ(0, Write(a_ + "/cgroup.type", "threaded"));
  EXPECT_EQ("domain threaded", Read(root_ + "/cgroup.type"));
  EXPECT_EQ("domain invalid", Read(b_ + "/cgroup.type"));
  EXPECT_EQ(0, Write(a_ + "/cgroup.type", "threaded"));
  EXPECT_EQ(EINVAL, Write(a_ + "/cgroup.type", "domain"));
  EXPECT_EQ(EOPNOTSUPP, Write(b_ + "/cgroup.procs", std::to_string(getpid())));
  ASSERT_EQ(0, Write(b_ + "/cgroup.type", "threaded"));
  EXPECT_EQ("threaded", Read(b_ + "/cgroup.type"));
}

TEST_F(ThreadedCgroup, PopulatedDescendantPreventsConversion) {
  a_ = Make(root_, "a");
  b_ = Make(a_, "b");
  Start(b_);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ(EOPNOTSUPP, Write(a_ + "/cgroup.type", "threaded"));
  EXPECT_EQ(EOPNOTSUPP, Write(b_ + "/cgroup.type", "threaded"));
  EXPECT_EQ("domain", Read(a_ + "/cgroup.type"));
}

TEST_F(ThreadedCgroup, AncestorConversionRebindsThreadedDescendantAcrossInvalidDomain) {
  Enable("pids");
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  a_ = Make(root_, "a");
  b_ = Make(a_, "b");
  std::string c = Make(b_, "c");
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(a_ + "/cgroup.subtree_control", "+pids"));
  ASSERT_EQ(0, Write(b_ + "/cgroup.subtree_control", "+pids"));
  ASSERT_EQ(0, Write(c + "/cgroup.type", "threaded"));
  ASSERT_EQ(0, Write(a_ + "/cgroup.type", "threaded"));
  EXPECT_EQ("domain threaded", Read(root_ + "/cgroup.type"));
  EXPECT_EQ("domain invalid", Read(b_ + "/cgroup.type"));
  EXPECT_EQ("threaded", Read(c + "/cgroup.type"));
  // A conversion rebinds all existing threaded descendants, not merely
  // descendants connected by an uninterrupted threaded parent chain.
  ASSERT_EQ(0, Write(c + "/cgroup.subtree_control", "+pids"));
  Start(c);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ((std::set<pid_t>{worker_}), Members(root_ + "/cgroup.procs"));
  EXPECT_TRUE(Members(b_ + "/cgroup.procs").empty());
  Move(1, a_);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ(0, Command(1, 'M', c).error);
  EXPECT_EQ(0, Command(1, 'F').error);
  EXPECT_EQ((std::set<pid_t>{worker_}), Members(root_ + "/cgroup.procs"));
  EXPECT_EQ(3u, Members(c + "/cgroup.threads").size());
}

TEST_F(ThreadedCgroup, DelegatedThreadedControllerMigrationAndForkWithoutPrivilege) {
  Enable("cpu");
  Enable("pids");
  a_ = root_ + "/a";
  b_ = root_ + "/b";
  dirs_.push_back(a_);
  dirs_.push_back(b_);
  for (const auto& path : {root_, root_ + "/cgroup.procs",
                           root_ + "/cgroup.subtree_control"}) {
    ASSERT_EQ(0, chown(path.c_str(), 65534, 65534));
  }
  pid_t helper = fork();
  ASSERT_GE(helper, 0);
  if (helper == 0) {
    alarm(15);
    if (Write(root_ + "/cgroup.procs", "0") || setgid(65534) || setuid(65534)) _exit(1);
    if (Write(root_ + "/cgroup.subtree_control", "+cpu +pids")) _exit(2);
    if (mkdir(a_.c_str(), 0755) || mkdir(b_.c_str(), 0755)) _exit(3);
    if (Write(a_ + "/cgroup.type", "threaded") ||
        Write(b_ + "/cgroup.type", "threaded")) _exit(4);
    if (Write(a_ + "/cgroup.procs", "0")) _exit(5);
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair)) _exit(6);
    pthread_t thread;
    if (pthread_create(&thread, nullptr, ThreadLoop, &pair[1])) _exit(7);
    Reply reply{};
    if (!Transfer(pair[0], &reply, sizeof(reply), false)) _exit(8);
    pid_t tid = reply.tid;
    if (reply.error || Write(b_ + "/cgroup.threads", std::to_string(tid))) _exit(9);
    if (Members(b_ + "/cgroup.threads") != std::set<pid_t>{tid} ||
        Members(root_ + "/cgroup.procs") != std::set<pid_t>{getpid()}) _exit(10);
    Request request{};
    request.operation = 'F';
    if (!Transfer(pair[0], &request, sizeof(request), true) ||
        !Transfer(pair[0], &reply, sizeof(reply), false) || reply.error) _exit(11);
    if (Write(b_ + "/pids.max", "0")) _exit(12);
    if (!Transfer(pair[0], &request, sizeof(request), true) ||
        !Transfer(pair[0], &reply, sizeof(reply), false) || reply.error != EAGAIN) _exit(13);
    request.operation = 'Q';
    if (!Transfer(pair[0], &request, sizeof(request), true) || pthread_join(thread, nullptr)) _exit(14);
    close(pair[0]);
    close(pair[1]);
    _exit(0);
  }
  int status;
  ASSERT_EQ(helper, waitpid(helper, &status, 0));
  EXPECT_EQ(0, status);
}

TEST_F(ThreadedCgroup, ThreadRootRestoresAfterLastThreadedChild) {
  Topology();
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, rmdir(a_.c_str()));
  EXPECT_EQ("domain threaded", Read(root_ + "/cgroup.type"));
  ASSERT_EQ(0, rmdir(b_.c_str()));
  EXPECT_EQ("domain", Read(root_ + "/cgroup.type"));
}

TEST_F(ThreadedCgroup, DomainControllerPreventsConversionWithoutChangingState) {
  Enable("memory");
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+memory"));
  a_ = Make(root_, "a");
  EXPECT_EQ(EOPNOTSUPP, Write(a_ + "/cgroup.type", "threaded"));
  EXPECT_EQ("domain", Read(root_ + "/cgroup.type"));
  EXPECT_EQ("domain", Read(a_ + "/cgroup.type"));
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "-memory"));
  ASSERT_EQ(0, Write(a_ + "/cgroup.type", "threaded"));
  EXPECT_EQ("domain threaded", Read(root_ + "/cgroup.type"));
}

TEST_F(ThreadedCgroup, TrueHierarchyRootAllowsMixedPopulatedChildren) {
  std::string sibling = Make(parent_, ("dunitest-threaded-mixed-" +
                                     std::to_string(getpid()) + "-" +
                                     std::to_string(sequence_++)).c_str());
  Start(sibling);
  ASSERT_FALSE(HasFatalFailure());
  int type_error = 0;
  Read(parent_ + "/cgroup.type", &type_error);
  int error = Write(root_ + "/cgroup.type", "threaded");
  // A delegated non-root parent cannot mix populated domain children;
  // absence of cgroup.type identifies the real hierarchy root.
  EXPECT_EQ(type_error == ENOENT ? 0 : EOPNOTSUPP, error);
  EXPECT_EQ("domain", Read(sibling + "/cgroup.type"));
  EXPECT_EQ(3u, Members(sibling + "/cgroup.threads").size());
}

TEST_F(ThreadedCgroup, ListsAggregateProcessesButSeparateThreads) {
  Topology();
  Start(a_);
  Move(1, b_);
  ASSERT_FALSE(HasFatalFailure());
  int error = 0;
  Read(a_ + "/cgroup.procs", &error);
  EXPECT_EQ(EOPNOTSUPP, error);
  EXPECT_EQ((std::set<pid_t>{worker_}), Members(root_ + "/cgroup.procs"));
  EXPECT_EQ((std::set<pid_t>{tids_[0], tids_[2]}), Members(a_ + "/cgroup.threads"));
  EXPECT_EQ((std::set<pid_t>{tids_[1]}), Members(b_ + "/cgroup.threads"));
  EXPECT_TRUE(Members(root_ + "/cgroup.threads").empty());
}

TEST_F(ThreadedCgroup, ProcsSameLeafStillGathersEntireThreadGroup) {
  Topology();
  Start(a_);
  Move(1, b_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(a_ + "/cgroup.procs", std::to_string(tids_[2])));
  EXPECT_EQ((std::set<pid_t>{tids_[0], tids_[1], tids_[2]}), Members(a_ + "/cgroup.threads"));
  EXPECT_TRUE(Members(b_ + "/cgroup.threads").empty());
}

TEST_F(ThreadedCgroup, SingleThreadMigrationDoesNotMoveLeader) {
  Topology();
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  for (int i = 0; i < 100; ++i) {
    const std::string& target = i % 2 ? a_ : b_;
    EXPECT_EQ(0, Command(1, 'M', target).error);
    EXPECT_NE(std::string::npos, std::string(Command(1).cgroup).find(target.substr(parent_.size())));
    EXPECT_TRUE(Members(a_ + "/cgroup.threads").count(tids_[0]));
  }
}

TEST_F(ThreadedCgroup, CrossDomainThreadRejectedProcessAllowed) {
  Topology();
  std::string other = Make(parent_, ("dunitest-threaded-other-" + std::to_string(getpid()) + "-" +
                          std::to_string(sequence_++)).c_str());
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ(EOPNOTSUPP, Write(other + "/cgroup.threads", std::to_string(tids_[1])));
  EXPECT_EQ(3u, Members(a_ + "/cgroup.threads").size());
  ASSERT_EQ(0, Write(other + "/cgroup.procs", std::to_string(tids_[1])));
  EXPECT_EQ(3u, Members(other + "/cgroup.threads").size());
  EXPECT_TRUE(Members(a_ + "/cgroup.threads").empty());
}

TEST_F(ThreadedCgroup, ForkFromNonLeaderInheritsItsLeaf) {
  Topology();
  Start(a_);
  Move(1, b_);
  ASSERT_FALSE(HasFatalFailure());
  Reply reply = Command(1, 'F');
  EXPECT_EQ(0, reply.error);
  EXPECT_NE(std::string::npos, std::string(reply.cgroup).find(b_.substr(parent_.size())));
  EXPECT_TRUE(Members(a_ + "/cgroup.threads").count(tids_[0]));
}

TEST_F(ThreadedCgroup, PidsOverLimitMigrationAndForkAdmission) {
  Enable("pids");
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  Topology();
  ASSERT_EQ(0, Write(b_ + "/pids.max", "1"));
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, Write(b_ + "/cgroup.procs", std::to_string(worker_)));
  EXPECT_EQ("3", Read(root_ + "/pids.current"));
  EXPECT_EQ("3", Read(b_ + "/pids.current"));
  EXPECT_EQ("0", Read(a_ + "/pids.current"));
  // Fork failure is reported by the child worker rather than inferred from
  // pids.current: this verifies real controller enforcement.
  Reply reply = Command(1, 'F');
  EXPECT_EQ(EAGAIN, reply.error);
  EXPECT_EQ("3", Read(root_ + "/pids.current"));
}

TEST_F(ThreadedCgroup, ThreadedControllersAllowInternalTasksDomainControllersDoNot) {
  Enable("pids");
  Enable("memory");
  Topology();
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  EXPECT_EQ(EOPNOTSUPP, Write(root_ + "/cgroup.subtree_control", "+memory"));
  EXPECT_EQ(0, Write(a_ + "/cgroup.subtree_control", "+pids"));
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ("threaded", Read(a_ + "/cgroup.type"));
  EXPECT_EQ(0, Write(a_ + "/cgroup.subtree_control", "+pids"));
}

TEST_F(ThreadedCgroup, OpenerCredentialsGovernCommonAncestorPermission) {
  Topology();
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  ASSERT_EQ(0, chown((b_ + "/cgroup.threads").c_str(), 65534, -1));
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    alarm(10);
    if (seteuid(65534)) _exit(1);
    int fd = open((b_ + "/cgroup.threads").c_str(), O_WRONLY);
    if (fd < 0) _exit(2);
    if (seteuid(0)) _exit(3);
    int error = WriteFd(fd, std::to_string(tids_[1]));
    close(fd);
    _exit(error == EACCES ? 0 : 4);
  }
  int status;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  EXPECT_EQ(0, status);
  EXPECT_TRUE(Members(a_ + "/cgroup.threads").count(tids_[1]));
}

TEST_F(ThreadedCgroup, RemovedNodeOldFileCannotBeRevived) {
  Topology();
  int fd = open((a_ + "/cgroup.threads").c_str(), O_WRONLY | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  ASSERT_EQ(0, rmdir(a_.c_str()));
  EXPECT_EQ(ENODEV, WriteFd(fd, "0"));
  close(fd);
  ASSERT_EQ(0, mkdir(a_.c_str(), 0755));
  EXPECT_EQ("domain invalid", Read(a_ + "/cgroup.type"));
}

TEST_F(ThreadedCgroup, NonLeaderExecRetainsExecutingLeafAndReleasesOtherThreads) {
  Enable("pids");
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  Topology();
  Start(a_);
  Move(1, b_);
  ASSERT_FALSE(HasFatalFailure());
  Reply reply = Command(1, 'E');
  EXPECT_EQ(0, reply.error);
  EXPECT_EQ(worker_, reply.tid);
  EXPECT_NE(std::string::npos, std::string(reply.cgroup).find(b_.substr(parent_.size())));
  int status;
  ASSERT_EQ(worker_, waitpid(worker_, &status, 0));
  EXPECT_EQ(0, status);
  worker_ = -1;
  EXPECT_TRUE(Members(a_ + "/cgroup.threads").empty());
  EXPECT_TRUE(Members(b_ + "/cgroup.threads").empty());
  EXPECT_EQ("0", Read(root_ + "/pids.current"));
}

TEST_F(ThreadedCgroup, CpusetConstrainsEachThreadAndProcessGather) {
  Enable("cpuset");
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+cpuset"));
  Topology();
  cpu_set_t allowed;
  ASSERT_EQ(0, sched_getaffinity(0, sizeof(allowed), &allowed));
  std::vector<int> cpus;
  for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) if (CPU_ISSET(cpu, &allowed)) cpus.push_back(cpu);
  ASSERT_FALSE(cpus.empty());
  int first = cpus.front(), second = cpus.size() > 1 ? cpus[1] : first;
  ASSERT_EQ(0, Write(a_ + "/cpuset.cpus", std::to_string(first)));
  ASSERT_EQ(0, Write(b_ + "/cpuset.cpus", std::to_string(second)));
  Start(a_);
  Move(1, b_);
  ASSERT_FALSE(HasFatalFailure());
  for (int i = 0; i < 3; ++i) {
    Reply reply = Command(i);
    EXPECT_EQ(0, reply.error);
    EXPECT_EQ(i == 1 ? second : first, reply.cpu);
    EXPECT_EQ(1, CPU_COUNT(&reply.affinity));
  }
  ASSERT_EQ(0, Write(b_ + "/cgroup.procs", std::to_string(tids_[1])));
  for (int i = 0; i < 3; ++i) {
    Reply reply = Command(i);
    EXPECT_EQ(0, reply.error);
    EXPECT_EQ(second, reply.cpu);
    EXPECT_EQ(1, CPU_COUNT(&reply.affinity));
  }
}

TEST_F(ThreadedCgroup, ThreadRootWithoutChildrenRestoresWhenTasksOrControllerDisappear) {
  Enable("pids");
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  Start(root_);
  ASSERT_FALSE(HasFatalFailure());
  EXPECT_EQ("domain threaded", Read(root_ + "/cgroup.type"));
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "-pids"));
  EXPECT_EQ("domain", Read(root_ + "/cgroup.type"));
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  EXPECT_EQ("domain threaded", Read(root_ + "/cgroup.type"));
  ASSERT_EQ(0, Write(parent_ + "/cgroup.procs", std::to_string(worker_)));
  EXPECT_EQ("domain", Read(root_ + "/cgroup.type"));
}

TEST_F(ThreadedCgroup, ConcurrentForkExitAndThreadMigrationPreservesAccounting) {
  Enable("pids");
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  Topology();
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  for (int i = 0; i < 100; ++i) {
    Request request{};
    request.operation = 'F';
    ASSERT_TRUE(Transfer(sockets_[1], &request, sizeof(request), true));
    // The worker forks and exits a child while the parent changes that
    // worker's cgroup. Both inheritance outcomes are legal, but charging
    // after child reap must return to the original three live threads.
    Move(1, i % 2 ? a_ : b_);
    Reply reply{};
    ASSERT_TRUE(Transfer(sockets_[1], &reply, sizeof(reply), false));
    ASSERT_EQ(0, reply.error);
    EXPECT_EQ("3", Read(root_ + "/pids.current"));
    EXPECT_EQ(3u, Members(a_ + "/cgroup.threads").size() +
                      Members(b_ + "/cgroup.threads").size());
  }
  ASSERT_EQ(0, Write(a_ + "/cgroup.procs", std::to_string(tids_[0])));
  EXPECT_EQ("3", Read(a_ + "/pids.current"));
  EXPECT_EQ("0", Read(b_ + "/pids.current"));
}

TEST_F(ThreadedCgroup, PidNamespaceProjectsOutsideTasksToZero) {
  Topology();
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  int pair[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    close(pair[0]);
    alarm(15);
    Reply reply{};
    if (unshare(CLONE_NEWPID)) {
      reply.error = errno;
      Transfer(pair[1], &reply, sizeof(reply), true);
      _exit(1);
    }
    pid_t inner = fork();
    if (inner == 0) {
      reply.error = Write(b_ + "/cgroup.procs", "0");
      std::snprintf(reply.cgroup, sizeof(reply.cgroup), "procs=[%s] a=[%s] b=[%s]",
                    Read(root_ + "/cgroup.procs").c_str(),
                    Read(a_ + "/cgroup.threads").c_str(),
                    Read(b_ + "/cgroup.threads").c_str());
      if (!reply.error && Members(root_ + "/cgroup.procs") != std::set<pid_t>{0, 1}) reply.error = EDOM;
      if (!reply.error && Members(b_ + "/cgroup.threads") != std::set<pid_t>{1}) reply.error = ERANGE;
      if (!reply.error && Members(a_ + "/cgroup.threads") != std::set<pid_t>{0}) reply.error = ESRCH;
      reply.tid = getpid();
      Transfer(pair[1], &reply, sizeof(reply), true);
      _exit(0);
    }
    int status;
    _exit(inner > 0 && waitpid(inner, &status, 0) == inner && status == 0 ? 0 : 2);
  }
  close(pair[1]);
  Reply reply{};
  EXPECT_TRUE(Transfer(pair[0], &reply, sizeof(reply), false));
  close(pair[0]);
  int status;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  EXPECT_EQ(0, status);
  EXPECT_EQ(0, reply.error) << strerror(reply.error) << ": " << reply.cgroup;
  EXPECT_EQ(1, reply.tid);
}

bool PassFd(int channel, int fd) {
  char byte = 'F';
  iovec iov{&byte, 1};
  alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))]{};
  msghdr message{};
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = control;
  message.msg_controllen = sizeof(control);
  cmsghdr* cmsg = CMSG_FIRSTHDR(&message);
  cmsg->cmsg_level = SOL_SOCKET;
  cmsg->cmsg_type = SCM_RIGHTS;
  cmsg->cmsg_len = CMSG_LEN(sizeof(int));
  std::memcpy(CMSG_DATA(cmsg), &fd, sizeof(fd));
  return sendmsg(channel, &message, MSG_NOSIGNAL) == 1;
}

int ReceiveFd(int channel) {
  pollfd pfd{channel, POLLIN, 0};
  if (poll(&pfd, 1, 10000) <= 0) return -1;
  char byte;
  iovec iov{&byte, 1};
  alignas(cmsghdr) char control[CMSG_SPACE(sizeof(int))]{};
  msghdr message{};
  message.msg_iov = &iov;
  message.msg_iovlen = 1;
  message.msg_control = control;
  message.msg_controllen = sizeof(control);
  if (recvmsg(channel, &message, 0) != 1) return -1;
  cmsghdr* cmsg = CMSG_FIRSTHDR(&message);
  if (!cmsg || cmsg->cmsg_level != SOL_SOCKET || cmsg->cmsg_type != SCM_RIGHTS ||
      cmsg->cmsg_len != CMSG_LEN(sizeof(int))) return -1;
  int fd;
  std::memcpy(&fd, CMSG_DATA(cmsg), sizeof(fd));
  return fd;
}

std::string MountOptions(const std::string& path) {
  std::istringstream lines(Read("/proc/mounts"));
  std::string line;
  while (std::getline(lines, line)) {
    std::istringstream fields(line);
    std::string source, target, type, options;
    if (fields >> source >> target >> type >> options && target == path && type == "cgroup2") {
      return options;
    }
  }
  return "";
}

bool MountUsesNsdelegate(const std::string& path) {
  return ("," + MountOptions(path) + ",").find(",nsdelegate,") != std::string::npos;
}

unsigned long MountRestoreFlags(const std::string& path) {
  std::string options = "," + MountOptions(path) + ",";
  unsigned long flags = MS_REMOUNT;
  const struct { const char* option; unsigned long flag; } known[] = {
    {"ro", MS_RDONLY}, {"nosuid", MS_NOSUID}, {"nodev", MS_NODEV},
    {"noexec", MS_NOEXEC}, {"noatime", MS_NOATIME}, {"nodiratime", MS_NODIRATIME},
    {"relatime", MS_RELATIME}, {"strictatime", MS_STRICTATIME},
    {"sync", MS_SYNCHRONOUS}, {"dirsync", MS_DIRSYNC},
  };
  for (const auto& item : known) {
    if (options.find("," + std::string(item.option) + ",") != std::string::npos) flags |= item.flag;
  }
  return flags;
}

TEST_F(ThreadedCgroup, NsdelegateRootWriteProtectionUsesOpenerNamespace) {
  Enable("cpu");
  Enable("pids");
  Enable("cpuset");
  char location[] = "/tmp/dunitest-threaded-delegation-XXXXXX";
  ASSERT_NE(nullptr, mkdtemp(location));
  scratch_dirs_.push_back(location);
  char child_view[] = "/tmp/dunitest-threaded-child-view-XXXXXX";
  ASSERT_NE(nullptr, mkdtemp(child_view));
  scratch_dirs_.push_back(child_view);
  bool delegated_before = MountUsesNsdelegate("/sys/fs/cgroup");
  unsigned long restore_flags = MountRestoreFlags("/sys/fs/cgroup");
  int pair[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
  pid_t helper = fork();
  ASSERT_GE(helper, 0);
  if (helper == 0) {
    close(pair[0]);
    alarm(20);
    if (unshare(CLONE_NEWNS) || mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr)) _exit(1);
    if (mount("cgroup2", location, "cgroup2", 0, "nsdelegate")) _exit(2);
    std::string mirror = std::string(location) + root_.substr(std::strlen("/sys/fs/cgroup"));
    int initial_fd = open((mirror + "/cpu.max").c_str(), O_WRONLY | O_CLOEXEC);
    // Every path after the successful mount must restore the hierarchy
    // policy, including failure to open the initial-namespace descriptor.
    pid_t child = initial_fd >= 0 ? fork() : -1;
    if (child == 0) {
      if (Write(mirror + "/cgroup.procs", "0") || unshare(CLONE_NEWCGROUP)) _exit(4);
      if (mount(nullptr, location, "cgroup2", MS_REMOUNT, nullptr) ||
          !MountUsesNsdelegate(location) || !MountUsesNsdelegate("/sys/fs/cgroup")) _exit(12);
      if (mount("cgroup2", child_view, "cgroup2", 0, nullptr) ||
          !MountUsesNsdelegate(child_view) ||
          Write(std::string(child_view) + "/cpu.max", "max 100000") != EPERM ||
          umount(child_view)) _exit(13);
      for (const auto& file : {"cgroup.type", "cpu.max", "pids.max", "cgroup.freeze",
                               "cpuset.cpus", "cpuset.mems"}) {
        const char* value = !std::strcmp(file, "cgroup.type") ? "threaded" :
                            !std::strcmp(file, "cpu.max") ? "max 100000" :
                            !std::strcmp(file, "pids.max") ? "max" : "0";
        if (Write(mirror + "/" + file, value) != EPERM) _exit(5);
        // Delegation denial precedes input parsing, including cpuset writes.
        if (Write(mirror + "/" + file, std::string(1, '\xff')) != EPERM) _exit(16);
      }
      // Both a different mount and a fd passed between namespaces must
      // retain the hierarchy policy and the namespace recorded at open.
      if (Write(root_ + "/cpu.max", "max 100000") != EPERM) _exit(6);
      if (Write(mirror + "/cgroup.subtree_control", "+cpu +pids") ||
          Write(mirror + "/cgroup.procs", "0") ||
          Write(mirror + "/cgroup.threads", "0")) _exit(7);
      if (WriteFd(initial_fd, "max 100000")) _exit(8);
      close(initial_fd);
      int restricted_fd = open((mirror + "/cpu.max").c_str(), O_WRONLY | O_CLOEXEC);
      if (restricted_fd < 0 || WriteFd(restricted_fd, "") || !PassFd(pair[1], restricted_fd)) _exit(9);
      char done;
      bool ok = Transfer(pair[1], &done, 1, false);
      close(restricted_fd);
      _exit(ok ? 0 : 10);
    }
    close(initial_fd);
    int status = 0;
    bool child_ok = child > 0 && waitpid(child, &status, 0) == child && status == 0;
    // nsdelegate is hierarchy-wide in Linux, so restore the original policy
    // while this helper still belongs to the initial cgroup namespace.
    int restored = mount(nullptr, location, "cgroup2", MS_REMOUNT,
                         delegated_before ? "nsdelegate" : nullptr);
    bool views_restored = MountUsesNsdelegate(location) == delegated_before &&
                          MountUsesNsdelegate("/sys/fs/cgroup") == delegated_before;
    // A noninitial namespace cannot re-enable a policy which the initial
    // namespace disabled, even by explicitly requesting nsdelegate.
    pid_t requester = child_ok && restored == 0 ? fork() : -1;
    if (requester == 0) {
      if (Write(mirror + "/cgroup.procs", "0") || unshare(CLONE_NEWCGROUP) ||
          mount("cgroup2", child_view, "cgroup2", 0, "nsdelegate")) _exit(14);
      bool unchanged = MountUsesNsdelegate(child_view) == delegated_before &&
                       MountUsesNsdelegate(location) == delegated_before;
      int result = umount(child_view);
      _exit(unchanged && result == 0 ? 0 : 15);
    }
    int request_status = 0;
    bool request_ok = requester > 0 && waitpid(requester, &request_status, 0) == requester &&
                      request_status == 0;
    int unmounted = umount(location);
    close(pair[1]);
    _exit(child_ok && restored == 0 && views_restored && request_ok && unmounted == 0 ? 0 : 11);
  }
  close(pair[1]);
  int restricted_fd = ReceiveFd(pair[0]);
  EXPECT_GE(restricted_fd, 0);
  if (restricted_fd >= 0) {
    EXPECT_EQ(EPERM, WriteFd(restricted_fd, "max 100000"));
    EXPECT_EQ(0, WriteFd(restricted_fd, ""));
    // The opener namespace stays pinned, but hierarchy policy is evaluated
    // anew for each write, rather than cached at open or by a mount view.
    for (int i = 0; i < 8; ++i) {
      EXPECT_EQ(0, mount(nullptr, "/sys/fs/cgroup", "cgroup2", restore_flags, nullptr));
      EXPECT_EQ(0, lseek(restricted_fd, 0, SEEK_SET));
      EXPECT_EQ(0, WriteFd(restricted_fd, "max 100000"));
      EXPECT_EQ(0, mount(nullptr, "/sys/fs/cgroup", "cgroup2", restore_flags, "nsdelegate"));
      EXPECT_EQ(0, lseek(restricted_fd, 0, SEEK_SET));
      EXPECT_EQ(EPERM, WriteFd(restricted_fd, "max 100000"));
    }
    close(restricted_fd);
  }
  char done = 'D';
  // Always unblock the helper even when an expectation fails.
  Transfer(pair[0], &done, 1, true);
  close(pair[0]);
  int status;
  ASSERT_EQ(helper, waitpid(helper, &status, 0));
  EXPECT_EQ(0, status);
  // A failing helper must not leave the host's shared hierarchy policy
  // changed. Keep the failure above, but restore before running other tests.
  if (MountUsesNsdelegate("/sys/fs/cgroup") != delegated_before) {
    EXPECT_EQ(0, mount(nullptr, "/sys/fs/cgroup", "cgroup2", restore_flags,
                       delegated_before ? "nsdelegate" : nullptr));
  }
  EXPECT_EQ(delegated_before, MountUsesNsdelegate("/sys/fs/cgroup"));
}

TEST_F(ThreadedCgroup, OpenerCgroupNamespaceGovernsMigrationAcrossBoundary) {
  Topology();
  Start(a_);
  ASSERT_FALSE(HasFatalFailure());
  int pair[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
  pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    close(pair[0]);
    alarm(15);
    if (Write(b_ + "/cgroup.procs", "0") || unshare(CLONE_NEWCGROUP)) _exit(1);
    int fd = open((b_ + "/cgroup.threads").c_str(), O_WRONLY);
    if (fd < 0 || !PassFd(pair[1], fd)) _exit(2);
    close(fd);
    _exit(0);
  }
  close(pair[1]);
  int fd = ReceiveFd(pair[0]);
  close(pair[0]);
  int status;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  ASSERT_EQ(0, status);
  ASSERT_GE(fd, 0);
  int error = WriteFd(fd, std::to_string(tids_[1]));
  close(fd);
  bool delegated = Read("/proc/mounts").find("nsdelegate") != std::string::npos;
  EXPECT_EQ(delegated ? ENOENT : 0, error);
  EXPECT_EQ(delegated, Members(a_ + "/cgroup.threads").count(tids_[1]) != 0);
}

TEST_F(ThreadedCgroup, CloneIntoThreadedLeafAndInvalidDomainAdmission) {
  a_ = Make(root_, "a");
  b_ = Make(a_, "b");
  ASSERT_EQ(0, Write(a_ + "/cgroup.type", "threaded"));
  int invalid = open(b_.c_str(), O_DIRECTORY | O_RDONLY | O_CLOEXEC);
  ASSERT_GE(invalid, 0);
  clone_args args{};
  args.flags = CLONE_INTO_CGROUP;
  args.exit_signal = SIGCHLD;
  args.cgroup = invalid;
  errno = 0;
  pid_t child = syscall(SYS_clone3, &args, sizeof(args));
  int error = errno;
  close(invalid);
  // Reap even an unexpectedly accepted child rather than leaking it or
  // allowing it to run the remainder of the test suite.
  if (child == 0) _exit(97);
  if (child > 0) {
    int status;
    waitpid(child, &status, 0);
  }
  ASSERT_EQ(-1, child);
  EXPECT_EQ(EOPNOTSUPP, error);
  ASSERT_EQ(0, Write(b_ + "/cgroup.type", "threaded"));
  int target = open(b_.c_str(), O_DIRECTORY | O_RDONLY | O_CLOEXEC);
  ASSERT_GE(target, 0);
  int pair[2];
  ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, pair));
  args.cgroup = target;
  child = syscall(SYS_clone3, &args, sizeof(args));
  error = errno;
  if (child == 0) {
    close(pair[0]);
    alarm(10);
    Reply reply = Inspect();
    bool ok = Transfer(pair[1], &reply, sizeof(reply), true);
    _exit(ok ? 0 : 98);
  }
  close(target);
  close(pair[1]);
  if (child < 0) {
    close(pair[0]);
    FAIL() << "clone3: " << strerror(error);
  }
  Reply reply{};
  EXPECT_TRUE(Transfer(pair[0], &reply, sizeof(reply), false));
  close(pair[0]);
  int status;
  ASSERT_EQ(child, waitpid(child, &status, 0));
  EXPECT_EQ(0, status);
  EXPECT_EQ(0, reply.error);
  EXPECT_NE(std::string::npos, std::string(reply.cgroup).find(b_.substr(parent_.size())));
}

#if defined(__x86_64__)
// clone3 changes the child's stack. A libc syscall wrapper cannot return
// safely on that new stack, so preserve entry/argument in syscall-preserved
// registers and call the child entry without returning through a C frame.
// The parent callee-saved registers and stack are restored normally.
extern "C" long ThreadedRawClone3(clone_args*, size_t, int (*)(void*), void*);
asm(".text\n"
    ".global ThreadedRawClone3\n"
    ".type ThreadedRawClone3,@function\n"
    "ThreadedRawClone3:\n"
    "push %r12\n"
    "push %r13\n"
    "mov %rdx,%r12\n"
    "mov %rcx,%r13\n"
    "mov $435,%eax\n"
    "syscall\n"
    "test %rax,%rax\n"
    "jz 1f\n"
    "pop %r13\n"
    "pop %r12\n"
    "ret\n"
    "1:\n"
    "mov %r13,%rdi\n"
    "call *%r12\n"
    "mov %eax,%edi\n"
    "mov $60,%eax\n"
    "syscall\n"
    "ud2\n"
    ".size ThreadedRawClone3,.-ThreadedRawClone3\n");

long RawCall3(long nr, long a, long b, long c) {
  long result;
  asm volatile("syscall" : "=a"(result)
               : "a"(nr), "D"(a), "S"(b), "d"(c)
               : "rcx", "r11", "memory");
  return result;
}

int RawThreadEntry(void* opaque) {
  // This raw thread deliberately has no libc TLS registration. Never call
  // libc, allocate, access errno, or use C++ runtime facilities here.
  int fd = *static_cast<int*>(opaque);
  int tid = RawCall3(SYS_gettid, 0, 0, 0);
  if (RawCall3(SYS_write, fd, reinterpret_cast<long>(&tid), sizeof(tid)) != sizeof(tid)) return 1;
  char command;
  long result;
  do {
    result = RawCall3(SYS_read, fd, reinterpret_cast<long>(&command), 1);
  } while (result == -EINTR);
  return result == 1 ? 0 : 2;
}

TEST_F(ThreadedCgroup, CloneThreadIntoSameDomainLeafAndRejectCrossDomain) {
  Topology();
  std::string other = Make(parent_, ("dunitest-threaded-clone-other-" +
                                   std::to_string(getpid()) + "-" +
                                   std::to_string(sequence_++)).c_str());
  ASSERT_FALSE(HasFatalFailure());
  pid_t helper = fork();
  ASSERT_GE(helper, 0);
  if (helper == 0) {
    alarm(15);
    if (Write(a_ + "/cgroup.procs", "0")) _exit(1);
    const size_t size = 64 * 1024;
    void* stack = mmap(nullptr, size, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    if (stack == MAP_FAILED) _exit(2);
    int pair[2];
    if (socketpair(AF_UNIX, SOCK_STREAM, 0, pair)) _exit(3);
    int tid_word = -1;
    clone_args args{};
    args.flags = CLONE_VM | CLONE_SIGHAND | CLONE_THREAD |
                 CLONE_INTO_CGROUP | CLONE_CHILD_CLEARTID;
    args.stack = reinterpret_cast<uintptr_t>(stack);
    args.stack_size = size;
    args.child_tid = reinterpret_cast<uintptr_t>(&tid_word);
    int target = open(other.c_str(), O_RDONLY | O_DIRECTORY);
    if (target < 0) _exit(4);
    args.cgroup = target;
    long rejected = ThreadedRawClone3(&args, sizeof(args), RawThreadEntry, &pair[1]);
    close(target);
    if (rejected != -EOPNOTSUPP) _exit(5);
    target = open(b_.c_str(), O_RDONLY | O_DIRECTORY);
    if (target < 0) _exit(6);
    args.cgroup = target;
    long created = ThreadedRawClone3(&args, sizeof(args), RawThreadEntry, &pair[1]);
    close(target);
    if (created <= 0) _exit(7);
    int tid;
    if (!Transfer(pair[0], &tid, sizeof(tid), false)) _exit(8);
    if (tid != created || Members(b_ + "/cgroup.threads") != std::set<pid_t>{tid} ||
        Members(a_ + "/cgroup.threads") != std::set<pid_t>{getpid()}) _exit(9);
    char command = 'Q';
    if (!Transfer(pair[0], &command, 1, true)) _exit(10);
    // The helper's alarm bounds the wait even if clear_child_tid regresses.
    while (__atomic_load_n(&tid_word, __ATOMIC_ACQUIRE) != 0) {
      int expected = __atomic_load_n(&tid_word, __ATOMIC_ACQUIRE);
      timespec timeout{1, 0};
      syscall(SYS_futex, &tid_word, FUTEX_WAIT, expected, &timeout, nullptr, 0);
    }
    // clear_child_tid precedes the final cgroup task unlink in Linux.
    // Check eventual teardown rather than imposing a stronger join contract.
    timespec start{}, now{};
    clock_gettime(CLOCK_MONOTONIC, &start);
    while (!Members(b_ + "/cgroup.threads").empty()) {
      clock_gettime(CLOCK_MONOTONIC, &now);
      if (now.tv_sec - start.tv_sec >= 3) _exit(11);
      sched_yield();
    }
    close(pair[0]);
    close(pair[1]);
    munmap(stack, size);
    _exit(0);
  }
  int status;
  ASSERT_EQ(helper, waitpid(helper, &status, 0));
  EXPECT_EQ(0, status);
}
#endif  // x86_64 raw clone3 trampoline coverage

TEST_F(ThreadedCgroup, DirectoryAndFileOwnershipSharedAcrossMountViews) {
  a_ = Make(root_, "a");
  char location[] = "/tmp/dunitest-threaded-mount-XXXXXX";
  ASSERT_NE(nullptr, mkdtemp(location));
  scratch_dirs_.push_back(location);
  bool delegated_before = MountUsesNsdelegate("/sys/fs/cgroup");
  pid_t helper = fork();
  ASSERT_GE(helper, 0);
  if (helper == 0) {
    alarm(15);
    if (unshare(CLONE_NEWNS) || mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr)) _exit(1);
    // Mount options replace the hierarchy-wide policy even in a private
    // mount namespace; preserve it instead of silently clearing nsdelegate.
    if (mount("cgroup2", location, "cgroup2", 0,
              delegated_before ? "nsdelegate" : nullptr)) _exit(3);
    std::string mirror = std::string(location) + a_.substr(std::strlen("/sys/fs/cgroup"));
    if (chown(a_.c_str(), 65534, 65534) || chmod(a_.c_str(), 0711) ||
        chown((a_ + "/cgroup.threads").c_str(), 65534, 65534) ||
        chmod((a_ + "/cgroup.threads").c_str(), 0620)) _exit(4);
    struct stat st{};
    if (stat(mirror.c_str(), &st) || st.st_uid != 65534 || st.st_gid != 65534 ||
        (st.st_mode & 0777) != 0711) _exit(5);
    if (stat((mirror + "/cgroup.threads").c_str(), &st) || st.st_uid != 65534 ||
        st.st_gid != 65534 || (st.st_mode & 0777) != 0620) _exit(6);
    if (chmod((mirror + "/cgroup.threads").c_str(), 0600) ||
        stat((a_ + "/cgroup.threads").c_str(), &st) || (st.st_mode & 0777) != 0600) _exit(7);
    if (umount(location)) _exit(8);
    _exit(0);
  }
  int status;
  ASSERT_EQ(helper, waitpid(helper, &status, 0));
  EXPECT_EQ(0, status);
  EXPECT_EQ(delegated_before, MountUsesNsdelegate("/sys/fs/cgroup"));
}

TEST_F(ThreadedCgroup, DelegatedMkdirSetsCreatorOwnershipAndRemainsVisible) {
  ASSERT_EQ(0, chown(root_.c_str(), 65534, 65534));
  std::string child_path = root_ + "/created-by-delegate";
  dirs_.push_back(child_path);
  pid_t helper = fork();
  ASSERT_GE(helper, 0);
  if (helper == 0) {
    alarm(10);
    if (setgid(65534) || setuid(65534)) _exit(1);
    if (mkdir(child_path.c_str(), 0750)) _exit(2);
    struct stat st{};
    if (stat(child_path.c_str(), &st) || st.st_uid != 65534 || st.st_gid != 65534) _exit(3);
    if (stat((child_path + "/cgroup.procs").c_str(), &st) ||
        st.st_uid != 65534 || st.st_gid != 65534) _exit(4);
    if (Write(child_path + "/cgroup.type", "threaded")) _exit(5);
    _exit(0);
  }
  int status;
  ASSERT_EQ(helper, waitpid(helper, &status, 0));
  EXPECT_EQ(0, status);
  EXPECT_EQ("threaded", Read(child_path + "/cgroup.type"));
}

TEST_F(ThreadedCgroup, SgidRootCreatorInheritsGroupForDirectoryAndFiles) {
  Enable("pids");
  ASSERT_EQ(0, chown(root_.c_str(), 0, 23456));
  ASSERT_EQ(0, chmod(root_.c_str(), 02755));
  std::string child = Make(root_, "sgid-child");
  ASSERT_FALSE(HasFailure());
  std::string grandchild = Make(child, "sgid-grandchild");
  ASSERT_FALSE(HasFailure());
  for (const auto& path : {child, grandchild}) {
    struct stat st{};
    ASSERT_EQ(0, stat(path.c_str(), &st));
    EXPECT_EQ(0u, st.st_uid);
    EXPECT_EQ(23456u, st.st_gid);
    EXPECT_NE(0u, st.st_mode & S_ISGID);
    ASSERT_EQ(0, stat((path + "/cgroup.procs").c_str(), &st));
    EXPECT_EQ(0u, st.st_uid);
    EXPECT_EQ(23456u, st.st_gid);
    EXPECT_EQ(0u, st.st_mode & S_ISGID);
  }
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  int old = open((child + "/pids.max").c_str(), O_RDONLY | O_CLOEXEC);
  ASSERT_GE(old, 0);
  EXPECT_EQ(0, Write(root_ + "/cgroup.subtree_control", "-pids"));
  EXPECT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+pids"));
  // Recreated controller files inherit from their own containing cgroup.
  struct stat st{};
  EXPECT_EQ(0, stat((child + "/pids.max").c_str(), &st));
  EXPECT_EQ(23456u, st.st_gid);
  EXPECT_EQ(0u, st.st_mode & S_ISGID);
  EXPECT_EQ(0, fchown(old, 0, 34567));
  EXPECT_EQ(0, stat((child + "/pids.max").c_str(), &st));
  EXPECT_EQ(23456u, st.st_gid);
  close(old);
}

TEST_F(ThreadedCgroup, SgidNonzeroCreatorUsesLinux66OwnershipOverride) {
  // Linux 6.6 cgroup_kn_set_ugid overrides kernfs group inheritance for a
  // nonzero creator; later Linux versions may intentionally differ.
  ASSERT_EQ(0, chown(root_.c_str(), 65534, 23456));
  ASSERT_EQ(0, chmod(root_.c_str(), 02777));
  std::string child = root_ + "/sgid-delegate";
  dirs_.push_back(child);
  pid_t helper = fork();
  ASSERT_GE(helper, 0);
  if (helper == 0) {
    alarm(10);
    if (setgid(65534) || setuid(65534) || mkdir(child.c_str(), 0755)) _exit(1);
    struct stat st{};
    if (stat(child.c_str(), &st) || st.st_uid != 65534 || st.st_gid != 65534 ||
        !(st.st_mode & S_ISGID)) _exit(2);
    if (stat((child + "/cgroup.procs").c_str(), &st) || st.st_uid != 65534 ||
        st.st_gid != 65534 || (st.st_mode & S_ISGID)) _exit(3);
    _exit(0);
  }
  int status;
  ASSERT_EQ(helper, waitpid(helper, &status, 0));
  EXPECT_EQ(0, status);
}

TEST_F(ThreadedCgroup, DisabledControllerOldFdCannotChangeRecreatedPermissions) {
  Enable("cpuset");
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+cpuset"));
  a_ = Make(root_, "a");
  int fd = open((a_ + "/cpuset.cpus").c_str(), O_WRONLY | O_CLOEXEC);
  ASSERT_GE(fd, 0);
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "-cpuset"));
  ASSERT_EQ(0, Write(root_ + "/cgroup.subtree_control", "+cpuset"));
  struct stat before{}, after{};
  ASSERT_EQ(0, stat((a_ + "/cpuset.cpus").c_str(), &before));
  // kernfs allows metadata changes on an unlinked inode; they must never
  // affect a new file of the same name in a later controller generation.
  EXPECT_EQ(0, fchmod(fd, 0600));
  EXPECT_EQ(0, fchown(fd, 65534, 65534));
  EXPECT_EQ(ENODEV, WriteFd(fd, "0"));
  close(fd);
  ASSERT_EQ(0, stat((a_ + "/cpuset.cpus").c_str(), &after));
  EXPECT_EQ(before.st_mode, after.st_mode);
  EXPECT_EQ(before.st_uid, after.st_uid);
  EXPECT_EQ(before.st_gid, after.st_gid);
}

}  // namespace

int main(int argc, char** argv) {
  if (argc == 3 && !std::strcmp(argv[1], "--threaded-exec")) {
    Reply reply = Inspect();
    bool ok = Transfer(std::atoi(argv[2]), &reply, sizeof(reply), true);
    return ok ? 0 : 96;
  }
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
