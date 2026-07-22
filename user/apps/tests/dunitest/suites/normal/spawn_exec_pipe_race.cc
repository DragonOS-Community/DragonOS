#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/types.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <string>
#include <vector>

extern char** environ;

namespace {

constexpr char kSiblingExecMode[] = "--spawn-exec-pipe-race-sibling-exec";
constexpr char kExecExitMode[] = "--spawn-exec-pipe-race-exec-exit";
constexpr char kExecBlockMode[] = "--spawn-exec-pipe-race-exec-block";
constexpr int kIterations = 512;
constexpr int kFatalIterations = 128;

#ifndef P_PIDFD
#define P_PIDFD 3
#endif

#ifndef SYS_pidfd_open
#ifdef __NR_pidfd_open
#define SYS_pidfd_open __NR_pidfd_open
#else
#define SYS_pidfd_open 434
#endif
#endif

void SleepForMillis(long millis) {
  timespec ts {};
  ts.tv_sec = millis / 1000;
  ts.tv_nsec = (millis % 1000) * 1000 * 1000;
  while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
  }
}

bool SetCloseOnExec(int fd) {
  int flags = fcntl(fd, F_GETFD);
  if (flags < 0) {
    return false;
  }
  return fcntl(fd, F_SETFD, flags | FD_CLOEXEC) == 0;
}

bool SetNonblock(int fd) {
  int flags = fcntl(fd, F_GETFL);
  if (flags < 0) {
    return false;
  }
  return fcntl(fd, F_SETFL, flags | O_NONBLOCK) == 0;
}

void CloseIfOpen(int* fd) {
  if (*fd >= 0) {
    close(*fd);
    *fd = -1;
  }
}

void KillProcessGroup(pid_t child) {
  kill(-child, SIGKILL);
  kill(child, SIGKILL);
}

void WriteErrnoAndExit(int fd, int saved_errno, int code) {
  ssize_t n = write(fd, &saved_errno, sizeof(saved_errno));
  if (n != static_cast<ssize_t>(sizeof(saved_errno))) {
    _exit(126);
  }
  _exit(code);
}

void DrainReadyFd(int fd, short revents, bool* eof, std::vector<char>* dst) {
  char buf[512];
  for (;;) {
    ssize_t n = read(fd, buf, sizeof(buf));
    if (n > 0) {
      if (dst != nullptr) {
        dst->insert(dst->end(), buf, buf + n);
      }
      continue;
    }
    if (n == 0) {
      *eof = true;
    } else if (errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) {
      *eof = true;
    }
    break;
  }

  if ((revents & POLLHUP) != 0) {
    *eof = true;
  }
}

bool WaitForChildExit(pid_t child, int* status, int timeout_ms) {
  const int rounds = timeout_ms / 10;
  for (int i = 0; i < rounds; ++i) {
    pid_t ret = waitpid(child, status, WNOHANG);
    if (ret == child) {
      return true;
    }
    if (ret < 0 && errno != EINTR) {
      return false;
    }
    SleepForMillis(10);
  }
  return false;
}

struct ChildCleanup {
  pid_t child = -1;
  int ready_read = -1;
  int ready_write = -1;
  int release_read = -1;
  int release_write = -1;
  int pidfd = -1;

  ~ChildCleanup() {
    CloseIfOpen(&ready_read);
    CloseIfOpen(&ready_write);
    CloseIfOpen(&release_read);
    CloseIfOpen(&release_write);
    CloseIfOpen(&pidfd);
    if (child > 0) {
      kill(child, SIGKILL);
      while (waitpid(child, nullptr, 0) < 0 && errno == EINTR) {
      }
    }
  }
};

void* SiblingExecThread(void*) {
  char arg0[] = "/proc/self/exe";
  char arg1[] = "--spawn-exec-pipe-race-exec-exit";
  char* const argv[] = {arg0, arg1, nullptr};
  char* const envp[] = {nullptr};
  execve("/proc/self/exe", argv, envp);
  _exit(errno);
}

struct FatalExecArgs {
  int tid_fd;
};

void* FatalSiblingExecThread(void* opaque) {
  auto* args = static_cast<FatalExecArgs*>(opaque);
  pid_t tid = static_cast<pid_t>(syscall(SYS_gettid));
  if (write(args->tid_fd, &tid, sizeof(tid)) != sizeof(tid)) {
    _exit(5);
  }
  return SiblingExecThread(nullptr);
}

