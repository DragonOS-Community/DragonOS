#include <gtest/gtest.h>

#include <cerrno>
#include <climits>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <sched.h>
#include <sys/wait.h>
#include <unistd.h>

#include <array>
#include <string>

#ifndef CLONE_NEWUSER
#define CLONE_NEWUSER 0x10000000
#endif

namespace {

constexpr const char* kNames[] = {
    "maxkeys", "maxbytes", "root_maxkeys", "root_maxbytes", "gc_delay",
};

std::string path_for(const char* name) {
    return std::string("/proc/sys/kernel/keys/") + name;
}

int read_value(const char* name) {
    int fd = open(path_for(name).c_str(), O_RDONLY);
    if (fd < 0) return -1;
    char text[64] = {};
    const ssize_t count = read(fd, text, sizeof(text) - 1);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (count <= 0) return -1;
    char* end = nullptr;
    const long value = strtol(text, &end, 10);
    return end == text || value < 0 || value > INT_MAX ? -1 : static_cast<int>(value);
}

ssize_t write_value(const char* name, const char* text) {
    int fd = open(path_for(name).c_str(), O_WRONLY);
    if (fd < 0) return -1;
    const ssize_t written = write(fd, text, strlen(text));
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return written;
}

class KeysSysctlTest : public ::testing::Test {
protected:
    std::array<int, 5> original_{};

    void SetUp() override {
        original_.fill(-1);
        ASSERT_EQ(0u, geteuid());
        for (size_t i = 0; i < original_.size(); ++i) {
            original_[i] = read_value(kNames[i]);
            ASSERT_GE(original_[i], 0) << kNames[i] << ": " << strerror(errno);
        }
    }

    void TearDown() override {
        for (size_t i = 0; i < original_.size(); ++i) {
            if (original_[i] < 0 || read_value(kNames[i]) == original_[i]) continue;
            const std::string text = std::to_string(original_[i]) + "\n";
            EXPECT_EQ(static_cast<ssize_t>(text.size()),
                      write_value(kNames[i], text.c_str()))
                << kNames[i] << ": " << strerror(errno);
        }
    }
};

TEST_F(KeysSysctlTest, LimitsUseLinuxRangesAndChangesAreVisible) {
    for (size_t i = 0; i < original_.size(); ++i) {
        const char* name = kNames[i];
        errno = 0;
        EXPECT_EQ(-1, write_value(name, "-1\n")) << name;
        EXPECT_EQ(EINVAL, errno) << name;
        errno = 0;
        EXPECT_EQ(-1, write_value(name, "2147483648\n")) << name;
        EXPECT_EQ(EINVAL, errno) << name;
        if (i != original_.size() - 1) {
            errno = 0;
            EXPECT_EQ(-1, write_value(name, "0\n")) << name;
            EXPECT_EQ(EINVAL, errno) << name;
        }
        EXPECT_EQ(original_[i], read_value(name)) << name;
    }

    ASSERT_EQ(2, write_value("gc_delay", "0\n")) << strerror(errno);
    EXPECT_EQ(0, read_value("gc_delay"));
    const int candidate = original_[0] == INT_MAX ? INT_MAX - 1 : original_[0] + 1;
    const std::string new_maxkeys = std::to_string(candidate) + "\n";
    ASSERT_EQ(static_cast<ssize_t>(new_maxkeys.size()),
              write_value("maxkeys", new_maxkeys.c_str())) << strerror(errno);
    EXPECT_EQ(candidate, read_value("maxkeys"));
}

TEST_F(KeysSysctlTest, ChildUserNamespaceCannotWriteGlobalQuota) {
    int inherited_fd = open(path_for("maxkeys").c_str(), O_WRONLY);
    ASSERT_GE(inherited_fd, 0) << strerror(errno);
    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0) _exit(10);
        if (setuid(1000) != 0) _exit(11);
        if (unshare(CLONE_NEWUSER) != 0) _exit(12);

        errno = 0;
        if (pwrite(inherited_fd, "200\n", 4, 0) != -1 || errno != EPERM)
            _exit(13);
        close(inherited_fd);

        errno = 0;
        const int reopened = open(path_for("maxkeys").c_str(), O_WRONLY);
        if (reopened >= 0) {
            close(reopened);
            _exit(14);
        }
        _exit(errno == EACCES || errno == EPERM ? 0 : 15);
    }
    close(inherited_fd);

    int status = 0;
    pid_t waited = -1;
    do {
        waited = waitpid(child, &status, 0);
    } while (waited < 0 && errno == EINTR);
    ASSERT_EQ(child, waited) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(original_[0], read_value("maxkeys"));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
