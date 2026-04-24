#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <string.h>
#include <unistd.h>

namespace {

// Linux kernel definition: __SIGRTMAX is 64. Keep a local constant so this
// test does not rely on the libc SIGRTMAX macro (which is typically 64 but
// exposed as an adjusted value by glibc).
constexpr int kKernelSigrtmax = 64;

int fd_pair() {
    int fds[2] = {-1, -1};
    if (pipe(fds) != 0) {
        return -1;
    }
    close(fds[1]);
    return fds[0];
}

}  // namespace

// 默认值应为 0，表示使用 SIGIO。
TEST(FcntlSignal, GetSigDefaultsToZero) {
    int fd = fd_pair();
    ASSERT_GE(fd, 0);

    int sig = fcntl(fd, F_GETSIG);
    EXPECT_EQ(0, sig) << "errno=" << errno << " (" << strerror(errno) << ")";

    close(fd);
}

// F_SETSIG 后 F_GETSIG 应返回同一值。
TEST(FcntlSignal, SetSigThenGetSigRoundTrip) {
    int fd = fd_pair();
    ASSERT_GE(fd, 0);

    ASSERT_EQ(0, fcntl(fd, F_SETSIG, SIGUSR1));
    EXPECT_EQ(SIGUSR1, fcntl(fd, F_GETSIG));

    ASSERT_EQ(0, fcntl(fd, F_SETSIG, SIGUSR2));
    EXPECT_EQ(SIGUSR2, fcntl(fd, F_GETSIG));

    // 0 表示恢复默认 SIGIO。
    ASSERT_EQ(0, fcntl(fd, F_SETSIG, 0));
    EXPECT_EQ(0, fcntl(fd, F_GETSIG));

    close(fd);
}

// 边界：SIGRTMAX 必须被接受。
TEST(FcntlSignal, SetSigAcceptsSigrtmaxBoundary) {
    int fd = fd_pair();
    ASSERT_GE(fd, 0);

    ASSERT_EQ(0, fcntl(fd, F_SETSIG, kKernelSigrtmax));
    EXPECT_EQ(kKernelSigrtmax, fcntl(fd, F_GETSIG));

    close(fd);
}

// 负值与超过 SIGRTMAX 的值应返回 EINVAL。
TEST(FcntlSignal, SetSigRejectsInvalidValues) {
    int fd = fd_pair();
    ASSERT_GE(fd, 0);

    errno = 0;
    EXPECT_EQ(-1, fcntl(fd, F_SETSIG, -1));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    EXPECT_EQ(-1, fcntl(fd, F_SETSIG, kKernelSigrtmax + 1));
    EXPECT_EQ(EINVAL, errno);

    // 校验失败不应修改现有值。
    EXPECT_EQ(0, fcntl(fd, F_GETSIG));

    close(fd);
}

// 对非法 fd 的 F_GETSIG/F_SETSIG 应返回 EBADF。
TEST(FcntlSignal, InvalidFdReturnsEBADF) {
    errno = 0;
    EXPECT_EQ(-1, fcntl(-1, F_GETSIG));
    EXPECT_EQ(EBADF, errno);

    errno = 0;
    EXPECT_EQ(-1, fcntl(-1, F_SETSIG, SIGUSR1));
    EXPECT_EQ(EBADF, errno);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}