#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <array>
#include <sstream>
#include <string>

namespace {

constexpr const char* kStatsPath = "/sys/kernel/debug/mm/allocation_stats";
constexpr const char* kSelftestPath = "/sys/kernel/debug/mm/allocation_selftest";
constexpr size_t kOrders = 19;

std::string ReadFd(int fd, size_t chunk_size) {
    std::string report;
    char buffer[256];
    while (true) {
        const ssize_t count = read(fd, buffer, chunk_size);
        if (count == 0) {
            break;
        }
        EXPECT_GT(count, 0) << strerror(errno);
        if (count < 0) {
            break;
        }
        report.append(buffer, static_cast<size_t>(count));
    }
    return report;
}

std::string ReadPath(const char* path) {
    const int fd = open(path, O_RDONLY);
    EXPECT_GE(fd, 0) << strerror(errno);
    if (fd < 0) {
        return {};
    }
    const auto report = ReadFd(fd, 256);
    EXPECT_EQ(close(fd), 0) << strerror(errno);
    return report;
}

void CheckStats(const std::string& report) {
    EXPECT_NE(report.find("version=1\n"), std::string::npos) << report;
    EXPECT_NE(report.find("order_unit=base_pages\n"), std::string::npos) << report;
    EXPECT_NE(report.find("request_unit=buddy_attempt\n"), std::string::npos) << report;
    EXPECT_NE(report.find("snapshot_end=1\n"), std::string::npos) << report;

    std::array<unsigned long long, 2> declared_pages{};
    std::array<unsigned long long, 2> counted_pages{};
    std::array<std::array<bool, kOrders>, 2> seen_zones{};
    std::array<bool, kOrders> seen_requests{};
    unsigned int free_page_lines = 0;
    unsigned int reason_lines = 0;
    std::istringstream stream(report);
    std::string line;
    while (std::getline(stream, line)) {
        char zone[16]{};
        unsigned int order = 0;
        unsigned long long blocks = 0;
        unsigned long long attempts = 0;
        unsigned long long failures = 0;
        if (sscanf(line.c_str(), "zone %15s order %u free_blocks %llu attempts %llu misses %llu",
                   zone, &order, &blocks, &attempts, &failures) == 5) {
            ASSERT_TRUE(strcmp(zone, "DMA32") == 0 || strcmp(zone, "Normal") == 0);
            const size_t index = strcmp(zone, "DMA32") == 0 ? 0 : 1;
            ASSERT_LT(order, kOrders);
            EXPECT_FALSE(seen_zones[index][order]);
            seen_zones[index][order] = true;
            counted_pages[index] += blocks << order;
            EXPECT_LE(failures, attempts);
        } else if (sscanf(line.c_str(), "free_pages %15s %llu", zone, &blocks) == 2) {
            ASSERT_TRUE(strcmp(zone, "DMA32") == 0 || strcmp(zone, "Normal") == 0);
            declared_pages[strcmp(zone, "DMA32") == 0 ? 0 : 1] = blocks;
            ++free_page_lines;
        } else if (sscanf(line.c_str(), "request order %u attempts %llu failures %llu", &order,
                          &attempts, &failures) == 3) {
            ASSERT_LT(order, kOrders);
            EXPECT_FALSE(seen_requests[order]);
            seen_requests[order] = true;
            EXPECT_LE(failures, attempts);
        } else if (line.compare(0, 8, "failure ") == 0) {
            ++reason_lines;
        }
    }
    EXPECT_EQ(free_page_lines, 2u);
    EXPECT_EQ(reason_lines, 6u);
    EXPECT_EQ(counted_pages, declared_pages);
    for (size_t order = 0; order < kOrders; ++order) {
        EXPECT_TRUE(seen_requests[order]) << order;
        EXPECT_TRUE(seen_zones[0][order]) << order;
        EXPECT_TRUE(seen_zones[1][order]) << order;
    }
}

}  // namespace

TEST(AllocationDiagnostics, SnapshotIsCompleteAndStableAcrossPartialReads) {
    const int fd = open(kStatsPath, O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    const auto report = ReadFd(fd, 7);
    EXPECT_EQ(lseek(fd, 0, SEEK_SET), 0) << strerror(errno);
    const auto reread = ReadFd(fd, 256);
    EXPECT_EQ(close(fd), 0) << strerror(errno);
    EXPECT_EQ(report, reread);
    CheckStats(report);
}

TEST(AllocationDiagnostics, PrivateArenaValidatesFailureClassesAndCountConservation) {
    struct stat info;
    if (stat(kSelftestPath, &info) < 0) {
        const int error = errno;
        if (error == ENOENT) {
            GTEST_SKIP() << "build the kernel with KERNEL_FEATURES=mm_selftest";
        }
        FAIL() << "stat(" << kSelftestPath << "): " << strerror(error);
    }
    const auto first = ReadPath(kSelftestPath);
    EXPECT_NE(first.find("status=ok\n"), std::string::npos) << first;
    EXPECT_NE(first.find("fragmentation_and_capacity_diagnostics=ok\n"), std::string::npos) << first;
    EXPECT_NE(first.find("zone_reserve_and_address_diagnostics=ok\n"), std::string::npos) << first;
    EXPECT_NE(first.find("metadata_count_conservation=ok\n"), std::string::npos) << first;
    EXPECT_NE(first.find("summary_fail=0\n"), std::string::npos) << first;
    EXPECT_EQ(first, ReadPath(kSelftestPath));
    CheckStats(ReadPath(kStatsPath));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
