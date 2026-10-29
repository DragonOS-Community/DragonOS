#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <unistd.h>

#include <sstream>
#include <string>

TEST(PageReporting, KernelOwnershipChecks) {
    const char* path = "/sys/kernel/debug/mm/page_reporting_selftest";
    const int fd = open(path, O_RDONLY | O_CLOEXEC);
    if (fd < 0 && errno == ENOENT) {
        GTEST_SKIP() << "requires KERNEL_FEATURES=mm_selftest";
    }
    ASSERT_GE(fd, 0) << "open " << path << " errno=" << errno;
    std::string report;
    char buffer[127];
    for (;;) {
        const ssize_t count = read(fd, buffer, sizeof(buffer));
        if (count < 0 && errno == EINTR) continue;
        if (count <= 0) {
            if (count < 0) ADD_FAILURE() << "read errno=" << errno;
            break;
        }
        report.append(buffer, static_cast<size_t>(count));
        if (report.size() > 16384) {
            ADD_FAILURE() << "unexpectedly large selftest report";
            break;
        }
    }
    ASSERT_EQ(0, close(fd));
    ASSERT_FALSE(report.empty());
    EXPECT_EQ(0u, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("summary_fail=0\n")) << report;
    std::istringstream lines(report);
    std::string line;
    size_t checks = 0;
    while (std::getline(lines, line)) {
        if (line == "status=ok" || line == "summary_fail=0") continue;
        ++checks;
        EXPECT_GT(line.size(), 3u) << report;
        EXPECT_EQ(line.size() - 3, line.rfind("=ok")) << report;
    }
    for (const char* check : {"ownership", "reported_reuse", "scan_progress",
                              "metadata_lifetime", "reserve", "discard_putback",
                              "accounting"}) {
        EXPECT_NE(std::string::npos, report.find(std::string(check) + "=ok\n"))
            << report;
    }
    EXPECT_GE(checks, 7u) << "all ownership and accounting checks must execute";
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