struct BlockingExecArgs {
  int ready_fd;
  int release_fd;
};

void* BlockingSiblingExecThread(void* opaque) {
  auto* args = static_cast<BlockingExecArgs*>(opaque);
  char ready_fd[16];
  char release_fd[16];
  snprintf(ready_fd, sizeof(ready_fd), "%d", args->ready_fd);
  snprintf(release_fd, sizeof(release_fd), "%d", args->release_fd);

  char arg0[] = "/proc/self/exe";
  char* const argv[] = {arg0, const_cast<char*>(kExecBlockMode), ready_fd,
                        release_fd, nullptr};
  char* const envp[] = {nullptr};
  execve("/proc/self/exe", argv, envp);
  _exit(errno);
}

void RunSiblingExecHelper() {
  pthread_t thread;
  if (pthread_create(&thread, nullptr, SiblingExecThread, nullptr) != 0) {
    _exit(1);
  }

  for (;;) {
    pause();
  }
}

void RunBlockingSiblingExecHelper(int ready_fd, int release_fd) {
  BlockingExecArgs args = {.ready_fd = ready_fd, .release_fd = release_fd};
  pthread_t thread;
  if (pthread_create(&thread, nullptr, BlockingSiblingExecThread, &args) != 0) {
    _exit(1);
  }

  for (;;) {
    pause();
  }
}

void RunBlockedExecImage(int ready_fd, int release_fd) {
  char ready = 'R';
  if (write(ready_fd, &ready, sizeof(ready)) != sizeof(ready)) {
    _exit(2);
  }

  char release = 0;
  ssize_t n;
  do {
    n = read(release_fd, &release, sizeof(release));
  } while (n < 0 && errno == EINTR);
  _exit(n == sizeof(release) ? 0 : 3);
}

void TriggerSiblingExecOnce() {
  pid_t child = fork();
  ASSERT_GE(child, 0) << "fork sibling trigger failed: " << strerror(errno);
  if (child == 0) {
    char arg0[] = "/proc/self/exe";
    char* const argv[] = {arg0, const_cast<char*>(kSiblingExecMode), nullptr};
    char* const envp[] = {nullptr};
    execve("/proc/self/exe", argv, envp);
    _exit(errno);
  }

  int status = 0;
  ASSERT_TRUE(WaitForChildExit(child, &status, 5000))
      << "sibling exec trigger did not exit";
  ASSERT_TRUE(WIFEXITED(status)) << "sibling trigger status=" << status;
  ASSERT_EQ(0, WEXITSTATUS(status)) << "sibling trigger status=" << status;
}

