#include <gtest/gtest.h>

#include <errno.h>
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

bool WaitForChild(pid_t child, int* status, int rounds = 300) {
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

TEST(PipeWaitqueueWakeup, BlockingReadConsumesChildReadyByte) {
    int ready_pipe[2] = {-1, -1};
    int release_pipe[2] = {-1, -1};
    ASSERT_EQ(0, pipe(ready_pipe)) << strerror(errno);
    ASSERT_EQ(0, pipe(release_pipe)) << strerror(errno);

    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        close(ready_pipe[0]);
        close(release_pipe[1]);

        for (int i = 0; i < 200; ++i) {
            char ready = 'r';
            if (write(ready_pipe[1], &ready, 1) != 1) {
                _exit(2);
            }

            char release = 0;
            ssize_t n = read(release_pipe[0], &release, 1);
            if (n != 1 || release != 'c') {
                _exit(3);
            }
        }
        close(ready_pipe[1]);
        close(release_pipe[0]);
        _exit(0);
    }

    close(ready_pipe[1]);
    close(release_pipe[0]);

    for (int i = 0; i < 200; ++i) {
        char ready = 0;
        ASSERT_EQ(1, read(ready_pipe[0], &ready, 1)) << strerror(errno);
        ASSERT_EQ('r', ready);

        char release = 'c';
        ASSERT_EQ(1, write(release_pipe[1], &release, 1)) << strerror(errno);
    }
    close(ready_pipe[0]);
    close(release_pipe[1]);

    int status = 0;
    if (!WaitForChild(child, &status)) {
        kill(child, SIGKILL);
        waitpid(child, nullptr, 0);
        FAIL() << "child did not finish pipe wakeup handshake";
    }
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
