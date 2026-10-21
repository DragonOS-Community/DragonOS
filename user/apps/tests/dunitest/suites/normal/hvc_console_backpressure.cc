#include <gtest/gtest.h>

#include <errno.h>
#include <stdlib.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
#include <poll.h>
#include <termios.h>
#include <time.h>
#include <unistd.h>

#include <array>
#include <atomic>
#include <cstdint>
#include <string>
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

    int release() {
        const int fd = fd_;
        fd_ = -1;
        return fd;
    }

    void reset(int fd) {
        if (fd_ >= 0) {
            close(fd_);
        }
        fd_ = fd;
    }

private:
    int fd_;
};

} // namespace

namespace {

// These tests require a host which stops reading a dedicated VirtIO console
// until the test has finished. Run each test separately, then compare the host
// stream with accepted_a copies of 'A', followed by accepted_b copies of 'B'.
// tools/testing/hvc_close_receiver.py provides this bounded host-side check.
// The normal runner's console and persistent command channel must stay usable.
class HvcConsoleLastCloseTest : public ::testing::Test {
protected:
    void SetUp() override {
        const char* device = getenv("DUNITEST_HVC_CLOSE_DEVICE");
        if (device == nullptr || *device == '\0') {
            GTEST_SKIP() << "requires DUNITEST_HVC_CLOSE_DEVICE and host-controlled TX backpressure";
        }
        const char prefix[] = "/dev/hvc";
        ASSERT_EQ(0, strncmp(device, prefix, sizeof(prefix) - 1));
        const char* index = device + sizeof(prefix) - 1;
        ASSERT_NE('\0', *index);
        for (const char* digit = index; *digit != '\0'; ++digit) {
            ASSERT_GE(*digit, '0');
            ASSERT_LE(*digit, '9');
        }
        char* end = nullptr;
        errno = 0;
        const unsigned long number = strtoul(index, &end, 10);
        ASSERT_EQ(0, errno);
        ASSERT_EQ('\0', *end);
        ASSERT_GE(number, 2ul) << "hvc0 and hvc1 belong to the console and command channel";
        device_ = device;
        RecordProperty("close_device", device_);
        RecordProperty("accepted_a", "0");
        RecordProperty("accepted_b", "0");
        fd_.reset(open(device_.c_str(), O_WRONLY | O_NOCTTY | O_NONBLOCK));
        ASSERT_GE(fd_.get(), 0) << strerror(errno);
        struct termios term = {};
        ASSERT_EQ(0, tcgetattr(fd_.get(), &term)) << strerror(errno);
        cfmakeraw(&term);
        ASSERT_EQ(0, tcsetattr(fd_.get(), TCSANOW, &term)) << strerror(errno);
    }

    static int64_t MonotonicMilliseconds() {
        struct timespec now = {};
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) {
            ADD_FAILURE() << "clock_gettime failed: " << strerror(errno);
            return -1;
        }
        return static_cast<int64_t>(now.tv_sec) * 1000 + now.tv_nsec / 1000000;
    }

    void FillUntilBlocked() {
        constexpr size_t kMaxBytes = 32 * 1024 * 1024;
        std::array<char, 4096> data;
        data.fill('A');
        const int64_t start = MonotonicMilliseconds();
        ASSERT_GE(start, 0);
        size_t accepted = 0;
        bool blocked = false;
        while (accepted < kMaxBytes) {
            const size_t remaining = kMaxBytes - accepted;
            const size_t length = remaining < data.size() ? remaining : data.size();
            const ssize_t count = write(fd_.get(), data.data(), length);
            if (count > 0) {
                accepted += static_cast<size_t>(count);
                RecordProperty("accepted_a", std::to_string(accepted));
            } else if (count < 0 && (errno == EAGAIN || errno == EWOULDBLOCK)) {
                blocked = true;
                break;
            } else if (count == 0 || errno != EINTR) {
                FAIL() << "write failed: count=" << count << ", errno=" << errno;
            }
            const int64_t now = MonotonicMilliseconds();
            ASSERT_GE(now, start);
            ASSERT_LT(now - start, 10000) << "host did not establish bounded TX backpressure";
        }
        RecordProperty("backpressure_reached", blocked ? "1" : "0");
        ASSERT_TRUE(blocked) << "host must stop reading the dedicated console before the test";
        int pending = 0;
        ASSERT_EQ(0, ioctl(fd_.get(), TIOCOUTQ, &pending)) << strerror(errno);
        RecordProperty("pending_before_close", pending);
        ASSERT_GT(pending, 256) << "requires an unsubmitted tail beyond the in-flight TX chunk";
    }