void SpawnGtestHelpWithCloexecPipeOnce(int iter) {
  int err_pipe[2] = {-1, -1};
  int out_pipe[2] = {-1, -1};
  int stderr_pipe[2] = {-1, -1};

  ASSERT_EQ(0, pipe(err_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(out_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(stderr_pipe)) << strerror(errno);
  ASSERT_TRUE(SetCloseOnExec(err_pipe[1])) << strerror(errno);
  ASSERT_TRUE(SetNonblock(err_pipe[0])) << strerror(errno);
  ASSERT_TRUE(SetNonblock(out_pipe[0])) << strerror(errno);
  ASSERT_TRUE(SetNonblock(stderr_pipe[0])) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << "fork failed at iter=" << iter << ": " << strerror(errno);
  if (child == 0) {
    close(err_pipe[0]);
    close(out_pipe[0]);
    close(stderr_pipe[0]);

    if (setpgid(0, 0) != 0) {
      int saved = errno;
      WriteErrnoAndExit(err_pipe[1], saved, 120);
    }

    if (dup2(out_pipe[1], STDOUT_FILENO) < 0 ||
        dup2(stderr_pipe[1], STDERR_FILENO) < 0) {
      int saved = errno;
      WriteErrnoAndExit(err_pipe[1], saved, 121);
    }

    close(out_pipe[1]);
    close(stderr_pipe[1]);

    char arg0[] = "/proc/self/exe";
    char arg1[] = "--gtest_help";
    char* const argv[] = {arg0, arg1, nullptr};
    execve("/proc/self/exe", argv, environ);

    int saved = errno;
    WriteErrnoAndExit(err_pipe[1], saved, 127);
  }

  close(err_pipe[1]);
  err_pipe[1] = -1;
  close(out_pipe[1]);
  out_pipe[1] = -1;
  close(stderr_pipe[1]);
  stderr_pipe[1] = -1;

  bool err_eof = false;
  bool out_eof = false;
  bool stderr_eof = false;
  bool child_exited = false;
  int status = 0;
  std::vector<char> err;
  std::vector<char> out;
  std::vector<char> child_stderr;

  for (int waited_ms = 0; waited_ms < 5000; waited_ms += 10) {
    pollfd fds[3] = {};
    fds[0].fd = err_eof ? -1 : err_pipe[0];
    fds[0].events = err_eof ? 0 : (POLLIN | POLLHUP);
    fds[1].fd = out_eof ? -1 : out_pipe[0];
    fds[1].events = out_eof ? 0 : (POLLIN | POLLHUP);
    fds[2].fd = stderr_eof ? -1 : stderr_pipe[0];
    fds[2].events = stderr_eof ? 0 : (POLLIN | POLLHUP);
    poll(fds, 3, 10);

    if (!err_eof) {
      DrainReadyFd(err_pipe[0], fds[0].revents, &err_eof, &err);
    }
    if (!out_eof) {
      DrainReadyFd(out_pipe[0], fds[1].revents, &out_eof, &out);
    }
    if (!stderr_eof) {
      DrainReadyFd(stderr_pipe[0], fds[2].revents, &stderr_eof, &child_stderr);
    }

    if (!child_exited) {
      pid_t ret = waitpid(child, &status, WNOHANG);
      if (ret == child) {
        child_exited = true;
      } else if (ret < 0 && errno != EINTR) {
        FAIL() << "waitpid failed at iter=" << iter << ": " << strerror(errno);
      }
    }

    if (err_eof && out_eof && stderr_eof && child_exited) {
      break;
    }
  }

  CloseIfOpen(&err_pipe[0]);
  CloseIfOpen(&out_pipe[0]);
  CloseIfOpen(&stderr_pipe[0]);

  if (!(err_eof && out_eof && stderr_eof && child_exited)) {
    KillProcessGroup(child);
    waitpid(child, nullptr, 0);
  }

  ASSERT_TRUE(err_eof) << "exec CLOEXEC error pipe did not reach EOF at iter=" << iter;
  ASSERT_TRUE(err.empty()) << "pre-exec/exec error pipe has data at iter=" << iter;
  ASSERT_TRUE(out_eof) << "stdout pipe did not reach EOF at iter=" << iter;
  ASSERT_TRUE(stderr_eof) << "stderr pipe did not reach EOF at iter=" << iter;
  ASSERT_TRUE(child_exited) << "child did not exit at iter=" << iter;
  ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status << " iter=" << iter;
  ASSERT_EQ(0, WEXITSTATUS(status)) << "child status=" << status << " iter=" << iter;
  ASSERT_FALSE(out.empty()) << "gtest help produced no stdout at iter=" << iter;
}

}  // namespace

TEST(SpawnExecPipeRace, GtestHelpSpawnAfterSiblingExecStress) {
  for (int i = 0; i < kIterations; ++i) {
    if ((i % 16) == 0) {
      ASSERT_NO_FATAL_FAILURE(TriggerSiblingExecOnce());
    }
    ASSERT_NO_FATAL_FAILURE(SpawnGtestHelpWithCloexecPipeOnce(i));
  }
}

TEST(SpawnExecPipeRace, NonLeaderExecKeepsOriginalPidAndPidfdAlive) {
  ChildCleanup cleanup;
  int ready_pipe[2] = {-1, -1};
  int release_pipe[2] = {-1, -1};
  ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
  cleanup.ready_read = ready_pipe[0];
  cleanup.ready_write = ready_pipe[1];
  ASSERT_EQ(0, pipe(release_pipe)) << strerror(errno);
  cleanup.release_read = release_pipe[0];
  cleanup.release_write = release_pipe[1];

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(ready_pipe[0]);
    close(release_pipe[1]);
    RunBlockingSiblingExecHelper(ready_pipe[1], release_pipe[0]);
    _exit(4);
  }
  cleanup.child = child;

  close(ready_pipe[1]);
  cleanup.ready_write = -1;
  close(release_pipe[0]);
  cleanup.release_read = -1;
  int pidfd = static_cast<int>(syscall(SYS_pidfd_open, child, 0));
  ASSERT_GE(pidfd, 0) << strerror(errno);
  cleanup.pidfd = pidfd;

  pollfd ready_poll = {.fd = ready_pipe[0], .events = POLLIN, .revents = 0};
  ASSERT_EQ(1, poll(&ready_poll, 1, 5000)) << "exec image did not become ready";
  char ready = 0;
  ASSERT_EQ(1, read(ready_pipe[0], &ready, sizeof(ready)));
  ASSERT_EQ('R', ready);

  int status = 0;
  EXPECT_EQ(0, waitpid(child, &status, WNOHANG));

  pollfd pidfd_poll = {.fd = pidfd, .events = POLLIN, .revents = 0};
  EXPECT_EQ(0, poll(&pidfd_poll, 1, 0));

  siginfo_t info {};
  ASSERT_EQ(0, syscall(SYS_waitid, P_PIDFD, pidfd, &info,
                       WEXITED | WNOHANG, nullptr))
      << strerror(errno);
  EXPECT_EQ(0, info.si_pid);

  char release = 'X';
  ASSERT_EQ(1, write(release_pipe[1], &release, sizeof(release)));
  ASSERT_TRUE(WaitForChildExit(child, &status, 5000));
  cleanup.child = -1;
  ASSERT_TRUE(WIFEXITED(status));
  ASSERT_EQ(0, WEXITSTATUS(status));

  pidfd_poll.revents = 0;
  EXPECT_EQ(1, poll(&pidfd_poll, 1, 1000));
  EXPECT_NE(0, pidfd_poll.revents & POLLIN);

  close(pidfd);
  cleanup.pidfd = -1;
  close(ready_pipe[0]);
  cleanup.ready_read = -1;
  close(release_pipe[1]);
  cleanup.release_write = -1;
}

