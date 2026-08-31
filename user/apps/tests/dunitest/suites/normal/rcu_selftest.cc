#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kRcuSelftestPath = "/sys/kernel/debug/rcu/selftest";
constexpr const char* kRcuCallbacksPath = "/sys/kernel/debug/rcu/callbacks";
constexpr const char* kRcuStatePath = "/sys/kernel/debug/rcu/state";
constexpr const char* kRcuStatsPath = "/sys/kernel/debug/rcu/stats";
constexpr const char* kSrcuStatePath = "/sys/kernel/debug/rcu/srcu/state";

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
    const long online_cpus = sysconf(_SC_NPROCESSORS_ONLN);
    ASSERT_GT(online_cpus, 0) << "sysconf(_SC_NPROCESSORS_ONLN) failed: errno=" << errno << " ("
                              << strerror(errno) << ")";
    if (online_cpus == 1) {
        EXPECT_NE(std::string::npos, report.find("smp_litmus=skip:no-remote-cpu\n")) << report;
    } else {
        EXPECT_NE(std::string::npos, report.find("smp_litmus=ok\n")) << report;
    }
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

TEST(RcuSelftest, ProgressStateSnapshotIsPresent) {
    const std::string report = ReadAll(kRcuStatePath);
    ASSERT_FALSE(report.empty());
    EXPECT_NE(std::string::npos, report.find("active=")) << report;
    EXPECT_NE(std::string::npos, report.find("current_seq=")) << report;
    EXPECT_NE(std::string::npos, report.find("completed_seq=")) << report;
    EXPECT_NE(std::string::npos, report.find("next_progress_ns=")) << report;
}

TEST(RcuSelftest, ProgressStatisticsArePresent) {
    const std::string report = ReadAll(kRcuStatsPath);
    ASSERT_FALSE(report.empty());
    EXPECT_NE(std::string::npos, report.find("gp_started=")) << report;
    EXPECT_NE(std::string::npos, report.find("gp_completed=")) << report;
    EXPECT_NE(std::string::npos, report.find("ipi_attempted=")) << report;
    EXPECT_NE(std::string::npos, report.find("callback_time_budget_hits=")) << report;
    EXPECT_NE(std::string::npos, report.find("slow_callbacks=")) << report;
}

TEST(RcuSelftest, SrcuDomainsArePresentAndQuiescent) {
    // Opening the shared selftest file runs the deterministic SRCU state and
    // runtime checks before this state snapshot is inspected.
    const std::string selftest = ReadAll(kRcuSelftestPath);
    ASSERT_FALSE(selftest.empty());
    ExpectReportOk(selftest);

    const std::string state = ReadAll(kSrcuStatePath);
    ASSERT_FALSE(state.empty());
    for (const char* name : {"name=tracepoint", "name=reboot_notifier"}) {
        const size_t start = state.find(name);
        ASSERT_NE(std::string::npos, start) << state;
        const size_t end = state.find('\n', start);
        const std::string line = state.substr(start, end - start);
        EXPECT_NE(std::string::npos, line.find("active=true")) << line;
        EXPECT_NE(std::string::npos, line.find("callbacks=0")) << line;
        EXPECT_NE(std::string::npos, line.find("executing=false")) << line;
    }
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