    void CloseAndRecord() {
        const int64_t before = MonotonicMilliseconds();
        ASSERT_GE(before, 0);
        const int result = close(fd_.release());
        const int saved_errno = errno;
        const int64_t after = MonotonicMilliseconds();
        ASSERT_GE(after, before);
        RecordProperty("close_elapsed_ms", std::to_string(after - before));
        EXPECT_EQ(0, result) << strerror(saved_errno);
        EXPECT_LT(after - before, 2000) << "last close must not wait indefinitely for the host";
    }

    std::string device_;
    UniqueFd fd_;
};

} // namespace

TEST_F(HvcConsoleLastCloseTest, LastClosePreservesAcceptedOutput) {
    ASSERT_NO_FATAL_FAILURE(FillUntilBlocked());
    CloseAndRecord();
}

TEST_F(HvcConsoleLastCloseTest, NonlastClosePreservesQueuedOutput) {
    UniqueFd remaining(open(device_.c_str(), O_WRONLY | O_NOCTTY | O_NONBLOCK));
    ASSERT_GE(remaining.get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(FillUntilBlocked());
    ASSERT_EQ(0, close(fd_.release())) << strerror(errno);
    int pending = 0;
    ASSERT_EQ(0, ioctl(remaining.get(), TIOCOUTQ, &pending)) << strerror(errno);
    RecordProperty("pending_after_nonlast_close", pending);
    EXPECT_GT(pending, 256);
    fd_.reset(remaining.release());
    CloseAndRecord();
}

TEST_F(HvcConsoleLastCloseTest, ReopenAppendsAfterPreviouslyAcceptedOutput) {
    ASSERT_NO_FATAL_FAILURE(FillUntilBlocked());
    ASSERT_NO_FATAL_FAILURE(CloseAndRecord());
    fd_.reset(open(device_.c_str(), O_WRONLY | O_NOCTTY | O_NONBLOCK));
    ASSERT_GE(fd_.get(), 0) << strerror(errno);
    std::array<char, 256> suffix;
    suffix.fill('B');
    const ssize_t count = write(fd_.get(), suffix.data(), suffix.size());
    if (count < 0) {
        EXPECT_TRUE(errno == EAGAIN || errno == EWOULDBLOCK) << strerror(errno);
    } else {
        EXPECT_GT(count, 0);
        RecordProperty("accepted_b", std::to_string(count));
    }
    CloseAndRecord();
}

TEST_F(HvcConsoleLastCloseTest, ReopenPreservesShortWriteOrder) {
    std::array<char, 256> data;
    data.fill('A');
    const ssize_t first = write(fd_.get(), data.data(), data.size());
    ASSERT_EQ(static_cast<ssize_t>(data.size()), first) << strerror(errno);
    RecordProperty("accepted_a", std::to_string(first));
    ASSERT_NO_FATAL_FAILURE(CloseAndRecord());
    fd_.reset(open(device_.c_str(), O_WRONLY | O_NOCTTY | O_NONBLOCK));
    ASSERT_GE(fd_.get(), 0) << strerror(errno);
    data.fill('B');
    const ssize_t second = write(fd_.get(), data.data(), data.size());
    ASSERT_EQ(static_cast<ssize_t>(data.size()), second) << strerror(errno);
    RecordProperty("accepted_b", std::to_string(second));
    CloseAndRecord();
}

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