TEST(SpawnExecPipeRace, FatalSignalDuringSiblingExecKeepsWaitOwnership) {
  for (int i = 0; i < kFatalIterations; ++i) {
    ChildCleanup cleanup;
    int tid_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(tid_pipe)) << "iter=" << i << ": " << strerror(errno);
    cleanup.ready_read = tid_pipe[0];
    cleanup.ready_write = tid_pipe[1];

    pid_t child = fork();
    ASSERT_GE(child, 0) << "iter=" << i << ": " << strerror(errno);
    if (child == 0) {
      close(tid_pipe[0]);
      FatalExecArgs args = {.tid_fd = tid_pipe[1]};
      pthread_t thread;
      if (pthread_create(&thread, nullptr, FatalSiblingExecThread, &args) != 0) {
        _exit(6);
      }
      for (;;) {
        pause();
      }
    }
    cleanup.child = child;

    close(tid_pipe[1]);
    cleanup.ready_write = -1;
    pid_t exec_tid = -1;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(exec_tid)),
              read(tid_pipe[0], &exec_tid, sizeof(exec_tid)))
        << "iter=" << i << ": " << strerror(errno);
    ASSERT_GT(exec_tid, 0) << "iter=" << i;

    if ((i & 1) != 0) {
      sched_yield();
    }
    if ((i & 3) == 3) {
      SleepForMillis(1);
    }

    errno = 0;
    int kill_result = static_cast<int>(
        syscall(SYS_tgkill, child, exec_tid, SIGKILL));
    ASSERT_TRUE(kill_result == 0 || errno == ESRCH)
        << "iter=" << i << ": " << strerror(errno);

    int status = 0;
    ASSERT_TRUE(WaitForChildExit(child, &status, 5000))
        << "fatal sibling exec was not waitable at iter=" << i;
    cleanup.child = -1;
    ASSERT_TRUE(WIFSIGNALED(status) ||
                (WIFEXITED(status) &&
                 (WEXITSTATUS(status) == 0 || WEXITSTATUS(status) == EAGAIN)))
        << "unexpected child status=" << status << " at iter=" << i;
  }
}

int main(int argc, char** argv) {
  if (argc >= 2 && strcmp(argv[1], kExecExitMode) == 0) {
    return 0;
  }
  if (argc >= 2 && strcmp(argv[1], kSiblingExecMode) == 0) {
    RunSiblingExecHelper();
    return 1;
  }
  if (argc >= 4 && strcmp(argv[1], kExecBlockMode) == 0) {
    RunBlockedExecImage(atoi(argv[2]), atoi(argv[3]));
    return 1;
  }

  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
