#include <gtest/gtest.h>

#ifdef _WIN32

TEST(ProcessSignalFork, PosixForkAndJobControlUnavailableOnWindows) {
    GTEST_SKIP() << "Windows does not provide POSIX fork/SIGSTOP/SIGCONT semantics";
}

#else

#include <errno.h>
#include <sched.h>
#include <signal.h>
#include <string.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

void SleepForMillis(long millis) {
    timespec ts {};
    ts.tv_sec = millis / 1000;
    ts.tv_nsec = (millis % 1000) * 1000 * 1000;
    while (nanosleep(&ts, &ts) != 0 && errno == EINTR) {
    }
}

bool WaitForExit(pid_t child, int* status, int rounds) {
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

}  // namespace

TEST(ProcessSignalFork, StopContinueRaceDoesNotLeaveChildStuck) {
    pid_t child = fork();
    ASSERT_GE(child, 0) << "fork failed: errno=" << errno << " (" << strerror(errno) << ")";

    if (child == 0) {
        for (;;) {
            asm volatile("" ::: "memory");
        }
    }

    for (int i = 0; i < 200; ++i) {
        ASSERT_EQ(0, kill(child, SIGSTOP))
            << "SIGSTOP failed at iteration " << i << ": errno=" << errno << " ("
            << strerror(errno) << ")";
        sched_yield();
        ASSERT_EQ(0, kill(child, SIGCONT))
            << "SIGCONT failed at iteration " << i << ": errno=" << errno << " ("
            << strerror(errno) << ")";

        int event_status = 0;
        while (waitpid(child, &event_status, WUNTRACED | WCONTINUED | WNOHANG) == child) {
        }
    }

    ASSERT_EQ(0, kill(child, SIGCONT))
        << "final SIGCONT failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_EQ(0, kill(child, SIGTERM))
        << "SIGTERM failed: errno=" << errno << " (" << strerror(errno) << ")";

    int status = 0;
    if (!WaitForExit(child, &status, 300)) {
        kill(child, SIGKILL);
        waitpid(child, &status, 0);
        FAIL() << "child did not exit after SIGSTOP/SIGCONT race stress";
    }

    ASSERT_TRUE(WIFSIGNALED(status) || WIFEXITED(status)) << "child status=" << status;
    if (WIFSIGNALED(status)) {
        EXPECT_EQ(SIGTERM, WTERMSIG(status));
    } else {
        EXPECT_EQ(0, WEXITSTATUS(status));
    }
}

#endif

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
