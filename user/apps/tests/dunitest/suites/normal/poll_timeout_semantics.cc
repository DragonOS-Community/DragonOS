#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <signal.h>
#include <time.h>
#include <unistd.h>

#include <array>
#include <cstdint>
#include <cstdlib>

namespace {

class FdGuard {
  public:
    explicit FdGuard(int fd = -1) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() {
        if (fd_ >= 0) {
            close(fd_);
        }
    }

    int Get() const { return fd_; }

  private:
    int fd_;
};

class SignalStateGuard {
  public:
    SignalStateGuard(int signal_number, void (*handler)(int)) : signal_number_(signal_number) {
        struct sigaction action {};
        action.sa_handler = handler;
        sigemptyset(&action.sa_mask);
        action.sa_flags = 0;
        action_saved_ = sigaction(signal_number_, &action, &old_action_) == 0;
        mask_saved_ = sigprocmask(SIG_SETMASK, nullptr, &old_mask_) == 0;
    }

    SignalStateGuard(const SignalStateGuard&) = delete;
    SignalStateGuard& operator=(const SignalStateGuard&) = delete;
    ~SignalStateGuard() {
        if (mask_saved_) {
            sigprocmask(SIG_SETMASK, &old_mask_, nullptr);
        }
        if (action_saved_) {
            sigaction(signal_number_, &old_action_, nullptr);
        }
    }

    bool Ready() const { return action_saved_ && mask_saved_; }

  private:
    int signal_number_;
    bool action_saved_ = false;
    bool mask_saved_ = false;
    struct sigaction old_action_ {};
    sigset_t old_mask_ {};
};

volatile sig_atomic_t alarm_fired = 0;

void RecordAlarm(int) { alarm_fired = 1; }

void IgnoreSignal(int) {}

std::int64_t MonotonicMilliseconds() {
    struct timespec now {};
    EXPECT_EQ(clock_gettime(CLOCK_MONOTONIC, &now), 0);
    return static_cast<std::int64_t>(now.tv_sec) * 1000 + now.tv_nsec / 1000000;
}

void ExpectTimeoutElapsed(struct pollfd* fds, nfds_t nfds) {
    constexpr int kTimeoutMs = 50;
    constexpr std::int64_t kMinimumElapsedMs = 20;

    const std::int64_t start = MonotonicMilliseconds();
    ASSERT_EQ(poll(fds, nfds, kTimeoutMs), 0) << "poll failed with errno " << errno;
    const std::int64_t elapsed = MonotonicMilliseconds() - start;
    EXPECT_GE(elapsed, kMinimumElapsedMs);
}

TEST(PollTimeoutSemantics, NegativeDescriptorsStillWaitForTimeout) {
    std::array<struct pollfd, 3> fds {{{-1, POLLIN, static_cast<short>(-1)},
                                      {-2, POLLOUT, static_cast<short>(-1)},
                                      {-3, 0, static_cast<short>(-1)}}};

    ExpectTimeoutElapsed(fds.data(), fds.size());
    for (const auto& fd : fds) {
        EXPECT_EQ(fd.revents, 0);
    }
}

TEST(PollTimeoutSemantics, RegularFileWithNoRequestedEventsStillWaits) {
    char path[] = "/tmp/poll_timeout_semantics.XXXXXX";
    FdGuard file(mkstemp(path));
    ASSERT_GE(file.Get(), 0) << "mkstemp failed with errno " << errno;
    ASSERT_EQ(unlink(path), 0) << "unlink failed with errno " << errno;
    struct pollfd fd {file.Get(), 0, static_cast<short>(-1)};

    ExpectTimeoutElapsed(&fd, 1);
    EXPECT_EQ(fd.revents, 0);
}

TEST(PollTimeoutSemantics, InvalidDescriptorReturnsBeforeTimeout) {
    int pipe_fds[2];
    ASSERT_EQ(pipe(pipe_fds), 0) << "pipe failed with errno " << errno;
    FdGuard read_end(pipe_fds[0]);
    const int invalid_fd = pipe_fds[1];
    ASSERT_EQ(close(pipe_fds[1]), 0);

    SignalStateGuard signals(SIGALRM, RecordAlarm);
    ASSERT_TRUE(signals.Ready()) << "failed to install SIGALRM state with errno " << errno;
    alarm_fired = 0;
    alarm(1);
    struct pollfd fd {invalid_fd, POLLIN, 0};
    const int result = poll(&fd, 1, 5000);
    alarm(0);

    ASSERT_EQ(result, 1) << "poll failed with errno " << errno;
    EXPECT_EQ(fd.revents, POLLNVAL);
    EXPECT_EQ(alarm_fired, 0);
}

TEST(PollTimeoutSemantics, PendingSignalWinsOverZeroTimeout) {
    SignalStateGuard signals(SIGUSR1, IgnoreSignal);
    ASSERT_TRUE(signals.Ready()) << "failed to install SIGUSR1 state with errno " << errno;

    sigset_t blocked;
    sigemptyset(&blocked);
    sigaddset(&blocked, SIGUSR1);
    ASSERT_EQ(sigprocmask(SIG_BLOCK, &blocked, nullptr), 0);
    ASSERT_EQ(raise(SIGUSR1), 0);

    sigset_t temporary_mask;
    sigemptyset(&temporary_mask);
    struct timespec timeout {};
    struct pollfd fd {-1, POLLIN, static_cast<short>(-1)};
    errno = 0;
    EXPECT_EQ(ppoll(&fd, 1, &timeout, &temporary_mask), -1);
    EXPECT_EQ(errno, EINTR);
    EXPECT_EQ(fd.revents, 0);
}

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
