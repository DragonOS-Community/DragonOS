#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <string.h>
#include <unistd.h>

#include <atomic>
#include <string>
#include <thread>
#include <vector>

namespace {

constexpr const char* kSelftestPath = "/sys/kernel/debug/page_cache/accounting_selftest";
constexpr const char* kWrapperCacheSelftestPath =
    "/sys/kernel/debug/vfs/mount_wrapper_cache_selftest";

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
    EXPECT_EQ(0, close(fd)) << strerror(errno);
    return content;
}

class ScopedTmpfsMount {
public:
    explicit ScopedTmpfsMount(const char* tag)
        : path_(std::string("/tmp/") + tag + "_" + std::to_string(getpid())) {}

    bool Mount() {
        if (mkdir(path_.c_str(), 0700) != 0 && errno != EEXIST) {
            return false;
        }
        mounted_ = mount("none", path_.c_str(), "tmpfs", 0, nullptr) == 0;
        return mounted_;
    }

    ~ScopedTmpfsMount() {
        if (mounted_) {
            umount(path_.c_str());
        }
        rmdir(path_.c_str());
    }

    const std::string& path() const { return path_; }

private:
    std::string path_;
    bool mounted_ = false;
};

}  // namespace

TEST(PageCacheAccounting, MembershipLifecycleIsBalanced) {
    const std::string report = ReadAll(kSelftestPath);
    ASSERT_FALSE(report.empty());
    EXPECT_NE(std::string::npos, report.find("ramfs_fallocate_range=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("write_prepare_rollback=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("preallocate_rollback=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("file_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("shmem_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("dirty_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("dirty_incarnation=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("remote_dirty_publish=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("writeback_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("writeback_admission_order=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("writeback_submission_token=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("writeback_defer_progress=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("writeback_budget_retry=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("fault_invalidate_retry_order=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("unevictable_membership=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("inflight_teardown=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("late_completion=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("global_wiring=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("layout=ok\n")) << report;
}

TEST(PageCacheAccounting, MountWrapperReplacementKeepsNewCacheEntry) {
    const std::string report = ReadAll(kWrapperCacheSelftestPath);
    ASSERT_FALSE(report.empty());
    EXPECT_NE(std::string::npos, report.find("replacement_survives_old_drop=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("replacement_final_drop=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
}

TEST(PageCacheAccounting, ConcurrentOpenCloseUsesOneTmpfsDentrySafely) {
    ScopedTmpfsMount tmpfs("mount_wrapper_concurrency");
    ASSERT_TRUE(tmpfs.Mount()) << strerror(errno);
    const std::string path = tmpfs.path() + "/shared";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    constexpr size_t kWorkers = 8;
    constexpr size_t kIterationsPerWorker = 4096;
    std::atomic<size_t> ready{0};
    std::atomic<bool> start{false};
    std::atomic<size_t> failures{0};
    std::vector<std::thread> workers;
    workers.reserve(kWorkers);
    for (size_t worker = 0; worker < kWorkers; ++worker) {
        workers.emplace_back([&] {
            ready.fetch_add(1, std::memory_order_release);
            while (!start.load(std::memory_order_acquire)) {
                std::this_thread::yield();
            }
            for (size_t iteration = 0; iteration < kIterationsPerWorker; ++iteration) {
                const int current = open(path.c_str(), O_RDONLY | O_CLOEXEC);
                if (current < 0 || close(current) != 0) {
                    failures.fetch_add(1, std::memory_order_relaxed);
                    return;
                }
            }
        });
    }
    while (ready.load(std::memory_order_acquire) != kWorkers) {
        std::this_thread::yield();
    }
    start.store(true, std::memory_order_release);
    for (auto& worker : workers) {
        worker.join();
    }

    EXPECT_EQ(0U, failures.load());
    EXPECT_EQ(0, unlink(path.c_str())) << strerror(errno);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
