#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kRcuSelftestPath = "/sys/kernel/debug/rcu/selftest";
constexpr const char* kRcuCallbacksPath = "/sys/kernel/debug/rcu/callbacks";

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
    EXPECT_NE(std::string::npos, report.find("pr1=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("pr2=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("pr3=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("pr5=ok\n")) << report;
}

}  // namespace

TEST(RcuSelftest, ReportIsPresentAndSuccessful) {
    const std::string report = ReadAll(kRcuSelftestPath);
    ASSERT_FALSE(report.empty());
    ExpectReportOk(report);
}

TEST(RcuSelftest, ReportIsStableAcrossReads) {
    const std::string first = ReadAll(kRcuSelftestPath);
    const std::string second = ReadAll(kRcuSelftestPath);

    ASSERT_FALSE(first.empty());
    ASSERT_FALSE(second.empty());
    ExpectReportOk(first);
    ExpectReportOk(second);
    EXPECT_EQ(first, second);
}

TEST(RcuSelftest, CallbackQueueSnapshotIsPresentAndStablePerOpen) {
    const std::string report = ReadAll(kRcuCallbacksPath);
    ASSERT_FALSE(report.empty());
    EXPECT_NE(std::string::npos, report.find("aggregate total=")) << report;
    EXPECT_NE(std::string::npos, report.find(" done=")) << report;
    EXPECT_NE(std::string::npos, report.find(" wait=")) << report;
    EXPECT_NE(std::string::npos, report.find(" next_ready=")) << report;
    EXPECT_NE(std::string::npos, report.find(" next=")) << report;
    EXPECT_NE(std::string::npos, report.find(" executing=")) << report;
    EXPECT_NE(std::string::npos, report.find("cpu=0 total=")) << report;
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
