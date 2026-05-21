#include <gtest/gtest.h>
#include <unistd.h>
#include <errno.h>
#include <sys/stat.h>
#include <cstdio>
#include <cstring>

namespace {

    constexpr const char* kProcSource = "/proc/self/status";
    constexpr const char* kTestLink = "/proc/self/status2";

} // namespace

TEST(ProcfsLink, LinkToProcFileReturnsEPERM) {
    // 确保测试开始前目标文件不存在（ENOENT 是可接受的）
    int ret = unlink(kTestLink);
    if (ret != 0 && errno != ENOENT) {
        FAIL() << "unlink failed with unexpected error: errno=" << errno
            << " (" << std::strerror(errno) << ")";
    }

    // 尝试创建硬链接到 procfs 文件
    int link_ret = link(kProcSource, kTestLink);

    EXPECT_EQ(-1, link_ret);
    EXPECT_EQ(EPERM, errno);
    EXPECT_NE(0, access(kTestLink, F_OK));

    // 清理（同样处理 ENOENT）
    ret = unlink(kTestLink);
    if (ret != 0 && errno != ENOENT) {
        ADD_FAILURE() << "cleanup unlink failed: errno=" << errno
            << " (" << std::strerror(errno) << ")";
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}