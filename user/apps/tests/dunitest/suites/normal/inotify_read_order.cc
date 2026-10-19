#include <gtest/gtest.h>
#include <array>
#include <cerrno>
#include <csignal>
#include <fcntl.h>
#include <poll.h>
#include <sys/inotify.h>
#include <sys/ioctl.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {
class InotifyReadOrder : public testing::Test {
protected:
    void SetUp() override {
        file_ = mkstemp(path_);
        ASSERT_GE(file_, 0);
        notify_ = inotify_init1(IN_NONBLOCK | IN_CLOEXEC);
        ASSERT_GE(notify_, 0);
        ASSERT_GE(inotify_add_watch(notify_, path_, IN_MODIFY), 0);
    }
    void TearDown() override {
        if (notify_ >= 0) close(notify_);
        if (file_ >= 0) close(file_);
        unlink(path_);
    }
    void Queue() { ASSERT_EQ(1, write(file_, "x", 1)); }
    int Queued() {
        int bytes = -1;
        EXPECT_EQ(0, ioctl(notify_, FIONREAD, &bytes));
        return bytes;
    }
    int file_ = -1, notify_ = -1;
    char path_[64] = "/tmp/inotify_order_XXXXXX";
};

TEST_F(InotifyReadOrder, EmptyNonblockingShortScalarReturnsEagain) {
    std::array<char, 16> output {};
    for (size_t length : {0U, 1U, 15U}) {
        errno = 0;
        EXPECT_EQ(-1, read(notify_, output.data(), length));
        EXPECT_EQ(EAGAIN, errno) << "count=" << length;
    }
    errno = 0;
    EXPECT_EQ(-1, read(notify_, nullptr, 0));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_F(InotifyReadOrder, QueuedShortScalarPreservesTheEvent) {
    ASSERT_NO_FATAL_FAILURE(Queue());
    ASSERT_EQ(16, Queued());
    std::array<char, 16> output {};
    for (size_t length : {0U, 1U, 15U}) {
        errno = 0;
        EXPECT_EQ(-1, read(notify_, output.data(), length));
        EXPECT_EQ(EINVAL, errno) << "count=" << length;
        EXPECT_EQ(16, Queued());
    }
    ASSERT_EQ(16, read(notify_, output.data(), output.size()));
    EXPECT_EQ(0, Queued());
}

TEST_F(InotifyReadOrder, ZeroVectorDoesNotUseScalarQueueRules) {
    char byte = 0;
    iovec empty {nullptr, 0}, short_output {&byte, 1};
    EXPECT_EQ(0, readv(notify_, &empty, 1));
    EXPECT_EQ(0, readv(notify_, nullptr, 0));
    errno = 0;
    EXPECT_EQ(-1, readv(notify_, &short_output, 1));
    EXPECT_EQ(EAGAIN, errno);
    ASSERT_NO_FATAL_FAILURE(Queue());
    EXPECT_EQ(0, readv(notify_, &empty, 1));
    EXPECT_EQ(16, Queued());
    errno = 0;
    EXPECT_EQ(-1, readv(notify_, &short_output, 1));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(16, Queued());
}

void Interrupt(int) {}
TEST_F(InotifyReadOrder, EmptyBlockingShortReadWaitsForSignal) {
    for (size_t length : {0U, 1U, 15U}) {
        int result_pipe[2];
        ASSERT_EQ(0, pipe(result_pipe));
        pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            close(result_pipe[0]);
            int flags = fcntl(notify_, F_GETFL);
            if (flags < 0 || fcntl(notify_, F_SETFL, flags & ~O_NONBLOCK)) _exit(2);
            struct sigaction action {};
            action.sa_handler = Interrupt;
            sigemptyset(&action.sa_mask);
            if (sigaction(SIGALRM, &action, nullptr)) _exit(3);
            char output[16] {};
            ualarm(30000, 0);
            errno = 0;
            const ssize_t result = read(notify_, output, length);
            const int error = errno;
            ualarm(0, 0);
            int values[] = {static_cast<int>(result), error};
            if (write(result_pipe[1], values, sizeof(values)) != sizeof(values)) _exit(4);
            _exit(0);
        }
        close(result_pipe[1]);
        pollfd ready {result_pipe[0], POLLIN, 0};
        int result = poll(&ready, 1, 2000);
        if (result != 1) {
            kill(child, SIGKILL);
            close(result_pipe[0]);
            waitpid(child, nullptr, 0);
            FAIL() << "blocking read exceeded its interrupt deadline";
        }
        int values[2] {};
        EXPECT_EQ(sizeof(values), read(result_pipe[0], values, sizeof(values)));
        close(result_pipe[0]);
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0));
        ASSERT_TRUE(WIFEXITED(status));
        ASSERT_EQ(0, WEXITSTATUS(status));
        EXPECT_EQ(-1, values[0]);
        EXPECT_EQ(EINTR, values[1]) << "count=" << length;
        ASSERT_EQ(0, fcntl(notify_, F_SETFL, O_NONBLOCK));
    }
}
} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
