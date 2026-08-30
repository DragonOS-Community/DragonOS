#include <errno.h>
#include <string.h>
#include <fcntl.h>
#include <gtest/gtest.h>
#include <poll.h>
#include <stdint.h>
#include <sys/epoll.h>
#include <sys/syscall.h>
#include <sys/timerfd.h>
#include <time.h>
#include <unistd.h>

namespace {

class UniqueFd {
  public:
    explicit UniqueFd(int fd = -1) : fd_(fd) {}
    ~UniqueFd() {
        if (fd_ >= 0) {
            close(fd_);
        }
    }
    UniqueFd(const UniqueFd &) = delete;
    UniqueFd &operator=(const UniqueFd &) = delete;
    int get() const { return fd_; }

  private:
    int fd_;
};

timespec AddMilliseconds(timespec value, long milliseconds) {
    value.tv_nsec += milliseconds * 1000 * 1000;
    value.tv_sec += value.tv_nsec / 1000000000L;
    value.tv_nsec %= 1000000000L;
    return value;
}

TEST(TimerFdSemantics, CreateValidationAndInitialState) {
    errno = 0;
    EXPECT_EQ(-1, timerfd_create(CLOCK_PROCESS_CPUTIME_ID, 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, timerfd_create(CLOCK_MONOTONIC, 0x40000000));
    EXPECT_EQ(EINVAL, errno);

    UniqueFd fd(timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK | TFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    EXPECT_NE(0, fcntl(fd.get(), F_GETFD) & FD_CLOEXEC);

    itimerspec current = {};
    ASSERT_EQ(0, timerfd_gettime(fd.get(), &current));
    EXPECT_EQ(0, current.it_value.tv_sec);
    EXPECT_EQ(0, current.it_value.tv_nsec);

    uint64_t ticks = 0;
    errno = 0;
    EXPECT_EQ(-1, read(fd.get(), &ticks, 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_read, fd.get(), nullptr, 1));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, read(fd.get(), &ticks, sizeof(ticks) - 1));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, read(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(EAGAIN, errno);

    // timerfd validates count before touching the pointer, but consumes a
    // pending expiration before the protected 8-byte copy reports EFAULT.
    itimerspec expired = {};
    expired.it_value.tv_nsec = 1;
    ASSERT_EQ(0, timerfd_settime(fd.get(), TFD_TIMER_ABSTIME, &expired, nullptr));
    pollfd ready = {fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&ready, 1, 1000));
    ASSERT_NE(0, ready.revents & POLLIN);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_read, fd.get(), nullptr, sizeof(ticks)));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, read(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(TimerFdSemantics, OneShotPollReadAndReset) {
    UniqueFd fd(timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    itimerspec value = {};
    value.it_value.tv_nsec = 50 * 1000 * 1000;
    ASSERT_EQ(0, timerfd_settime(fd.get(), 0, &value, nullptr));

    pollfd pfd = {fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&pfd, 1, 1000));
    EXPECT_EQ(POLLIN, pfd.revents);

    uint64_t ticks = 0;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(ticks)), read(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(1u, ticks);
    errno = 0;
    EXPECT_EQ(-1, read(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(TimerFdSemantics, PeriodicAccumulatesMissedExpirations) {
    UniqueFd fd(timerfd_create(CLOCK_BOOTTIME, 0));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    itimerspec value = {};
    value.it_value.tv_nsec = 20 * 1000 * 1000;
    value.it_interval = value.it_value;
    ASSERT_EQ(0, timerfd_settime(fd.get(), 0, &value, nullptr));
    usleep(95 * 1000);

    uint64_t ticks = 0;
    ASSERT_EQ(static_cast<ssize_t>(sizeof(ticks)), read(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_GE(ticks, 3u);

    itimerspec current = {};
    ASSERT_EQ(0, timerfd_gettime(fd.get(), &current));
    EXPECT_EQ(value.it_interval.tv_nsec, current.it_interval.tv_nsec);
    EXPECT_TRUE(current.it_value.tv_sec > 0 || current.it_value.tv_nsec > 0);
}

TEST(TimerFdSemantics, AbsoluteAndPastDeadlines) {
    UniqueFd fd(timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    itimerspec value = {};
    ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &value.it_value));
    value.it_value = AddMilliseconds(value.it_value, 40);
    ASSERT_EQ(0, timerfd_settime(fd.get(), TFD_TIMER_ABSTIME, &value, nullptr));

    pollfd pfd = {fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&pfd, 1, 1000));
    uint64_t ticks = 0;
    ASSERT_EQ(8, read(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(1u, ticks);

    value = {};
    value.it_value.tv_nsec = 1;
    ASSERT_EQ(0, timerfd_settime(fd.get(), TFD_TIMER_ABSTIME, &value, nullptr));
    pfd.revents = 0;
    ASSERT_EQ(1, poll(&pfd, 1, 1000));
    ASSERT_NE(0, pfd.revents & POLLIN);
    ASSERT_EQ(8, read(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(1u, ticks);
}

TEST(TimerFdSemantics, SameDeadlineTimersRemainIndependent) {
    constexpr int kTimerCount = 32;
    int fds[kTimerCount];
    pollfd poll_fds[kTimerCount];

    itimerspec value = {};
    ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &value.it_value));
    value.it_value = AddMilliseconds(value.it_value, 500);

    for (int i = 0; i < kTimerCount; ++i) {
        fds[i] = timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK);
        ASSERT_GE(fds[i], 0) << strerror(errno);
        ASSERT_EQ(0, timerfd_settime(fds[i], TFD_TIMER_ABSTIME, &value, nullptr));
        poll_fds[i] = {fds[i], POLLIN, 0};
    }

    itimerspec disarm = {};
    for (int i = 0; i < kTimerCount; i += 2) {
        ASSERT_EQ(0, timerfd_settime(fds[i], 0, &disarm, nullptr));
    }

    ASSERT_EQ(kTimerCount / 2, poll(poll_fds, kTimerCount, 2000));
    for (int i = 0; i < kTimerCount; ++i) {
        uint64_t ticks = 0;
        if (i % 2 == 0) {
            EXPECT_EQ(0, poll_fds[i].revents);
            errno = 0;
            EXPECT_EQ(-1, read(fds[i], &ticks, sizeof(ticks)));
            EXPECT_EQ(EAGAIN, errno);
        } else {
            EXPECT_EQ(POLLIN, poll_fds[i].revents);
            ASSERT_EQ(8, read(fds[i], &ticks, sizeof(ticks)));
            EXPECT_EQ(1u, ticks);
        }
        close(fds[i]);
    }
}

TEST(TimerFdSemantics, GetOldDisarmAndFcntlNonblock) {
    UniqueFd fd(timerfd_create(CLOCK_REALTIME, 0));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    itimerspec first = {};
    first.it_value.tv_sec = 2;
    first.it_interval.tv_sec = 1;
    ASSERT_EQ(0, timerfd_settime(fd.get(), 0, &first, nullptr));

    itimerspec disarm = {};
    itimerspec old = {};
    ASSERT_EQ(0, timerfd_settime(fd.get(), 0, &disarm, &old));
    EXPECT_EQ(1, old.it_interval.tv_sec);
    EXPECT_TRUE(old.it_value.tv_sec > 0 || old.it_value.tv_nsec > 0);

    ASSERT_EQ(0, fcntl(fd.get(), F_SETFL, fcntl(fd.get(), F_GETFL) | O_NONBLOCK));
    uint64_t ticks = 0;
    errno = 0;
    EXPECT_EQ(-1, read(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(EAGAIN, errno);
}

TEST(TimerFdSemantics, EpollDupAndIoRestrictions) {
    UniqueFd fd(timerfd_create(CLOCK_MONOTONIC, TFD_NONBLOCK));
    UniqueFd duplicate(dup(fd.get()));
    UniqueFd epoll_fd(epoll_create1(EPOLL_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    ASSERT_GE(duplicate.get(), 0);
    ASSERT_GE(epoll_fd.get(), 0);

    epoll_event event = {};
    event.events = EPOLLIN;
    event.data.fd = fd.get();
    ASSERT_EQ(0, epoll_ctl(epoll_fd.get(), EPOLL_CTL_ADD, fd.get(), &event));

    itimerspec value = {};
    value.it_value.tv_nsec = 30 * 1000 * 1000;
    ASSERT_EQ(0, timerfd_settime(duplicate.get(), 0, &value, nullptr));
    epoll_event ready = {};
    ASSERT_EQ(1, epoll_wait(epoll_fd.get(), &ready, 1, 1000));
    EXPECT_NE(0u, ready.events & EPOLLIN);

    uint64_t ticks = 0;
    ASSERT_EQ(8, read(duplicate.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(1u, ticks);
    EXPECT_EQ(0, lseek(fd.get(), 123, SEEK_SET));
    errno = 0;
    EXPECT_EQ(-1, pread(fd.get(), &ticks, sizeof(ticks), 0));
    EXPECT_EQ(ESPIPE, errno);
    errno = 0;
    EXPECT_EQ(-1, write(fd.get(), &ticks, sizeof(ticks)));
    EXPECT_EQ(EINVAL, errno);
}

} // namespace

int main(int argc, char **argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
