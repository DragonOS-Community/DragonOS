#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>

TEST(PageRetirement, OwnershipAndLifecycle) {
    constexpr const char* path = "/sys/kernel/debug/mm/page_retirement_selftest";
    struct stat info;
    if (stat(path, &info) < 0) {
        if (errno == ENOENT) GTEST_SKIP() << "requires KERNEL_FEATURES=mm_selftest";
        FAIL() << "stat retirement report errno=" << errno;
    }
    const int fd = open(path, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0) << "open errno=" << errno;
    std::string report;
    char buffer[127];
    for (;;) {
        const ssize_t count = read(fd, buffer, sizeof(buffer));
        if (count < 0 && errno == EINTR) continue;
        if (count < 0) {
            const int error = errno;
            close(fd);
            FAIL() << "read errno=" << error;
        }
        if (count == 0) break;
        report.append(buffer, static_cast<size_t>(count));
    }
    ASSERT_EQ(0, close(fd));
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("summary_fail=0\n")) << report;
    for (const char* key : {"last_unmap", "partial_unmap", "madvise_range", "teardown", "inactive_teardown", "multibatch_munmap", "multibatch_teardown", "multibatch_inactive_teardown", "heap_denied_teardown", "heap_denied_inactive_teardown",
            "heap_denied_manager_contention", "heap_denied_reclaimer_contention",
            "heap_denied_mm_write_contention", "heap_denied_pt_edit_contention",
            "heap_denied_page_write_contention", "heap_denied_vma_contention",
            "heap_denied_rwsem_mixed_broadcast",
            "heap_denied_lazy_mm_queue", "heap_denied_futex_wake",
            "heap_denied_page_entry_wait", "heap_denied_device_output_buffer",
            "heap_denied_workqueue", "heap_denied_irq_dispatch", "heap_denied_clocksource_watchdog", "heap_denied_fuse_wait", "heap_denied_rcu_slot_drop",
            "two_mm_interleaved_lru", "two_mm_interleaved_plain", "last_mapping_only", "access_pin_lifetime", "extra_arc_lifetime",
            "reclaimer_snapshot_lifetime", "file_backing_preserved", "shmem_backing_preserved",
            "shm_backing_pin", "normal_backing_pin", "unevictable_preserved", "oom_lru",
            "oom_reclaimer_busy", "oom_plain_ignores_lru_contention", "oom_partial_batch_busy"}) {
        EXPECT_NE(std::string::npos, report.find(std::string(key) + "=ok\n")) << report;
    }
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
