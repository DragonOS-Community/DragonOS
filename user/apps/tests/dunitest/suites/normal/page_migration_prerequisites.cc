#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <sys/stat.h>
#include <unistd.h>

#include <initializer_list>
#include <string>

namespace {

// Pin the caller so the controlled remote holder uses a distinct CPU.
class PageMigrationPrerequisites : public testing::Test {
 protected:
    void SetUp() override {
        struct stat info;
        if (stat("/sys/kernel/debug/mm/page_access_selftest", &info) < 0) {
            if (errno == ENOENT) {
                GTEST_SKIP() << "requires KERNEL_FEATURES=mm_selftest";
            }
            FAIL() << "stat page_access_selftest errno=" << errno;
        }
        ASSERT_EQ(0, sched_getaffinity(0, sizeof(saved_), &saved_));
        int first = -1, count = 0;
        for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) {
            if (CPU_ISSET(cpu, &saved_)) {
                if (first < 0) first = cpu;
                ++count;
            }
        }
        ASSERT_GE(count, 2) << "SMP prerequisite tests require two available CPUs";
        cpu_set_t single;
        CPU_ZERO(&single);
        CPU_SET(first, &single);
        ASSERT_EQ(0, sched_setaffinity(0, sizeof(single), &single));
        changed_ = true;
    }
    void TearDown() override {
        if (changed_) EXPECT_EQ(0, sched_setaffinity(0, sizeof(saved_), &saved_));
    }
    void Check(const char* name, std::initializer_list<const char*> checks) {
        const std::string path = std::string("/sys/kernel/debug/mm/") + name;
        const int fd = open(path.c_str(), O_RDONLY | O_CLOEXEC);
        ASSERT_GE(fd, 0) << path << " errno=" << errno;
        std::string report;
        char buffer[127];
        ssize_t count;
        while ((count = read(fd, buffer, sizeof(buffer))) != 0) {
            if (count < 0 && errno == EINTR) continue;
            if (count < 0) {
                close(fd);
                FAIL() << "read " << path << " errno=" << errno;
            }
            report.append(buffer, static_cast<size_t>(count));
        }
        ASSERT_EQ(0, close(fd));
        EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
        EXPECT_NE(std::string::npos, report.find("summary_fail=0\n")) << report;
        for (const char* check : checks) {
            EXPECT_NE(std::string::npos, report.find(std::string(check) + "=ok\n")) << report;
        }
    }
 private:
    cpu_set_t saved_{};
    bool changed_ = false;
};

TEST_F(PageMigrationPrerequisites, PhysicalAccessAndIsolationContract) {
    Check("page_access_selftest", {"candidate_identity_and_mm_scope", "pin_isolation_exclusion",
          "data_lock_and_rollback", "unsupported_revalidation", "remote_copy_and_pin_lifetime",
          "cross_cpu_pin_exclusion", "pin_counter_overflow"});
}

#if defined(__x86_64__)
TEST_F(PageMigrationPrerequisites, TeardownCompletesBeforePhysicalRelease) {
    Check("tlb_teardown_selftest", {"smp_available", "kernel_teardown_order", "kernel_missing_flush_detected",
          "kernel_missing_ack_detected", "user_teardown_order", "user_missing_flush_detected",
          "user_missing_ack_detected", "user_lru_teardown_order", "user_lru_missing_flush_detected",
          "user_lru_missing_ack_detected"});
}
#endif

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
