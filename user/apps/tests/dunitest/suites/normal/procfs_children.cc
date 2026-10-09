// Linux 6.6 /proc/<pid>/task/<tid>/children and named-FIFO identity.
#include <errno.h>
#include <fcntl.h>
#include <gtest/gtest.h>
#include <memory>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <string>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vector>

namespace {
std::string Path(pid_t tid) {
  return "/proc/" + std::to_string(getpid()) + "/task/" + std::to_string(tid) +
         "/children";
}
std::string ReadChildren(pid_t tid, size_t chunk = 32) {
  int fd = open(Path(tid).c_str(), O_RDONLY);
  if (fd < 0) {
    ADD_FAILURE() << Path(tid) << ": " << strerror(errno);
    return "<open failed>";
  }
  std::string out;
  char buf[32];
  ssize_t n;
  while ((n = read(fd, buf, chunk)) > 0)
    out.append(buf, n);
  EXPECT_EQ(n, 0) << strerror(errno);
  if (!out.empty())
    EXPECT_EQ(read(fd, buf, sizeof(buf)), 0);
  EXPECT_EQ(lseek(fd, 0, SEEK_SET), 0);
  std::string again;
  while ((n = read(fd, buf, sizeof(buf))) > 0)
    again.append(buf, n);
  EXPECT_EQ(n, 0);
  EXPECT_EQ(again, out);
  close(fd);
  return out;
}
class Child {
public:
  pid_t pid = -1;
  Child() {
    pid = fork();
    if (pid == 0) {
      for (;;)
        pause();
    }
  }
  ~Child() { Reap(); }
  void Reap() {
    if (pid > 0) {
      kill(pid, SIGKILL);
      while (waitpid(pid, nullptr, 0) < 0 && errno == EINTR) {
      }
    }
    pid = -1;
  }
};
struct ThreadChild {
  int ready[2], release[2], report[2];
  int mode = 0;
  bool setup_ok = true;
  pid_t tid = -1, child = -1, sibling = -1;
};
void *ExecInThread(void *arg) {
  int fd = *static_cast<int *>(arg);
  const auto number = std::to_string(fd);
  execl("/proc/self/exe", "procfs_children_test", "--park", number.c_str(),
        nullptr);
  _exit(20);
}
ssize_t ReadReport(int fd, void *buf, size_t len) {
  pollfd pfd{fd, POLLIN, 0};
  int rc;
  do {
    rc = poll(&pfd, 1, 5000);
  } while (rc < 0 && errno == EINTR);
  return rc > 0 ? read(fd, buf, len) : -1;
}
void *ForkInThread(void *arg) {
  auto *data = static_cast<ThreadChild *>(arg);
  data->tid = syscall(SYS_gettid);
  data->child = fork();
  if (data->child == 0) {
    if (data->mode == 1) {
      pid_t sibling = syscall(SYS_clone, CLONE_PARENT | SIGCHLD, nullptr,
                              nullptr, nullptr, 0);
      if (sibling == 0) {
        for (;;)
          pause();
      }
      if (write(data->report[1], &sibling, sizeof(sibling)) != sizeof(sibling))
        _exit(21);
    } else if (data->mode == 2) {
      pthread_t exec_thread;
      if (pthread_create(&exec_thread, nullptr, ExecInThread,
                         &data->report[1]) != 0)
        _exit(22);
    }
    for (;;)
      pause();
  }
  if (data->child < 0) {
    data->setup_ok = false;
  } else if (data->mode == 1) {
    data->setup_ok =
        ReadReport(data->report[0], &data->sibling, sizeof(data->sibling)) ==
            sizeof(data->sibling) &&
        data->sibling > 0;
  } else if (data->mode == 2) {
    char ready = 0;
    data->setup_ok =
        ReadReport(data->report[0], &ready, 1) == 1 && ready == 'x';
  }
  char c = 'x';
  if (write(data->ready[1], &c, 1) != 1)
    return nullptr;
  if (read(data->release[0], &c, 1) != 1)
    return nullptr;
  return nullptr;
}
} // namespace

TEST(ProcfsChildren, LiveZombieAndReapedChild) {
  EXPECT_EQ(ReadChildren(getpid()), "");
  Child child;
  ASSERT_GT(child.pid, 0);
  const auto expected = std::to_string(child.pid) + " ";
  EXPECT_EQ(ReadChildren(getpid(), 1), expected);
  ASSERT_EQ(kill(child.pid, SIGKILL), 0);
  siginfo_t info = {};
  ASSERT_EQ(waitid(P_PID, child.pid, &info, WEXITED | WNOWAIT), 0);
  EXPECT_EQ(ReadChildren(getpid()), expected);
  child.Reap();
  EXPECT_EQ(ReadChildren(getpid()), "");
}

TEST(ProcfsChildren, ManyChildrenSurviveShortReadsAndSeek) {
  std::vector<std::unique_ptr<Child>> children;
  std::string expected;
  for (int i = 0; i < 70; ++i) {
    auto child = std::make_unique<Child>();
    ASSERT_GT(child->pid, 0);
    expected += std::to_string(child->pid) + " ";
    children.push_back(std::move(child));
  }
  EXPECT_EQ(ReadChildren(getpid(), 1), expected);
}

