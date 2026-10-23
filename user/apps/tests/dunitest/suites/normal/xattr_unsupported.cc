#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <fcntl.h>
#include <stdlib.h>
#include <string>
#include <sys/syscall.h>
#include <unistd.h>

namespace {
constexpr const char* kPath = "/proc/self/stat";
constexpr const char* kName = "user.dunitest_unsupported";

class XattrUnsupported : public ::testing::Test {
protected:
    void SetUp() override { fd = open(kPath, O_RDONLY | O_CLOEXEC); ASSERT_GE(fd, 0); }
    void TearDown() override { if (fd >= 0) close(fd); }
    int fd = -1;
};

TEST_F(XattrUnsupported, AttributeOperationsReportUnsupported) {
    if (geteuid() != 0) GTEST_SKIP() << "mutation checks require root to bypass mode 0444";
    const std::array<long, 3> get = {SYS_getxattr, SYS_lgetxattr, SYS_fgetxattr};
    const std::array<long, 3> set = {SYS_setxattr, SYS_lsetxattr, SYS_fsetxattr};
    const std::array<long, 3> remove = {SYS_removexattr, SYS_lremovexattr, SYS_fremovexattr};
    for (size_t i = 0; i < get.size(); ++i) {
        SCOPED_TRACE(i);
        const auto target = i == 2 ? static_cast<unsigned long>(fd)
                                  : reinterpret_cast<unsigned long>(kPath);
        errno = 0;
        EXPECT_EQ(-1, syscall(get[i], target, kName, nullptr, 0));
        EXPECT_EQ(EOPNOTSUPP, errno);
        errno = 0;
        EXPECT_EQ(-1, syscall(set[i], target, kName, "v", 1, 0));
        EXPECT_EQ(EOPNOTSUPP, errno);
        errno = 0;
        EXPECT_EQ(-1, syscall(remove[i], target, kName));
        EXPECT_EQ(EOPNOTSUPP, errno);
    }
}

TEST_F(XattrUnsupported, EmptyListDoesNotAccessOutput) {
    const std::array<long, 3> list = {SYS_listxattr, SYS_llistxattr, SYS_flistxattr};
    for (size_t i = 0; i < list.size(); ++i) {
        SCOPED_TRACE(i);
        const auto target = i == 2 ? static_cast<unsigned long>(fd)
                                  : reinterpret_cast<unsigned long>(kPath);
        std::array<char, 32> output;
        output.fill('x');
        EXPECT_EQ(0, syscall(list[i], target, nullptr, 0));
        EXPECT_EQ(0, syscall(list[i], target, output.data(), output.size()));
        for (const auto byte : output) EXPECT_EQ('x', byte);
        EXPECT_EQ(0, syscall(list[i], target, nullptr, output.size()));
    }
}
TEST_F(XattrUnsupported, NoFollowListsDanglingFinalSymlink) {
    char directory[] = "/tmp/dunitest_xattr_link_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(directory));
    const std::string link = std::string(directory) + "/link";
    const int linked = symlink("missing", link.c_str());
    if (linked != 0) {
        rmdir(directory);
        FAIL() << "symlink failed: " << errno;
    }
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_listxattr, link.c_str(), nullptr, 0));
    EXPECT_EQ(ENOENT, errno);
    EXPECT_EQ(0, syscall(SYS_llistxattr, link.c_str(), nullptr, 0));
    std::array<char, 16> output;
    output.fill('x');
    EXPECT_EQ(0, syscall(SYS_llistxattr, link.c_str(), output.data(), output.size()));
    for (const auto byte : output) EXPECT_EQ('x', byte);
    EXPECT_EQ(0, unlink(link.c_str()));
    EXPECT_EQ(0, rmdir(directory));
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
