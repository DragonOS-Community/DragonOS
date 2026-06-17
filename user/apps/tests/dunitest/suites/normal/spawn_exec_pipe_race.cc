#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <sys/types.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <string>
#include <vector>

extern char** environ;

namespace {

constexpr char kSiblingExecMode[] = "--spawn-exec-pipe-race-sibling-exec";
constexpr char kExecExitMode[] = "--spawn-exec-pipe-race-exec-exit";
constexpr int kIterations = 512;

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

void* SiblingExecThread(void*) {
  char arg0[] = "/proc/self/exe";
  char arg1[] = "--spawn-exec-pipe-race-exec-exit";
  char* const argv[] = {arg0, arg1, nullptr};
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

int main(int argc, char** argv) {
  if (argc >= 2 && strcmp(argv[1], kExecExitMode) == 0) {
    return 0;
  }
  if (argc >= 2 && strcmp(argv[1], kSiblingExecMode) == 0) {
    RunSiblingExecHelper();
    return 1;
  }

  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
