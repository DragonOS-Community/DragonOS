#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

TEST(CredentialOverrideSelftest, ControlledScopeAndOwnedSnapshot) {
    constexpr const char* path = "/sys/kernel/debug/credentials/selftest";
    int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0 && errno == ENOENT) {
        GTEST_SKIP() << "requires a kernel built with cred_selftest";
    }
    ASSERT_GE(fd, 0) << "errno=" << errno;
    struct stat st = {};
    EXPECT_EQ(0, fstat(fd, &st));
    EXPECT_EQ(static_cast<mode_t>(0400), st.st_mode & 0777);

    std::string report;
    char buffer[37];
    bool read_ok = true;
    while (true) {
        ssize_t size = read(fd, buffer, sizeof(buffer));
        if (size < 0 && errno == EINTR) {
            continue;
        }
        if (size < 0) {
            ADD_FAILURE() << "read errno=" << errno;
            read_ok = false;
            break;
        }
        if (size == 0) {
            break;
        }
        report.append(buffer, static_cast<size_t>(size));
        if (report.size() > 4096) {
            ADD_FAILURE() << "selftest report exceeds its bounded snapshot";
            read_ok = false;
            break;
        }
    }
    if (read_ok) {
        EXPECT_EQ(0, read(fd, buffer, sizeof(buffer))) << "snapshot EOF changed";
    }
    EXPECT_EQ(0, close(fd));
    ASSERT_TRUE(read_ok);
    ASSERT_FALSE(report.empty());
    EXPECT_EQ(0U, report.find("status=ok\npassed=8\nfailed=0\n")) << report;
    for (const char* item : {"objective_window", "nested_distinct", "nested_same",
                             "result_error_restore", "captured_snapshot_lifetime",
                             "irq_disabled_read", "task_isolation", "final_restore"}) {
        EXPECT_NE(std::string::npos, report.find(std::string(item) + "=ok\n")) << report;
    }
    EXPECT_NE(std::string::npos, report.find("migration=not_proven\n"));
}

TEST(CredentialOverrideSelftest, UnprivilegedOpenIsDenied) {
    constexpr const char* path = "/sys/kernel/debug/credentials/selftest";
    struct stat st = {};
    if (stat(path, &st) < 0 && errno == ENOENT) {
        GTEST_SKIP() << "requires a kernel built with cred_selftest";
    }
    if (geteuid() != 0) {
        GTEST_SKIP() << "requires root to drop credentials in a child";
    }
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(1);
        int fd = open(path, O_RDONLY | O_CLOEXEC);
        int error = errno;
        if (fd >= 0) close(fd);
        _exit(fd < 0 && (error == EACCES || error == EPERM) ? 0 : 2);
    }
    int status = 0;
    pid_t result;
    do {
        result = waitpid(child, &status, 0);
    } while (result < 0 && errno == EINTR);
    ASSERT_EQ(child, result);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
