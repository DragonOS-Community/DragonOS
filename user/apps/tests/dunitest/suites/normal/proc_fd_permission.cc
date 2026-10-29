#include <gtest/gtest.h>
#include "cap_common.h"
#include <errno.h>
#include <fcntl.h>
#include <grp.h>
#include <stdlib.h>
#include <string.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <string>

namespace {
class Fd {
public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    void reset(int fd = -1) { if (fd_ >= 0) close(fd_); fd_ = fd; }
private:
    int fd_;
};

#define CHILD_CHECK(condition) \
    do { if (!(condition)) { \
        dprintf(STDERR_FILENO, "child line %d errno %d (%s)\n", __LINE__, errno, strerror(errno)); \
        _exit(1); \
    } } while (0)

void drop_to_user() {
    CHILD_CHECK(setgroups(0, nullptr) == 0 && setgid(1001) == 0 && setuid(1001) == 0);
    cap_user_data_t zero[2] = {};
    CHILD_CHECK(capset_errno(_LINUX_CAPABILITY_VERSION_3, 0, zero) == 0);
}

template <typename Action>
void worker(Action action) {
    const pid_t child = fork(); ASSERT_GE(child, 0);
    if (child == 0) { action(); _exit(0); }
    int status = 0; ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)); EXPECT_EQ(0, WEXITSTATUS(status));
}

class ProcFdPermissionTest : public ::testing::Test {
protected:
    void SetUp() override {
        char name[] = "/tmp/proc_fd_permission_XXXXXX";
        file_.reset(mkstemp(name)); ASSERT_GE(file_.get(), 0);
        path_ = name;
        ASSERT_EQ(0, fchmod(file_.get(), 0644));
        ASSERT_EQ(4, write(file_.get(), "data", 4));
    }
    void TearDown() override {
        if (target_ > 0) stop_target();
        if (!path_.empty()) { EXPECT_EQ(0, unlink(path_.c_str())); }
    }
    void start_target() {
        int command[2]; ASSERT_EQ(0, pipe(command));
        Fd command_read(command[0]); command_.reset(command[1]);
        int ack[2]; ASSERT_EQ(0, pipe(ack));
        ack_.reset(ack[0]); Fd ack_write(ack[1]);
        target_ = fork(); ASSERT_GE(target_, 0);
        if (target_ == 0) {
            command_.reset(); ack_.reset();
            char value = 'r';
            if (write(ack_write.get(), &value, 1) != 1) _exit(2);
            while (read(command_read.get(), &value, 1) == 1) {
                if (value != 'c' || close(file_.get()) != 0) _exit(3);
                if (write(ack_write.get(), &value, 1) != 1) _exit(4);
            }
            _exit(0);
        }
        char ready;
        ASSERT_EQ(1, read(ack_.get(), &ready, 1)); ASSERT_EQ('r', ready);
        link_ = "/proc/" + std::to_string(target_) + "/fd/" + std::to_string(file_.get());
    }
    void stop_target() {
        // EOF also safely releases the child when an earlier assertion failed;
        // unlike writing an exit command it cannot SIGPIPE if the child died.
        command_.reset();
        int status = 0; EXPECT_EQ(target_, waitpid(target_, &status, 0));
        EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
        target_ = -1;
        ack_.reset();
    }
    Fd file_, command_, ack_;
    pid_t target_ = -1;
    std::string path_, link_;
};

TEST_F(ProcFdPermissionTest, SameThreadGroupRetainsAccessAfterCredentialDrop) {
    ASSERT_NO_FATAL_FAILURE(worker([&] {
        const std::string link = "/proc/self/fd/" + std::to_string(file_.get());
        // Materialize root-owned proc entries before setuid; ordinary DAC on
        // this cached fd directory must not prevent the same-TGID exception.
        Fd directory(open("/proc/self/fd", O_RDONLY | O_DIRECTORY));
        CHILD_CHECK(directory.get() >= 0);
        Fd held(open(link.c_str(), O_PATH | O_NOFOLLOW)); CHILD_CHECK(held.get() >= 0);
        drop_to_user();
        Fd after(open("/proc/self/fd", O_RDONLY | O_DIRECTORY)); CHILD_CHECK(after.get() >= 0);
        char bytes[4096];
        ssize_t size = readlink(link.c_str(), bytes, sizeof(bytes));
        CHILD_CHECK(size == static_cast<ssize_t>(path_.size()) &&
                    memcmp(bytes, path_.data(), path_.size()) == 0);
        size = readlinkat(held.get(), "", bytes, sizeof(bytes));
        CHILD_CHECK(size == static_cast<ssize_t>(path_.size()) &&
                    memcmp(bytes, path_.data(), path_.size()) == 0);
        Fd reopened(open(link.c_str(), O_RDONLY)); CHILD_CHECK(reopened.get() >= 0);
        struct stat before {}, actual {};
        CHILD_CHECK(fstat(file_.get(), &before) == 0 && fstat(reopened.get(), &actual) == 0);
        CHILD_CHECK(before.st_dev == actual.st_dev && before.st_ino == actual.st_ino);
        CHILD_CHECK(read(reopened.get(), bytes, 4) == 4 && memcmp(bytes, "data", 4) == 0);
    }));
}

TEST_F(ProcFdPermissionTest, OtherUidCannotTraverseOrReadHeldMagicLinkWithoutCapability) {
    ASSERT_NO_FATAL_FAILURE(start_target());
    Fd held(open(link_.c_str(), O_PATH | O_NOFOLLOW)); ASSERT_GE(held.get(), 0);
    ASSERT_NO_FATAL_FAILURE(worker([&] {
        drop_to_user();
        char bytes[4096];
        CHILD_CHECK(readlink(link_.c_str(), bytes, sizeof(bytes)) == -1 && errno == EACCES);
        CHILD_CHECK(open(link_.c_str(), O_RDONLY) == -1 && errno == EACCES);
        // O_PATH skips final-inode checks, not traversal of target's fd dir.
        CHILD_CHECK(open(link_.c_str(), O_PATH | O_NOFOLLOW) == -1 && errno == EACCES);
        // The path fd acquired while privileged bypasses directory traversal,
        // but readlink still checks the current caller's PTRACE_READ_FSCREDS.
        CHILD_CHECK(readlinkat(held.get(), "", bytes, sizeof(bytes)) == -1 && errno == EACCES);
    }));
}

TEST_F(ProcFdPermissionTest, HeldLinkDistinguishesClosedDescriptorFromExitedTarget) {
    ASSERT_NO_FATAL_FAILURE(start_target());
    Fd held(open(link_.c_str(), O_PATH | O_NOFOLLOW)); ASSERT_GE(held.get(), 0);
    char bytes[4096];
    ASSERT_GT(readlinkat(held.get(), "", bytes, sizeof(bytes)), 0);
    const char close_command = 'c';
    ASSERT_EQ(1, write(command_.get(), &close_command, 1));
    char ack; ASSERT_EQ(1, read(ack_.get(), &ack, 1)); ASSERT_EQ('c', ack);
    ASSERT_EQ(-1, readlinkat(held.get(), "", bytes, sizeof(bytes))); EXPECT_EQ(ENOENT, errno);
    ASSERT_NO_FATAL_FAILURE(stop_target());
    ASSERT_EQ(-1, readlinkat(held.get(), "", bytes, sizeof(bytes))); EXPECT_EQ(EACCES, errno);
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