void CheckThreadChildren(int mode) {
  ThreadChild data;
  data.mode = mode;
  ASSERT_EQ(pipe(data.report), 0);
  ASSERT_EQ(pipe(data.ready), 0);
  ASSERT_EQ(pipe(data.release), 0);
  pthread_t thread;
  ASSERT_EQ(pthread_create(&thread, nullptr, ForkInThread, &data), 0);
  char c;
  const ssize_t received = read(data.ready[0], &c, 1);
  EXPECT_EQ(received, 1);
  EXPECT_TRUE(data.setup_ok) << "fork/clone/exec helper setup failed";
  if (received == 1 && data.setup_ok && data.child > 0) {
    std::string expected = std::to_string(data.child) + " ";
    if (mode == 1) {
      EXPECT_GT(data.sibling, 0);
      expected += std::to_string(data.sibling) + " ";
    }
    EXPECT_EQ(ReadChildren(data.tid, 1), expected);
    EXPECT_EQ(ReadChildren(getpid()), "");
  } else
    ADD_FAILURE() << "worker fork failed";
  EXPECT_EQ(write(data.release[1], "x", 1), 1);
  EXPECT_EQ(pthread_join(thread, nullptr), 0);
  if (data.child > 0) {
    std::string expected = std::to_string(data.child) + " ";
    if (data.sibling > 0)
      expected += std::to_string(data.sibling) + " ";
    EXPECT_EQ(ReadChildren(getpid()), expected);
    kill(data.child, SIGKILL);
    EXPECT_EQ(waitpid(data.child, nullptr, 0), data.child);
  }
  if (data.sibling > 0) {
    kill(data.sibling, SIGKILL);
    EXPECT_EQ(waitpid(data.sibling, nullptr, 0), data.sibling);
  }
  for (int fd : {data.ready[0], data.ready[1], data.release[0], data.release[1],
                 data.report[0], data.report[1]})
    close(fd);
  EXPECT_EQ(ReadChildren(getpid()), "");
}

TEST(ProcfsChildren, ThreadOwnsChildUntilItExits) { CheckThreadChildren(0); }
TEST(ProcfsChildren, CloneParentPreservesParentThread) {
  CheckThreadChildren(1);
}
TEST(ProcfsChildren, NonleaderExecPreservesParentThread) {
  CheckThreadChildren(2);
}

TEST(ProcfsChildren, ProcMountUsesItsPidNamespace) {
  pid_t outer = fork();
  ASSERT_GE(outer, 0);
  if (outer == 0) {
    if (unshare(CLONE_NEWNS | CLONE_NEWPID) != 0)
      _exit(10);
    if (mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr) != 0)
      _exit(11);
    pid_t inner = fork();
    if (inner < 0)
      _exit(12);
    if (inner == 0) {
      if (mount("proc", "/proc", "proc", 0, nullptr) != 0)
        _exit(13);
      bool ok = getpid() == 1;
      {
        Child child;
        ok = ok && child.pid > 1 &&
             ReadChildren(1, 1) == std::to_string(child.pid) + " ";
      }
      _exit(ok ? 0 : 14);
    }
    int status;
    if (waitpid(inner, &status, 0) != inner)
      _exit(15);
    _exit(WIFEXITED(status) ? WEXITSTATUS(status) : 16);
  }
  int status;
  ASSERT_EQ(waitpid(outer, &status, 0), outer);
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(WEXITSTATUS(status), 0)
      << "namespace setup or PID translation failed";
}

TEST(ProcfsChildren, UnlinkedNamedFifoRetainsFdPath) {
  char dir[] = "/tmp/proc-children-fifo.XXXXXX";
  ASSERT_NE(mkdtemp(dir), nullptr);
  const std::string fifo = std::string(dir) + "/fifo";
  ASSERT_EQ(mkfifo(fifo.c_str(), 0600), 0);
  int fd = open(fifo.c_str(), O_RDWR | O_NONBLOCK);
  ASSERT_GE(fd, 0);
  const auto link = "/proc/self/fd/" + std::to_string(fd);
  char buf[512];
  ssize_t len = readlink(link.c_str(), buf, sizeof(buf));
  ASSERT_GT(len, 0);
  EXPECT_EQ(std::string(buf, len), fifo);
  EXPECT_EQ(unlink(fifo.c_str()), 0);
  len = readlink(link.c_str(), buf, sizeof(buf));
  EXPECT_GT(len, 0);
  if (len > 0)
    EXPECT_EQ(std::string(buf, len), fifo + " (deleted)");
  close(fd);
  EXPECT_EQ(rmdir(dir), 0);
}

int main(int argc, char **argv) {
  if (argc == 3 && strcmp(argv[1], "--park") == 0) {
    const int fd = atoi(argv[2]);
    if (write(fd, "x", 1) != 1)
      _exit(23);
    for (;;)
      pause();
  }
  alarm(30);
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
