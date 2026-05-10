#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

namespace {

constexpr const char* kDevPtsDir = "/dev/pts";

TEST(DevPtsDir, ReadReturnsEisdir) {
    struct stat st = {};
    ASSERT_EQ(0, stat(kDevPtsDir, &st))
        << "stat(/dev/pts) failed: errno=" << errno << " (" << strerror(errno) << ")";
    ASSERT_TRUE(S_ISDIR(st.st_mode)) << "/dev/pts is not a directory";

    int fd = open(kDevPtsDir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(fd, 0) << "open(/dev/pts) failed: errno=" << errno << " (" << strerror(errno)
                     << ")";

    char buf[16] = {};
    errno = 0;
    ssize_t nread = read(fd, buf, sizeof(buf));
    int saved_errno = errno;

    int close_ret = close(fd);
    ASSERT_EQ(0, close_ret)
        << "close(/dev/pts) failed: errno=" << errno << " (" << strerror(errno) << ")";

    EXPECT_EQ(-1, nread) << "read(/dev/pts) unexpectedly succeeded";
    EXPECT_EQ(EISDIR, saved_errno)
        << "read(/dev/pts) should return EISDIR, got errno=" << saved_errno << " ("
        << strerror(saved_errno) << ")";
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
