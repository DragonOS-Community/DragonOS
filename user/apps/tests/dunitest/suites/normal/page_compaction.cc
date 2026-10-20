#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <string.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>

namespace {
constexpr const char* kReport = "/sys/kernel/debug/mm/page_compaction_selftest";

std::string ReadReport(int fd, size_t chunk) {
    std::string report;
    char buffer[256];
    for (;;) {
        const ssize_t count = read(fd, buffer, chunk);
        if (count < 0 && errno == EINTR) continue;
        if (count == 0) return report;
        if (count < 0) {
            ADD_FAILURE() << "read compaction selftest: " << strerror(errno);
            return report;
        }
        report.append(buffer, static_cast<size_t>(count));
    }
}

void CheckReport(const std::string& report) {
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("summary_fail=0\n")) << report;
    for (const char* key : {
            "real_fragmented_capture", "partial_pin_cleanup", "migration_budget_cleanup",
            "unknown_blocker", "rmap_budget", "deferred_free_fastpath",
            "scan_budget_no_mutation", "isolation_allocation_free_window",
            "atomic_rejected", "seed_boundaries_no_mutation", "repeated_capture",
            "dma_same_mask_fragmentation", "dma_atomic_no_recovery",
            "dma_noio_no_recovery", "dma_automatic_recovery",
            "buddy_compaction_split_capture", "buddy_compaction_metadata_slots",
            "buddy_compaction_abort_stale", "buddy_compaction_cross_free",
            "buddy_compaction_budget_rollback", "buddy_compaction_range_validation",
            "buddy_compaction_alloc_exclusion", "buddy_compaction_zone_scratch_reuse"}) {
        EXPECT_NE(std::string::npos, report.find(std::string(key) + "=ok\n")) << report;
    }
}
}  // namespace

TEST(PageCompaction, RealBuddyOwnershipBudgetsAndRepeatedExecution) {
    struct stat info;
    if (stat(kReport, &info) != 0) {
        if (errno == ENOENT) GTEST_SKIP() << "requires x86_64 KERNEL_FEATURES=mm_selftest";
        FAIL() << "stat compaction selftest: " << strerror(errno);
    }
    const int fd = open(kReport, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    const std::string report = ReadReport(fd, 7);
    EXPECT_EQ(0, lseek(fd, 0, SEEK_SET));
    const std::string reread = ReadReport(fd, 256);
    EXPECT_EQ(0, close(fd));
    CheckReport(report);
    EXPECT_EQ(report, reread) << "one open must retain its original snapshot";

    // A separate open runs the real fixture again after its previous owners
    // and isolation token have been released, without rebooting the guest.
    const int second = open(kReport, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(second, 0) << strerror(errno);
    const std::string repeated = ReadReport(second, 256);
    EXPECT_EQ(0, close(second));
    CheckReport(repeated);
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
