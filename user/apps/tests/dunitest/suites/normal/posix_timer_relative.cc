#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <string.h>
#include <sys/syscall.h>
#include <time.h>
#include <unistd.h>

namespace {

// Use kernel timer IDs, not libc's opaque timer_t representation.
class PosixTimerRelative : public testing::TestWithParam<clockid_t> {
  protected:
    void SetUp() override {
        sigemptyset(&signals_);
        sigaddset(&signals_, SIGUSR1);
        sigaddset(&signals_, SIGALRM);
        ASSERT_EQ(0, sigprocmask(SIG_BLOCK, &signals_, &old_mask_));
        blocked_ = true;
    }

    void TearDown() override {
        if (id_ >= 0) {
            const long result = syscall(SYS_timer_delete, id_);
            EXPECT_EQ(0, result) << strerror(errno);
            // Do not unblock a potentially live timer after a cleanup failure.
            if (result != 0) {
                return;
            }
            id_ = -1;
        }
        if (blocked_) {
            // Deletion need not discard an already pending SI_TIMER signal.
            if (Drain()) {
                EXPECT_EQ(0, sigprocmask(SIG_SETMASK, &old_mask_, nullptr));
            }
        }
    }

    long Create(sigevent* event) {
        return syscall(SYS_timer_create, GetParam(), event, &id_);
    }

    long Set(const itimerspec& value) {
        return syscall(SYS_timer_settime, id_, 0, &value, nullptr);
    }

    int Wait(siginfo_t* info, long milliseconds = 2000) {
        const timespec limit = {milliseconds / 1000,
                                (milliseconds % 1000) * 1000000};
        // No unbounded EINTR retries: an unrelated signal is a test failure.
        return sigtimedwait(&signals_, info, &limit);
    }

    bool Drain() {
        const timespec zero = {};
        siginfo_t info = {};
        // Only two ordinary signals are used; allow bounded EINTR retries.
        for (int attempt = 0; attempt < 8; ++attempt) {
            if (sigtimedwait(&signals_, &info, &zero) >= 0) {
                continue;
            }
            if (errno == EAGAIN) {
                return true;
            }
            if (errno != EINTR) {
                ADD_FAILURE() << "signal drain: " << strerror(errno);
                return false;
            }
        }
        ADD_FAILURE() << "signal drain did not finish within its bound";
        return false;
    }

    int id_ = -1;

  private:
    sigset_t signals_ = {};
    sigset_t old_mask_ = {};
    bool blocked_ = false;
};

TEST_P(PosixTimerRelative, NoneLifecycle) {
    sigevent event = {};
    event.sigev_notify = SIGEV_NONE;
    ASSERT_EQ(0, Create(&event)) << strerror(errno);
    itimerspec current = {};
    ASSERT_EQ(0, syscall(SYS_timer_gettime, id_, &current));
    EXPECT_EQ(0, current.it_value.tv_sec);
    EXPECT_EQ(0, current.it_value.tv_nsec);
    EXPECT_EQ(0, current.it_interval.tv_sec);
    EXPECT_EQ(0, current.it_interval.tv_nsec);

    itimerspec value = {};
    value.it_value.tv_sec = 60;
    value.it_interval.tv_sec = 2;
    ASSERT_EQ(0, Set(value));
    ASSERT_EQ(0, syscall(SYS_timer_gettime, id_, &current));
    EXPECT_GE(current.it_value.tv_sec, 0);
    EXPECT_LE(current.it_value.tv_sec, 60);
    EXPECT_GE(current.it_value.tv_nsec, 0);
    EXPECT_LT(current.it_value.tv_nsec, 1000000000L);
    EXPECT_TRUE(current.it_value.tv_sec > 0 || current.it_value.tv_nsec > 0);
    EXPECT_EQ(2, current.it_interval.tv_sec);
    EXPECT_EQ(0, current.it_interval.tv_nsec);

    value = {};
    ASSERT_EQ(0, Set(value));
    ASSERT_EQ(0, syscall(SYS_timer_gettime, id_, &current));
    // Linux may retain the old expiry for SIGEV_NONE after disarming;
    // assert zero remaining time on the signal-delivering timer below instead.
    EXPECT_EQ(0, current.it_interval.tv_sec);
    EXPECT_EQ(0, current.it_interval.tv_nsec);
    const int deleted = id_;
    ASSERT_EQ(0, syscall(SYS_timer_delete, id_));
    id_ = -1;
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_timer_gettime, deleted, &current));
    EXPECT_EQ(EINVAL, errno);
}

