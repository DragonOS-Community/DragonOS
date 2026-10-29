#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <string>
#include <vector>

#ifndef POLL_IN
#define POLL_IN 1
#endif

#ifndef POLL_OUT
#define POLL_OUT 2
#endif

#ifndef F_SETSIG
#define F_SETSIG 10
#endif

#ifndef O_PATH
#define O_PATH 010000000
#endif

namespace {

volatile sig_atomic_t g_signal_count = 0;
volatile sig_atomic_t g_signal_number = 0;
volatile sig_atomic_t g_signal_fd = -1;
volatile sig_atomic_t g_signal_code = 0;
volatile sig_atomic_t g_signal_band = 0;

void ResetSignalState() {
  g_signal_count = 0;
  g_signal_number = 0;
  g_signal_fd = -1;
  g_signal_code = 0;
  g_signal_band = 0;
}

void FasyncSignalHandler(int sig, siginfo_t* info, void*) {
  g_signal_count++;
  g_signal_number = sig;
  if (info != nullptr) {
    g_signal_fd = info->si_fd;
    g_signal_code = info->si_code;
    g_signal_band = info->si_band;
  }
}

void InstallSignalHandler(int signum) {
  struct sigaction action {};
  action.sa_sigaction = FasyncSignalHandler;
  sigemptyset(&action.sa_mask);
  action.sa_flags = SA_SIGINFO;
  ASSERT_EQ(0, sigaction(signum, &action, nullptr)) << strerror(errno);
}

bool WaitForSignal(int rounds = 100) {
  for (int i = 0; i < rounds; ++i) {
    if (g_signal_count > 0) {
      return true;
    }
    usleep(10 * 1000);
  }
  return false;
}

void SleepForMillis(long millis) {
  timespec ts {};
  ts.tv_sec = millis / 1000;
  ts.tv_nsec = (millis % 1000) * 1000 * 1000;
  while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
  }
}

bool WaitForExit(pid_t child, int* status, int rounds = 300) {
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

int FillPipeNonblock(int write_fd) {
  int old_flags = fcntl(write_fd, F_GETFL);
  if (old_flags < 0) {
    return -1;
  }
  if (fcntl(write_fd, F_SETFL, old_flags | O_NONBLOCK) != 0) {
    return -1;
  }

  std::vector<char> bytes(4096, 'x');
  for (;;) {
    ssize_t n = write(write_fd, bytes.data(), bytes.size());
    if (n > 0) {
      continue;
    }
    if (n < 0 && errno == EAGAIN) {
      break;
    }
    return -1;
  }

  if (fcntl(write_fd, F_SETFL, old_flags & ~O_NONBLOCK) != 0) {
    return -1;
  }
  return 0;
}

std::string MakeTempFifo() {
  char tmpl[] = "/tmp/pipe_release_fifo_XXXXXX";
  char* dir = mkdtemp(tmpl);
  if (dir == nullptr) {
    return "";
  }
  std::string path = std::string(dir) + "/fifo";
  if (mkfifo(path.c_str(), 0600) != 0) {
    rmdir(dir);
    return "";
  }
  return path;
}

void CleanupTempFifo(const std::string& path) {
  unlink(path.c_str());
  std::string dir = path.substr(0, path.rfind('/'));
  rmdir(dir.c_str());
}

void EnableFasyncSignal(int fd, int signum) {
  ASSERT_EQ(0, fcntl(fd, F_SETOWN, getpid())) << strerror(errno);
  ASSERT_EQ(0, fcntl(fd, F_SETSIG, signum)) << strerror(errno);
  int flags = fcntl(fd, F_GETFL);
  ASSERT_GE(flags, 0) << strerror(errno);
  ASSERT_EQ(0, fcntl(fd, F_SETFL, flags | O_ASYNC)) << strerror(errno);
}

}  // namespace

