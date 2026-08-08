#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <unistd.h>

#include <string>

namespace {

constexpr const char* kSelftestPath = "/sys/kernel/debug/mm/dma_allocator_selftest";

std::string ReadAll() {
    int fd = open(kSelftestPath, O_RDONLY);
    EXPECT_GE(fd, 0) << strerror(errno);
    if (fd < 0) {
        return {};
    }

    std::string report;
    char buf[256];
    while (true) {
        const ssize_t n = read(fd, buf, sizeof(buf));
        if (n == 0) {
            break;
        }
        EXPECT_GT(n, 0) << strerror(errno);
        if (n < 0) {
            close(fd);
            return {};
        }
        report.append(buf, static_cast<size_t>(n));
    }
    EXPECT_EQ(close(fd), 0) << strerror(errno);
    return report;
}

void ExpectSuccessfulReport(const std::string& report) {
    EXPECT_NE(report.find("status=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("bounded_orders=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("bounded_candidate_selection=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("split_free_merge=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("fragmented_arena=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("dma32_zone=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("metadata_reuse=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("pool_mask_separation=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("pool_domain_separation=ok\n"), std::string::npos) << report;
    EXPECT_NE(report.find("summary_fail=0\n"), std::string::npos) << report;
}

}  // namespace

TEST(DmaAllocatorSelftest, BoundedAllocationAndReusePass) {
    const std::string report = ReadAll();
    ASSERT_FALSE(report.empty());
    ExpectSuccessfulReport(report);
}

TEST(DmaAllocatorSelftest, RepeatedRunsRemainSuccessful) {
    const std::string first = ReadAll();
    const std::string second = ReadAll();
    ASSERT_FALSE(first.empty());
    ASSERT_FALSE(second.empty());
    ExpectSuccessfulReport(first);
    ExpectSuccessfulReport(second);
    EXPECT_EQ(first, second);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
