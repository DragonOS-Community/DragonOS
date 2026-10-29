#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

TEST(SysrqTrigger, TaskDumpAndNoopPosition) {
    if (geteuid() != 0) GTEST_SKIP() << "requires global administrator";
    int fd = open("/proc/sysrq-trigger", O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << errno;
    struct stat st = {};
    ASSERT_EQ(fstat(fd, &st), 0);
    EXPECT_EQ(st.st_mode & 0777, 0200);
    EXPECT_EQ(write(fd, "", 0), 0);
    EXPECT_EQ(write(fd, "t\n", 2), 2);
    EXPECT_EQ(lseek(fd, 123, SEEK_SET), 0);
    EXPECT_EQ(write(fd, "?t", 2), 2);  // Only the first byte is dispatched.
    EXPECT_EQ(lseek(fd, 0, SEEK_CUR), 0);
    EXPECT_EQ(close(fd), 0);
}

TEST(SysrqTrigger, ReceivedFdDoesNotGrantGlobalAuthority) {
    if (geteuid() != 0) GTEST_SKIP() << "requires global administrator";
    int fd = open("/proc/sysrq-trigger", O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << errno;
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(1001) != 0 || setuid(1001) != 0) _exit(1);
        errno = 0;
        if (write(fd, "t", 1) != -1 || errno != EPERM) _exit(2);
        close(fd);
        errno = 0;
        fd = open("/proc/sysrq-trigger", O_WRONLY);
        _exit(fd == -1 && errno == EACCES ? 0 : 3);
    }
    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child);
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
    EXPECT_EQ(close(fd), 0);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
