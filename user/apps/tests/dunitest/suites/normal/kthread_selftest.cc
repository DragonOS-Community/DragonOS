#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kKthreadSelftestPath = "/sys/kernel/debug/kthread/selftest";

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

    EXPECT_EQ(0, close(fd)) << "close(" << path << ") failed: errno=" << errno << " ("
                            << strerror(errno) << ")";
    return content;
}

}  // namespace

TEST(KthreadSelftest, CreateRunStopAndReapHandshakes) {
    const std::string report = ReadAll(kKthreadSelftestPath);
    ASSERT_FALSE(report.empty());
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("create_stopped_stop=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("create_and_run_stop=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("quick_exit_512=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("quick_exit_completed=512\n")) << report;
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
