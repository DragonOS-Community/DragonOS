#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kErrSeqSelftestPath = "/sys/kernel/debug/errseq/selftest";

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

void ExpectReportOk(const std::string& report) {
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("unseen_sample=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("multi_watcher=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("late_sample=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("pagecache_multi_fd=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("syncfs_sb_cursor=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("sync_file_range_wait=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("msync_range=ok\n")) << report;
}

}  // namespace

TEST(ErrSeqSelftest, ReportIsPresentAndSuccessful) {
    const std::string report = ReadAll(kErrSeqSelftestPath);
    ASSERT_FALSE(report.empty());
    ExpectReportOk(report);
}

TEST(ErrSeqSelftest, ReportIsStableAcrossReads) {
    const std::string first = ReadAll(kErrSeqSelftestPath);
    const std::string second = ReadAll(kErrSeqSelftestPath);

    ASSERT_FALSE(first.empty());
    ASSERT_FALSE(second.empty());
    ExpectReportOk(first);
    ExpectReportOk(second);
    EXPECT_EQ(first, second);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
