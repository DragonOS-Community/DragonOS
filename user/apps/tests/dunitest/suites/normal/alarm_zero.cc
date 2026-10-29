#include <gtest/gtest.h>

#include <errno.h>
#include <signal.h>
#include <stdint.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

bool SleepMilliseconds(long milliseconds) {
    timespec delay = {milliseconds / 1000, (milliseconds % 1000) * 1000000};
    while (nanosleep(&delay, &delay) < 0) {
        if (errno != EINTR) return false;
    }
    return true;
}

enum class Scenario { FirstCancel, RepeatedCancel, CancelAfterDisarm, Nonzero };

int RunScenario(Scenario scenario) {
    // Do not call alarm(0) to initialize the child: that is the operation under test.
    struct sigaction action = {};
    action.sa_handler = SIG_DFL;
    sigemptyset(&action.sa_mask);
    sigset_t signals;
    sigemptyset(&signals);
    sigaddset(&signals, SIGALRM);
    if (sigaction(SIGALRM, &action, nullptr) != 0 ||
        sigprocmask(SIG_UNBLOCK, &signals, nullptr) != 0) return 10;

    switch (scenario) {
        case Scenario::FirstCancel:
            if (alarm(0) != 0) return 11;
            return SleepMilliseconds(300) ? 0 : 12;
        case Scenario::RepeatedCancel:
            for (int i = 0; i < 3; ++i) {
                if (alarm(0) != 0) return 13;
                if (!SleepMilliseconds(100)) return 14;
            }
            return 0;
        case Scenario::CancelAfterDisarm:
            if (alarm(2) != 0) return 15;
            alarm(0);
            if (alarm(0) != 0) return 16;
            return SleepMilliseconds(2200) ? 0 : 17;
        case Scenario::Nonzero:
            if (alarm(1) != 0) return 18;
            // A real alarm must terminate the child, not merely return success.
            SleepMilliseconds(3000);
            return 19;
    }
    return 20;
}

class AlarmZero : public testing::Test {
  protected:
    void TearDown() override {
        if (child_ > 0) {
            kill(child_, SIGKILL);
            while (waitpid(child_, nullptr, 0) < 0 && errno == EINTR) {}
        }
    }

    void Check(Scenario scenario, int expected_signal = 0) {
        timespec start = {};
        ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &start));
        child_ = fork();
        ASSERT_GE(child_, 0);
        if (child_ == 0) _exit(RunScenario(scenario));

        // Independent watchdog: never use alarm() to protect its own tests.
        for (;;) {
            int status = 0;
            const pid_t result = waitpid(child_, &status, WNOHANG);
            if (result == child_) {
                child_ = -1;
                if (expected_signal != 0) {
                    ASSERT_TRUE(WIFSIGNALED(status)) << "wait status: " << status;
                    EXPECT_EQ(expected_signal, WTERMSIG(status));
                } else {
                    ASSERT_TRUE(WIFEXITED(status)) << "wait status: " << status;
                    EXPECT_EQ(0, WEXITSTATUS(status));
                }
                return;
            }
            ASSERT_TRUE(result == 0 || (result < 0 && errno == EINTR)) << errno;
            timespec now = {};
            ASSERT_EQ(0, clock_gettime(CLOCK_MONOTONIC, &now));
            const int64_t elapsed_ns = (now.tv_sec - start.tv_sec) * INT64_C(1000000000)
                                       + now.tv_nsec - start.tv_nsec;
            ASSERT_LT(elapsed_ns, INT64_C(10000000000)) << "child timed out";
            ASSERT_TRUE(SleepMilliseconds(10));
        }
    }

    pid_t child_ = -1;
};

TEST_F(AlarmZero, FirstCancelDoesNotSignal) { Check(Scenario::FirstCancel); }
TEST_F(AlarmZero, RepeatedCancelDoesNotSignal) { Check(Scenario::RepeatedCancel); }
TEST_F(AlarmZero, CancelAfterDisarmDoesNotRearm) { Check(Scenario::CancelAfterDisarm); }
TEST_F(AlarmZero, NonzeroAlarmStillSignals) { Check(Scenario::Nonzero, SIGALRM); }

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
