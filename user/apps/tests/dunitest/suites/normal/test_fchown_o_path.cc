#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <sys/syscall.h>
#include <sys/types.h>
#include <unistd.h>

namespace {

// RAII helper for test file cleanup
class TestFileGuard {
public:
    explicit TestFileGuard(const char* path) : path_(path) {}
    ~TestFileGuard() {
        if (path_) {
            unlink(path_);
        }
    }
    TestFileGuard(const TestFileGuard&) = delete;
    TestFileGuard& operator=(const TestFileGuard&) = delete;

private:
    const char* path_;
};

// Test: Raw fchown syscall on O_PATH fd should return EBADF
TEST(FchownOPathTest, RawSyscallReturnsEBADF) {
    const char* test_file = "/tmp/test_fchown_o_path.txt";
    uid_t test_uid = 1000;
    gid_t test_gid = 1000;

    // Create test file (will be auto-cleaned by TestFileGuard)
    int fd = creat(test_file, 0644);
    ASSERT_GE(fd, 0) << "Failed to create test file: " << strerror(errno);
    close(fd);

    TestFileGuard guard(test_file);  // RAII cleanup

    // Open file with O_PATH flag
    int o_path_fd = open(test_file, O_PATH);
    ASSERT_GE(o_path_fd, 0) << "Failed to open file with O_PATH: " << strerror(errno);

    // Test raw SYS_fchown syscall on O_PATH fd
    errno = 0;
    long ret = syscall(SYS_fchown, o_path_fd, test_uid, test_gid);

    // Should fail with EBADF
    EXPECT_EQ(ret, -1) << "SYS_fchown on O_PATH fd should return -1";
    EXPECT_EQ(errno, EBADF) << "SYS_fchown on O_PATH fd should set errno to EBADF, got: " << strerror(errno);

    close(o_path_fd);
    // TestFileGuard will automatically unlink test_file
}

// Test: Regular fchown on normal fd should work
TEST(FchownOPathTest, NormalFdWorks) {
    const char* test_file = "/tmp/test_fchown_normal.txt";
    uid_t test_uid = 1000;
    gid_t test_gid = 1000;

    // Create test file (will be auto-cleaned by TestFileGuard)
    int fd = creat(test_file, 0644);
    ASSERT_GE(fd, 0) << "Failed to create test file: " << strerror(errno);

    TestFileGuard guard(test_file);  // RAII cleanup

    // Test raw SYS_fchown syscall on normal fd
    errno = 0;
    long ret = syscall(SYS_fchown, fd, test_uid, test_gid);

    // Should succeed or fail with EPERM (permission denied), but NOT EBADF
    if (ret == -1) {
        EXPECT_NE(errno, EBADF) << "SYS_fchown on normal fd should not return EBADF, got: " << strerror(errno);
    }

    close(fd);
    // TestFileGuard will automatically unlink test_file
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
