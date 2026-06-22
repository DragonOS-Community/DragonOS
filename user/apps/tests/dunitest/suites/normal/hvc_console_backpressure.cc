#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

#include <array>

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

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
