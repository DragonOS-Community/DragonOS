#include <gtest/gtest.h>

#include <errno.h>
#include <poll.h>
#include <pthread.h>
#include <signal.h>
#include <stdint.h>
#include <stdio.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

#include <atomic>
#include <thread>

namespace {

constexpr uint64_t kMs = 1000000;

#define REQUIRE_WAIT(expr)                                                   \
  do {                                                                       \
    if (!(expr)) {                                                           \
      fprintf(stderr, "wait scenario line %d: %s errno=%d\n", __LINE__, #expr, \
              errno);                                                        \
      _exit(111);                                                            \
    }                                                                        \
  } while (0)

uint64_t Mono() {
  timespec ts{};
  REQUIRE_WAIT(clock_gettime(CLOCK_MONOTONIC, &ts) == 0);
  return uint64_t(ts.tv_sec) * 1000000000 + ts.tv_nsec;
}

void PauseMs(unsigned ms) {
  timespec ts{time_t(ms / 1000), long(ms % 1000) * 1000000};
  while (nanosleep(&ts, &ts) < 0) REQUIRE_WAIT(errno == EINTR);
}

// The watchdog is only a hang/cleanup bound, never a timeout precision oracle.
// Every scenario and its threads live in one owned process; failure cannot
// strand a blocked thread in the gtest runner.
template <class F>
void Scenario(F fn) {
  const pid_t child = fork();
  ASSERT_GE(child, 0);
  if (child == 0) {
    fn();
    _exit(0);
  }
  const uint64_t end = Mono() + 10000 * kMs;
  int status = 0;
  pid_t result;
  do {
    result = waitpid(child, &status, WNOHANG);
    if (result == child || (result < 0 && errno != EINTR)) break;
    PauseMs(1);
  } while (Mono() < end);
  if (result != child) {
    // ECHILD means ownership has already gone; never signal a recycled PID.
    if (!(result < 0 && errno == ECHILD)) {
      kill(child, SIGKILL);
      while (waitpid(child, &status, 0) < 0 && errno == EINTR) {}
    }
    FAIL() << "wait scenario did not complete: " << result;
  }
  ASSERT_TRUE(WIFEXITED(status)) << status;
  EXPECT_EQ(0, WEXITSTATUS(status));
}

volatile sig_atomic_t handled;
void Handler(int) { handled = 1; }

void InstallHandler() {
  handled = 0;
  struct sigaction action{};
  action.sa_handler = Handler;
  sigemptyset(&action.sa_mask);
  REQUIRE_WAIT(sigaction(SIGUSR1, &action, nullptr) == 0);
  sigset_t one;
  sigemptyset(&one);
  sigaddset(&one, SIGUSR1);
  REQUIRE_WAIT(pthread_sigmask(SIG_UNBLOCK, &one, nullptr) == 0);
}

void EmptyTimeout(bool use_epoll) {
  const int epfd = use_epoll ? epoll_create1(EPOLL_CLOEXEC) : -1;
  REQUIRE_WAIT(!use_epoll || epfd >= 0);
  epoll_event out{};
  for (int i = 0; i < 8; ++i) {
    const uint64_t before = Mono();
    const int result = use_epoll ? epoll_wait(epfd, &out, 1, 5)
                                : poll(nullptr, 0, 5);
    const uint64_t after = Mono();
    REQUIRE_WAIT(result == 0);
    // Relative timeout starts after our left clock. No upper latency bound:
    // scheduler/host delays are not syscall correctness failures.
    REQUIRE_WAIT(after >= before && after - before >= 5 * kMs);
  }
  if (epfd >= 0) REQUIRE_WAIT(close(epfd) == 0);
}

TEST(WaitDeadlineSemantics, EmptyPollDoesNotExpireEarly) {
  Scenario([] { EmptyTimeout(false); });
}

TEST(WaitDeadlineSemantics, EmptyEpollDoesNotExpireEarly) {
  Scenario([] { EmptyTimeout(true); });
}

TEST(WaitDeadlineSemantics, ReadyAtZeroTimeoutThenNextWaitExpires) {
  Scenario([] {
    int fds[2];
    REQUIRE_WAIT(pipe(fds) == 0);
    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    REQUIRE_WAIT(epfd >= 0);
    epoll_event event{};
    event.events = EPOLLIN;
    event.data.u64 = 19;
    REQUIRE_WAIT(epoll_ctl(epfd, EPOLL_CTL_ADD, fds[0], &event) == 0);
    char byte = 'a';
    REQUIRE_WAIT(write(fds[1], &byte, 1) == 1);
    pollfd pollfd{fds[0], POLLIN, 0};
    REQUIRE_WAIT(poll(&pollfd, 1, 0) == 1 && (pollfd.revents & POLLIN));
    epoll_event out{};
    REQUIRE_WAIT(epoll_wait(epfd, &out, 1, 0) == 1);
    REQUIRE_WAIT(out.data.u64 == 19 && (out.events & EPOLLIN));
    REQUIRE_WAIT(read(fds[0], &byte, 1) == 1);
    const uint64_t before = Mono();
    REQUIRE_WAIT(epoll_wait(epfd, &out, 1, 5) == 0);
    REQUIRE_WAIT(Mono() - before >= 5 * kMs);
    REQUIRE_WAIT(close(epfd) == 0);
    REQUIRE_WAIT(close(fds[0]) == 0 && close(fds[1]) == 0);
  });
}

TEST(WaitDeadlineSemantics, InfiniteNoFdPollInterruptedWithoutTimer) {
  Scenario([] {
    InstallHandler();
    const pthread_t target = pthread_self();
    std::atomic<bool> finished{false};
    std::thread sender([target, &finished] {
      // Repeated delivery exercises both sides of the sleep publication.
      // The owned process watchdog bounds a broken infinite wait.
      while (!finished.load()) {
        PauseMs(1);
        REQUIRE_WAIT(pthread_kill(target, SIGUSR1) == 0);
      }
    });
    for (int i = 0; i < 16; ++i) {
      handled = 0;
      errno = 0;
      const int result = poll(nullptr, 0, -1);
      const int saved_errno = errno;
      REQUIRE_WAIT(result == -1 && saved_errno == EINTR && handled);
    }
    finished.store(true);
    sender.join();
  });
}

TEST(WaitDeadlineSemantics, PendingSignalPpollMaskHandshake) {
  Scenario([] {
    InstallHandler();
    sigset_t one, original;
    sigemptyset(&one);
    sigaddset(&one, SIGUSR1);
    REQUIRE_WAIT(pthread_sigmask(SIG_BLOCK, &one, &original) == 0);
    for (int i = 0; i < 16; ++i) {
      handled = 0;
      REQUIRE_WAIT(pthread_kill(pthread_self(), SIGUSR1) == 0);
      errno = 0;
      // Pending before entering the wait. The temporary mask unblocks it;
      // no periodic timer may be needed to make progress.
      const int result = ppoll(nullptr, 0, nullptr, &original);
      const int saved_errno = errno;
      REQUIRE_WAIT(result == -1 && saved_errno == EINTR && handled);
    }
    REQUIRE_WAIT(pthread_sigmask(SIG_SETMASK, &original, nullptr) == 0);
  });
}

TEST(WaitDeadlineSemantics, MaskedSignalDoesNotEndPpollTimeout) {
  Scenario([] {
    InstallHandler();
    sigset_t one, original;
    sigemptyset(&one);
    sigaddset(&one, SIGUSR1);
    REQUIRE_WAIT(pthread_sigmask(SIG_BLOCK, &one, &original) == 0);
    REQUIRE_WAIT(pthread_kill(pthread_self(), SIGUSR1) == 0);
    const timespec timeout{0, 5000000};
    const uint64_t before = Mono();
    REQUIRE_WAIT(ppoll(nullptr, 0, &timeout, nullptr) == 0);
    REQUIRE_WAIT(Mono() - before >= 5 * kMs && !handled);
    REQUIRE_WAIT(pthread_sigmask(SIG_SETMASK, &original, nullptr) == 0);
    REQUIRE_WAIT(handled);
  });
}

TEST(WaitDeadlineSemantics, InterruptedEpollRetiresOldTimeout) {
  Scenario([] {
    InstallHandler();
    const int epfd = epoll_create1(EPOLL_CLOEXEC);
    REQUIRE_WAIT(epfd >= 0);
    const pthread_t target = pthread_self();
    std::thread sender([target] {
      PauseMs(20);
      REQUIRE_WAIT(pthread_kill(target, SIGUSR1) == 0);
    });
    epoll_event event{};
    errno = 0;
    const int result = epoll_wait(epfd, &event, 1, 100);
    const int saved_errno = errno;
    sender.join();
    REQUIRE_WAIT(result == -1 && saved_errno == EINTR && handled);
    const uint64_t before = Mono();
    // Cross the cancelled operation's old deadline with a fresh operation.
    REQUIRE_WAIT(epoll_wait(epfd, &event, 1, 150) == 0);
    REQUIRE_WAIT(Mono() - before >= 150 * kMs);
    REQUIRE_WAIT(close(epfd) == 0);
  });
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