TEST(PipeReleaseWakeup, ReadUnblocksWithEofWhenLastWriterCloses) {
  int fds[2] = {-1, -1};
  ASSERT_EQ(0, pipe(fds)) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(fds[0]);
    SleepForMillis(50);
    close(fds[1]);
    _exit(0);
  }

  close(fds[1]);
  char ch = 0;
  errno = 0;
  EXPECT_EQ(0, read(fds[0], &ch, sizeof(ch))) << strerror(errno);
  close(fds[0]);

  int status = 0;
  ASSERT_TRUE(WaitForExit(child, &status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeReleaseWakeup, WriteUnblocksWithEpipeWhenLastReaderCloses) {
  int data_pipe[2] = {-1, -1};
  int ready_pipe[2] = {-1, -1};
  ASSERT_EQ(0, pipe(data_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
  ASSERT_EQ(0, FillPipeNonblock(data_pipe[1])) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    signal(SIGPIPE, SIG_IGN);
    close(data_pipe[0]);
    close(ready_pipe[0]);
    char ready = 'r';
    if (write(ready_pipe[1], &ready, 1) != 1) {
      _exit(2);
    }
    close(ready_pipe[1]);

    char byte = 'z';
    ssize_t n = write(data_pipe[1], &byte, 1);
    if (n < 0 && errno == EPIPE) {
      _exit(0);
    }
    _exit(3);
  }

  close(ready_pipe[1]);
  close(data_pipe[1]);

  char ready = 0;
  ASSERT_EQ(1, read(ready_pipe[0], &ready, 1)) << strerror(errno);
  close(ready_pipe[0]);

  SleepForMillis(50);
  close(data_pipe[0]);

  int status = 0;
  if (!WaitForExit(child, &status)) {
    kill(child, SIGKILL);
    waitpid(child, &status, 0);
    FAIL() << "writer stayed blocked after the last reader closed";
  }
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST(PipeReleaseWakeup, EpollReportsHupAndErrAfterEndpointClose) {
  int fds[2] = {-1, -1};
  ASSERT_EQ(0, pipe(fds)) << strerror(errno);

  int epfd = epoll_create1(0);
  ASSERT_GE(epfd, 0) << strerror(errno);

  epoll_event ev {};
  ev.events = EPOLLIN;
  ev.data.fd = fds[0];
  ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, fds[0], &ev)) << strerror(errno);

  ASSERT_EQ(0, close(fds[1])) << strerror(errno);
  epoll_event out {};
  ASSERT_EQ(1, epoll_wait(epfd, &out, 1, 1000)) << strerror(errno);
  EXPECT_EQ(fds[0], static_cast<int>(out.data.fd));
  EXPECT_NE(0U, out.events & EPOLLHUP);

  close(fds[0]);
  close(epfd);

  ASSERT_EQ(0, pipe(fds)) << strerror(errno);
  epfd = epoll_create1(0);
  ASSERT_GE(epfd, 0) << strerror(errno);

  ev = {};
  ev.events = EPOLLOUT;
  ev.data.fd = fds[1];
  ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, fds[1], &ev)) << strerror(errno);

  ASSERT_EQ(0, close(fds[0])) << strerror(errno);
  out = {};
  ASSERT_EQ(1, epoll_wait(epfd, &out, 1, 1000)) << strerror(errno);
  EXPECT_EQ(fds[1], static_cast<int>(out.data.fd));
  EXPECT_NE(0U, out.events & EPOLLERR);

  close(fds[1]);
  close(epfd);
}

TEST(PipeReleaseWakeup, NonblockErrnoRegression) {
  int fds[2] = {-1, -1};
  ASSERT_EQ(0, pipe(fds)) << strerror(errno);

  int read_flags = fcntl(fds[0], F_GETFL);
  ASSERT_GE(read_flags, 0) << strerror(errno);
  ASSERT_EQ(0, fcntl(fds[0], F_SETFL, read_flags | O_NONBLOCK)) << strerror(errno);

  char byte = 0;
  errno = 0;
  EXPECT_EQ(-1, read(fds[0], &byte, 1));
  EXPECT_EQ(EAGAIN, errno);

  ASSERT_EQ(0, FillPipeNonblock(fds[1])) << strerror(errno);
  int write_flags = fcntl(fds[1], F_GETFL);
  ASSERT_GE(write_flags, 0) << strerror(errno);
  ASSERT_EQ(0, fcntl(fds[1], F_SETFL, write_flags | O_NONBLOCK)) << strerror(errno);
  errno = 0;
  EXPECT_EQ(-1, write(fds[1], "x", 1));
  EXPECT_EQ(EAGAIN, errno);

  struct sigaction old_sigpipe {};
  struct sigaction ignore_sigpipe {};
  ignore_sigpipe.sa_handler = SIG_IGN;
  sigemptyset(&ignore_sigpipe.sa_mask);
  ASSERT_EQ(0, sigaction(SIGPIPE, &ignore_sigpipe, &old_sigpipe)) << strerror(errno);
  ASSERT_EQ(0, close(fds[0])) << strerror(errno);
  errno = 0;
  EXPECT_EQ(-1, write(fds[1], "x", 1));
  EXPECT_EQ(EPIPE, errno);
  ASSERT_EQ(0, sigaction(SIGPIPE, &old_sigpipe, nullptr)) << strerror(errno);

  close(fds[1]);
}

