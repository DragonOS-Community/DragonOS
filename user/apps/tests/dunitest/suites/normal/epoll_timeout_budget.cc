#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

constexpr int kTimeoutMs = 200;
constexpr int kSoftLimitMs = 1200;
constexpr int kHardLimitMs = 2500;
constexpr int kRounds = 30;

long diff_ms(const timespec& start, const timespec& end) {
    long sec = end.tv_sec - start.tv_sec;
    long nsec = end.tv_nsec - start.tv_nsec;
    return sec * 1000 + nsec / 1000000;
}

void drain_pipe_once(int rfd) {
    char buf[256];
    while (read(rfd, buf, sizeof(buf)) > 0) {
    }
}

void flap_pipe_forever(int rfd, int wfd) {
    char one = 'x';
    while (true) {
        if (write(wfd, &one, sizeof(one)) == -1) {
            if (errno != EAGAIN) {
                _exit(3);
            }
        }
        drain_pipe_once(rfd);
        usleep(1000);
    }
}

class EpollBudgetFixture : public ::testing::Test {
protected:
    void TearDown() override {
        if (child_ > 0) {
            kill(child_, SIGTERM);
            waitpid(child_, nullptr, 0);
            child_ = -1;
        }
        if (epfd_ >= 0) {
            close(epfd_);
            epfd_ = -1;
        }
        if (pipefd_[0] >= 0) {
            close(pipefd_[0]);
            pipefd_[0] = -1;
        }
        if (pipefd_[1] >= 0) {
            close(pipefd_[1]);
            pipefd_[1] = -1;
        }
    }

    int epfd_ = -1;
    int pipefd_[2] = {-1, -1};
    pid_t child_ = -1;
};

}  // namespace

TEST_F(EpollBudgetFixture, TimeoutBudgetNotResetByNoEventWakeups) {
    ASSERT_EQ(0, pipe(pipefd_)) << "pipe failed: errno=" << errno << " (" << strerror(errno)
                                << ")";
    ASSERT_EQ(0, fcntl(pipefd_[0], F_SETFL, O_NONBLOCK))
        << "fcntl(read, O_NONBLOCK) failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, fcntl(pipefd_[1], F_SETFL, O_NONBLOCK))
        << "fcntl(write, O_NONBLOCK) failed: errno=" << errno << " (" << strerror(errno) << ")";

    epfd_ = epoll_create1(0);
    ASSERT_GE(epfd_, 0) << "epoll_create1 failed: errno=" << errno << " (" << strerror(errno)
                        << ")";

    epoll_event ev = {};
    ev.events = EPOLLIN;
    ev.data.fd = pipefd_[0];
    ASSERT_EQ(0, epoll_ctl(epfd_, EPOLL_CTL_ADD, pipefd_[0], &ev))
        << "epoll_ctl ADD failed: errno=" << errno << " (" << strerror(errno) << ")";

    child_ = fork();
    ASSERT_GE(child_, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";
    if (child_ == 0) {
        flap_pipe_forever(pipefd_[0], pipefd_[1]);
        _exit(0);
    }

    int hard_violations = 0;
    int soft_violations = 0;

    for (int i = 0; i < kRounds; i++) {
        timespec ts1 = {};
        timespec ts2 = {};
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &ts1));
        errno = 0;
        epoll_event out = {};
        int ret = epoll_wait(epfd_, &out, 1, kTimeoutMs);
        int saved_errno = errno;
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &ts2));

        long elapsed = diff_ms(ts1, ts2);

        if (ret < 0 && saved_errno != EINTR) {
            hard_violations++;
            continue;
        }
        if (ret > 0) {
            drain_pipe_once(pipefd_[0]);
        }

        if (elapsed > kHardLimitMs) {
            hard_violations++;
        } else if (elapsed > kSoftLimitMs) {
            soft_violations++;
        }
    }

    EXPECT_EQ(0, hard_violations) << "hard_violations=" << hard_violations
                                  << " soft_violations=" << soft_violations;
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
