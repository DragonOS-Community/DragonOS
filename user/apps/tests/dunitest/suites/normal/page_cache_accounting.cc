#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kSelftestPath = "/sys/kernel/debug/page_cache/accounting_selftest";

std::string ReadAll(const char* path) {
    int fd = open(path, O_RDONLY);
    EXPECT_GE(fd, 0) << "open(" << path << ") failed: errno=" << errno << " (" << strerror(errno)
                     << ")";
    if (fd < 0) {
        return {};
    }

    std::string content;
    char buf[256];
    while (true) {
        ssize_t n = read(fd, buf, sizeof(buf));
        if (n == 0) {
            break;
        }
        EXPECT_GT(n, 0) << "read(" << path << ") failed: errno=" << errno << " ("
                        << strerror(errno) << ")";
        if (n <= 0) {
            close(fd);
            return {};
        }
        content.append(buf, static_cast<size_t>(n));
    }
    EXPECT_EQ(0, close(fd)) << strerror(errno);
    return content;
}

}  // namespace

TEST(PageCacheAccounting, MembershipLifecycleIsBalanced) {
    const std::string report = ReadAll(kSelftestPath);
    ASSERT_FALSE(report.empty());
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("file_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("shmem_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("dirty_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("writeback_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("unevictable_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("inflight_teardown=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("late_completion=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("global_wiring=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("layout=ok\n")) << report;
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
