#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kSelftestPath = "/sys/kernel/debug/timekeeping/selftest";

std::string ReadAll(int fd) {
    std::string output;
    char buffer[127];
    for (;;) {
        const ssize_t count = read(fd, buffer, sizeof(buffer));
        if (count == 0) {
            break;
        }
        if (count < 0) {
            ADD_FAILURE() << "read(" << kSelftestPath << ") failed: errno=" << errno;
            break;
        }
        output.append(buffer, static_cast<size_t>(count));
    }
    return output;
}

}  // namespace

TEST(TimekeepingSelftest, KernelPureAndTransactionalChecksPass) {
    const int fd = open(kSelftestPath, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << "open(" << kSelftestPath << ") failed: errno=" << errno;
    const std::string report = ReadAll(fd);
    ASSERT_EQ(0, close(fd));

    ASSERT_FALSE(report.empty());
    EXPECT_EQ(0u, report.find("status=ok\n")) << report;
    EXPECT_EQ(std::string::npos, report.find("=fail\n")) << report;
    EXPECT_NE(std::string::npos, report.find("timekeeping.wrap_24=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("timekeeping.switch_continuity=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("timekeeping.settimeofday_domains=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("rwlock.ticket_fifo=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("rwlock.upgrader_reader_limit=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("rwlock.pending_blocks_new_owners=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("kvm_allocator.staged_success=ok\n")) << report;
    EXPECT_NE(std::string::npos,
              report.find("kvm_allocator.mapping_failure_releases_frame=ok\n"))
        << report;
    EXPECT_NE(std::string::npos, report.find("summary_fail=0\n")) << report;
}

TEST(TimekeepingSelftest, ReportIsPerOpenSnapshotAndReadOnly) {
    const int first = open(kSelftestPath, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(first, 0);
    const std::string first_report = ReadAll(first);
    ASSERT_EQ(0, close(first));

    const int second = open(kSelftestPath, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(second, 0);
    const std::string second_report = ReadAll(second);
    ASSERT_EQ(0, close(second));
    EXPECT_EQ(first_report, second_report);

    const int writable = open(kSelftestPath, O_WRONLY | O_CLOEXEC);
    ASSERT_GE(writable, 0) << "root open for callback test failed: errno=" << errno;
    errno = 0;
    EXPECT_EQ(-1, write(writable, "x", 1));
    EXPECT_EQ(EPERM, errno);
    EXPECT_EQ(0, close(writable));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