TEST_P(PosixTimerRelative, OneShotSignalPayload) {
    sigevent event = {};
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    event.sigev_value.sival_int = 0x12345;
    ASSERT_EQ(0, Create(&event)) << strerror(errno);
    itimerspec value = {};
    value.it_value.tv_nsec = 20000000;
    ASSERT_EQ(0, Set(value));
    siginfo_t info = {};
    ASSERT_EQ(SIGUSR1, Wait(&info)) << strerror(errno);
    EXPECT_EQ(SIGUSR1, info.si_signo);
    EXPECT_EQ(SI_TIMER, info.si_code);
    EXPECT_EQ(event.sigev_value.sival_int, info.si_value.sival_int);
}

TEST_P(PosixTimerRelative, PeriodicSignalAndDisarm) {
    sigevent event = {};
    event.sigev_notify = SIGEV_SIGNAL;
    event.sigev_signo = SIGUSR1;
    ASSERT_EQ(0, Create(&event)) << strerror(errno);
    itimerspec value = {};
    value.it_value.tv_nsec = 20000000;
    value.it_interval = value.it_value;
    ASSERT_EQ(0, Set(value));
    for (int i = 0; i < 2; ++i) {
        siginfo_t info = {};
        ASSERT_EQ(SIGUSR1, Wait(&info)) << strerror(errno);
        EXPECT_EQ(SI_TIMER, info.si_code);
    }
    value = {};
    ASSERT_EQ(0, Set(value));
    itimerspec current = {};
    ASSERT_EQ(0, syscall(SYS_timer_gettime, id_, &current));
    EXPECT_EQ(0, current.it_value.tv_sec);
    EXPECT_EQ(0, current.it_value.tv_nsec);
    EXPECT_EQ(0, current.it_interval.tv_sec);
    EXPECT_EQ(0, current.it_interval.tv_nsec);
    ASSERT_TRUE(Drain());
    siginfo_t info = {};
    errno = 0;
    EXPECT_EQ(-1, Wait(&info, 100));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(PosixTimerRelative, DefaultNotification) {
    ASSERT_EQ(0, Create(nullptr)) << strerror(errno);
    itimerspec value = {};
    value.it_value.tv_nsec = 20000000;
    ASSERT_EQ(0, Set(value));
    siginfo_t info = {};
    ASSERT_EQ(SIGALRM, Wait(&info)) << strerror(errno);
    EXPECT_EQ(SIGALRM, info.si_signo);
    EXPECT_EQ(SI_TIMER, info.si_code);
}

INSTANTIATE_TEST_SUITE_P(Clocks, PosixTimerRelative,
                        testing::Values(CLOCK_REALTIME, CLOCK_MONOTONIC));

TEST(PosixTimerClock, UnsupportedClock) {
    sigevent event = {};
    event.sigev_notify = SIGEV_NONE;
    int id = -1;
    errno = 0;
    const long result = syscall(SYS_timer_create, CLOCK_MONOTONIC_RAW, &event, &id);
    const int error = errno;
    // Keep the negative test safe even if an implementation accepts the clock.
    if (result == 0) {
        EXPECT_EQ(0, syscall(SYS_timer_delete, id));
    }
    EXPECT_EQ(-1, result);
    EXPECT_TRUE(error == EINVAL || error == EOPNOTSUPP) << strerror(error);
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
