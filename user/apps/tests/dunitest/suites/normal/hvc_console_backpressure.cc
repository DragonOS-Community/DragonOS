#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <unistd.h>

#include <array>
#include <atomic>
#include <thread>

namespace {

class UniqueFd {
public:
    explicit UniqueFd(int fd = -1) : fd_(fd) {}
    UniqueFd(const UniqueFd&) = delete;
    UniqueFd& operator=(const UniqueFd&) = delete;

    ~UniqueFd() {
        if (fd_ >= 0) {
            close(fd_);
        }
    }

    int get() const { return fd_; }

private:
    int fd_;
};

} // namespace

TEST(HvcConsoleBackpressureTest, LargeNonblockingWriteReportsProgressOrAgain) {
    UniqueFd fd(open("/dev/hvc0", O_WRONLY | O_NONBLOCK));
    if (fd.get() < 0 && errno == ENOENT) {
        GTEST_SKIP() << "/dev/hvc0 is not available on this platform";
    }
    ASSERT_GE(fd.get(), 0) << "open(/dev/hvc0) failed: errno=" << errno << " ("
                           << strerror(errno) << ")";

    std::array<char, 8192> buf{};
    for (size_t i = 0; i < buf.size(); ++i) {
        buf[i] = static_cast<char>('a' + (i % 26));
    }

    ssize_t ret = write(fd.get(), buf.data(), buf.size());
    if (ret < 0) {
        EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK || errno == EINTR)
            << "unexpected write errno=" << errno << " (" << strerror(errno) << ")";
    } else {
        EXPECT_GT(ret, 0);
        EXPECT_LE(static_cast<size_t>(ret), buf.size());
    }

    int pending = -1;
    ASSERT_EQ(0, ioctl(fd.get(), TIOCOUTQ, &pending))
        << "TIOCOUTQ failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_GE(pending, 0);
}

TEST(HvcConsoleBackpressureTest, PollRegistrationSurvivesConcurrentTxCompletions) {
    UniqueFd fd(open("/dev/hvc0", O_WRONLY | O_NONBLOCK));
    if (fd.get() < 0 && errno == ENOENT) {
        GTEST_SKIP() << "/dev/hvc0 is not available on this platform";
    }
    ASSERT_GE(fd.get(), 0) << "open(/dev/hvc0) failed: errno=" << errno << " ("
                           << strerror(errno) << ")";

    constexpr int kPollRounds = 20000;
    constexpr int kWriteRounds = 256;
    std::atomic<bool> start{false};
    std::atomic<int> poll_error{0};
    std::atomic<int> write_error{0};

    std::thread poller([&]() {
        while (!start.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        pollfd pfd = {fd.get(), POLLOUT, 0};
        for (int i = 0; i < kPollRounds; ++i) {
            pfd.revents = 0;
            const int ret = poll(&pfd, 1, 0);
            if (ret < 0 && errno != EINTR) {
                poll_error.store(errno != 0 ? errno : EIO, std::memory_order_release);
                return;
            }
        }
    });

    std::thread writer([&]() {
        while (!start.load(std::memory_order_acquire)) {
            std::this_thread::yield();
        }
        std::array<char, 64> data{};
        data.fill('x');
        for (int i = 0; i < kWriteRounds; ++i) {
            const ssize_t ret = write(fd.get(), data.data(), data.size());
            if (ret < 0 && errno != EAGAIN && errno != EWOULDBLOCK && errno != EINTR) {
                write_error.store(errno != 0 ? errno : EIO, std::memory_order_release);
                return;
            }
            std::this_thread::yield();
        }
    });

    start.store(true, std::memory_order_release);
    poller.join();
    writer.join();

    EXPECT_EQ(0, poll_error.load()) << strerror(poll_error.load());
    EXPECT_EQ(0, write_error.load()) << strerror(write_error.load());
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