TEST(PipeReleaseWakeup, RdwrCloseNotifiesRemainingReader) {
  std::string path = MakeTempFifo();
  ASSERT_FALSE(path.empty()) << strerror(errno);

  int rdwr_fd = open(path.c_str(), O_RDWR | O_NONBLOCK);
  ASSERT_GE(rdwr_fd, 0) << strerror(errno);
  int read_fd = open(path.c_str(), O_RDONLY | O_NONBLOCK);
  ASSERT_GE(read_fd, 0) << strerror(errno);

  InstallSignalHandler(SIGUSR1);
  EnableFasyncSignal(read_fd, SIGUSR1);
  ResetSignalState();

  int epfd = epoll_create1(0);
  ASSERT_GE(epfd, 0) << strerror(errno);
  epoll_event ev {};
  ev.events = EPOLLIN;
  ev.data.fd = read_fd;
  ASSERT_EQ(0, epoll_ctl(epfd, EPOLL_CTL_ADD, read_fd, &ev)) << strerror(errno);

  ASSERT_EQ(0, close(rdwr_fd)) << strerror(errno);
  ASSERT_TRUE(WaitForSignal()) << "reader did not receive SIGIO after O_RDWR writer vanished";
  EXPECT_EQ(SIGUSR1, g_signal_number);
  EXPECT_EQ(read_fd, g_signal_fd);
  EXPECT_EQ(POLL_IN, g_signal_code);
  EXPECT_EQ(static_cast<long>(EPOLLIN | EPOLLRDNORM), g_signal_band);

  epoll_event out {};
  ASSERT_EQ(1, epoll_wait(epfd, &out, 1, 1000)) << strerror(errno);
  EXPECT_EQ(read_fd, static_cast<int>(out.data.fd));
  EXPECT_NE(0U, out.events & EPOLLHUP);

  close(epfd);
  close(read_fd);
  CleanupTempFifo(path);
}

TEST(PipeReleaseWakeup, RdwrCloseNotifiesRemainingWriter) {
  std::string path = MakeTempFifo();
  ASSERT_FALSE(path.empty()) << strerror(errno);

  int rdwr_fd = open(path.c_str(), O_RDWR | O_NONBLOCK);
  ASSERT_GE(rdwr_fd, 0) << strerror(errno);
  int write_fd = open(path.c_str(), O_WRONLY | O_NONBLOCK);
  ASSERT_GE(write_fd, 0) << strerror(errno);

  InstallSignalHandler(SIGUSR2);
  EnableFasyncSignal(write_fd, SIGUSR2);
  ResetSignalState();

  ASSERT_EQ(0, close(rdwr_fd)) << strerror(errno);
  ASSERT_TRUE(WaitForSignal()) << "writer did not receive SIGIO after O_RDWR reader vanished";
  EXPECT_EQ(SIGUSR2, g_signal_number);
  EXPECT_EQ(write_fd, g_signal_fd);
  EXPECT_EQ(POLL_OUT, g_signal_code);
  EXPECT_EQ(static_cast<long>(EPOLLOUT | EPOLLWRNORM | EPOLLWRBAND), g_signal_band);

  close(write_fd);
  CleanupTempFifo(path);
}

TEST(PipeReleaseWakeup, OPathCloseDoesNotNotifyPipeEndpoint) {
  std::string path = MakeTempFifo();
  ASSERT_FALSE(path.empty()) << strerror(errno);

  int rdwr_fd = open(path.c_str(), O_RDWR | O_NONBLOCK);
  ASSERT_GE(rdwr_fd, 0) << strerror(errno);
  int read_fd = open(path.c_str(), O_RDONLY | O_NONBLOCK);
  ASSERT_GE(read_fd, 0) << strerror(errno);

  InstallSignalHandler(SIGUSR1);
  EnableFasyncSignal(read_fd, SIGUSR1);
  ResetSignalState();

  int opath_fd = open(path.c_str(), O_PATH);
  ASSERT_GE(opath_fd, 0) << strerror(errno);
  ASSERT_EQ(0, close(opath_fd)) << strerror(errno);
  EXPECT_FALSE(WaitForSignal(10));

  close(read_fd);
  close(rdwr_fd);
  CleanupTempFifo(path);
}

TEST(PipeReleaseWakeup, ForkedLoggerPipeSeesEofAfterChildClosesWriteEnd) {
  int log_pipe[2] = {-1, -1};
  int ready_pipe[2] = {-1, -1};
  ASSERT_EQ(0, pipe(log_pipe)) << strerror(errno);
  ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);

  pid_t child = fork();
  ASSERT_GE(child, 0) << strerror(errno);
  if (child == 0) {
    close(log_pipe[0]);
    close(ready_pipe[0]);
    char ready = 'r';
    if (write(ready_pipe[1], &ready, 1) != 1) {
      _exit(2);
    }
    close(ready_pipe[1]);
    close(log_pipe[1]);
    _exit(0);
  }

  close(log_pipe[1]);
  close(ready_pipe[1]);
  char ready = 0;
  ASSERT_EQ(1, read(ready_pipe[0], &ready, 1)) << strerror(errno);
  close(ready_pipe[0]);

  pollfd pfd {};
  pfd.fd = log_pipe[0];
  pfd.events = POLLIN;
  ASSERT_EQ(1, poll(&pfd, 1, 1000)) << strerror(errno);
  EXPECT_NE(0, pfd.revents & POLLHUP);

  char byte = 0;
  EXPECT_EQ(0, read(log_pipe[0], &byte, 1)) << strerror(errno);
  close(log_pipe[0]);

  int status = 0;
  ASSERT_TRUE(WaitForExit(child, &status));
  ASSERT_TRUE(WIFEXITED(status));
  EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
  ::testing::InitGoogleTest(&argc, argv);
  return RUN_ALL_TESTS();
}
