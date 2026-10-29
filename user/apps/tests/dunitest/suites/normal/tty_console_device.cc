#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <termios.h>
#include <unistd.h>

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

void ExpectConsoleDevice(const char* path) {
    struct stat st = {};
    ASSERT_EQ(0, stat(path, &st)) << "stat(" << path << ") failed: errno=" << errno << " ("
                                 << strerror(errno) << ")";
    EXPECT_TRUE(S_ISCHR(st.st_mode)) << path << " is not a character device";
    EXPECT_EQ(4u, major(st.st_rdev)) << path << " major mismatch";
    EXPECT_EQ(0u, minor(st.st_rdev)) << path << " minor mismatch";
}

} // namespace

TEST(TtyConsoleDevice, Tty0AndVc0AreUsableWithoutFramebuffer) {
    ExpectConsoleDevice("/dev/tty0");

    struct stat alias_stat = {};
    ASSERT_EQ(0, lstat("/dev/vc0", &alias_stat))
        << "lstat(/dev/vc0) failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(S_ISLNK(alias_stat.st_mode)) << "/dev/vc0 is not a symbolic link";

    char target[16] = {};
    ssize_t target_len = readlink("/dev/vc0", target, sizeof(target));
    ASSERT_EQ(4, target_len) << "readlink(/dev/vc0) failed: errno=" << errno << " ("
                             << strerror(errno) << ")";
    EXPECT_EQ(0, memcmp(target, "tty0", 4));

    ExpectConsoleDevice("/dev/vc0");

    UniqueFd tty(open("/dev/tty0", O_RDWR | O_NOCTTY));
    ASSERT_GE(tty.get(), 0) << "open(/dev/tty0) failed: errno=" << errno << " ("
                            << strerror(errno) << ")";

    struct termios term = {};
    ASSERT_EQ(0, tcgetattr(tty.get(), &term))
        << "tcgetattr(/dev/tty0) failed: errno=" << errno << " (" << strerror(errno) << ")";

    struct winsize winsize = {};
    ASSERT_EQ(0, ioctl(tty.get(), TIOCGWINSZ, &winsize))
        << "TIOCGWINSZ failed: errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_GT(winsize.ws_row, 0u);
    EXPECT_GT(winsize.ws_col, 0u);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
