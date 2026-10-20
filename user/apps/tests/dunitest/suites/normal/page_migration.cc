#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <linux/futex.h>
#include <sched.h>
#include <stdint.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <atomic>
#include <string>
#include <thread>

namespace {
constexpr const char* kCommand = "/sys/kernel/debug/mm/page_migration";
constexpr const char* kReport = "/sys/kernel/debug/mm/page_migration_selftest";
constexpr size_t kPage = 4096;

std::string ReadReport(int fd) {
    std::string report;
    char buffer[127];
    off_t offset = 0;
    for (;;) {
        const ssize_t size = pread(fd, buffer, sizeof(buffer), offset);
        if (size < 0 && errno == EINTR) continue;
        if (size <= 0) break;
        report.append(buffer, static_cast<size_t>(size));
        offset += size;
    }
    return report;
}

std::string Migrate(void* page, uint32_t mode = 0) {
    struct Command { uint64_t address; uint32_t mode; uint32_t reserved; };
    const Command command{reinterpret_cast<uint64_t>(page), mode, 0};
    static_assert(sizeof(command) == 16, "migration command ABI");
    for (int attempt = 0; attempt != 128; ++attempt) {
        const int fd = open(kCommand, O_RDWR | O_CLOEXEC);
        if (fd < 0) return "open_error=" + std::to_string(errno);
        const ssize_t size = write(fd, &command, sizeof(command));
        const std::string report = size == sizeof(command) ? ReadReport(fd) :
            "write_error=" + std::to_string(errno);
        close(fd);
        if (report.find("error=busy\n") == std::string::npos) return report;
        sched_yield();
    }
    return "error=busy\n";
}

class PageMigration : public testing::Test {
 protected:
    void SetUp() override {
        struct stat info;
        if (stat(kReport, &info) < 0) {
            if (errno == ENOENT) GTEST_SKIP() << "requires x86_64 KERNEL_FEATURES=mm_selftest";
            FAIL() << "stat migration selftest errno=" << errno;
        }
        page_ = mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, page_);
        *static_cast<volatile uint64_t*>(page_) = 0x123456789abcdefULL;
    }
    void TearDown() override {
        if (page_ != MAP_FAILED) EXPECT_EQ(0, munmap(page_, kPage));
    }
    void* page_ = MAP_FAILED;
};

TEST_F(PageMigration, OwnershipAccountingAndRollbackSelftest) {
    const int fd = open(kReport, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    const std::string report = ReadReport(fd);
    close(fd);
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_NE(std::string::npos, report.find("summary_fail=0\n")) << report;
    for (const char* key : {"identity_data_permissions", "unique_charge_full_limit", "offline_owner",
            "pin_and_extra_arc", "rollback_stages", "unsupported_lru", "repeated_migration", "atomic_hva_pin",
            "ownership_freeze_before_copy", "cached_file_clean", "cached_file_dirty",
            "cached_shmem_clean", "cached_shmem_dirty", "cached_offline_charge",
            "cached_entry_pin_busy", "cached_extra_arc_busy", "cached_access_pin_busy",
            "cached_read_dma_reservation_busy", "cached_writeback_claim_busy",
            "cached_reclaimer_snapshot_recency"}) {
        EXPECT_NE(std::string::npos, report.find(std::string(key) + "=ok\n")) << report;
    }
}

TEST_F(PageMigration, RealMappingAndFailureRollback) {
    for (uint32_t stage = 1; stage <= 5; ++stage) {
        const std::string report = Migrate(page_, stage);
        EXPECT_NE(std::string::npos, report.find("error=injected\n")) << report;
        EXPECT_EQ(0x123456789abcdefULL, *static_cast<volatile uint64_t*>(page_));
    }
    const std::string report = Migrate(page_);
    EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
    EXPECT_EQ(0x123456789abcdefULL, *static_cast<volatile uint64_t*>(page_));
}

TEST_F(PageMigration, ReadonlyAndCowKeepTheirSemantics) {
    ASSERT_EQ(0, mprotect(page_, kPage, PROT_READ));
    const std::string readonly = Migrate(page_);
    EXPECT_NE(std::string::npos, readonly.find("status=ok\n")) << readonly;
    ASSERT_EQ(0, mprotect(page_, kPage, PROT_READ | PROT_WRITE));
    int ready[2], done[2];
    ASSERT_EQ(0, pipe(ready));
    ASSERT_EQ(0, pipe(done));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        close(ready[0]); close(done[1]);
        const char value = 'x';
        write(ready[1], &value, 1);
        char finish;
        read(done[0], &finish, 1);
        const bool intact = *static_cast<volatile uint64_t*>(page_) == 0x123456789abcdefULL;
        _exit(intact ? 0 : 1);
    }
    close(ready[1]); close(done[0]);
    char value;
    ASSERT_EQ(1, read(ready[0], &value, 1));
    const std::string shared = Migrate(page_);
    EXPECT_NE(std::string::npos, shared.find("error=unsupported\n")) << shared;
    *static_cast<volatile uint64_t*>(page_) = 99;  // ordinary COW creates one-source private page
    const std::string private_copy = Migrate(page_);
    EXPECT_NE(std::string::npos, private_copy.find("status=ok\n")) << private_copy;
    EXPECT_EQ(1, write(done[1], "x", 1));
    close(ready[0]); close(done[1]);
    int status;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status) && WEXITSTATUS(status) == 0);
    EXPECT_EQ(99u, *static_cast<volatile uint64_t*>(page_));
}

TEST_F(PageMigration, OtherCpuFaultWaitsAndWritesArePreserved) {
    cpu_set_t available;
    ASSERT_EQ(0, sched_getaffinity(0, sizeof(available), &available));
    int first = -1, second = -1;
    for (int cpu = 0; cpu < CPU_SETSIZE; ++cpu) if (CPU_ISSET(cpu, &available)) {
        if (first < 0) first = cpu; else { second = cpu; break; }
    }
    ASSERT_GE(second, 0) << "mandatory migration tests require SMP";
    cpu_set_t caller;
    CPU_ZERO(&caller); CPU_SET(first, &caller);
    ASSERT_EQ(0, sched_setaffinity(0, sizeof(caller), &caller));
    std::atomic<bool> started{false}, stop{false};
    std::atomic<int> affinity_error{0};
    uint64_t count = 0;
    auto* value = static_cast<uint64_t*>(page_);
    __atomic_store_n(value, 0, __ATOMIC_RELAXED);
    std::thread writer([&] {
        cpu_set_t worker;
        CPU_ZERO(&worker); CPU_SET(second, &worker);
        affinity_error = sched_setaffinity(0, sizeof(worker), &worker);
        started.store(true, std::memory_order_release);
        while (!stop.load(std::memory_order_acquire)) {
            __atomic_fetch_add(value, uint64_t{1}, __ATOMIC_RELAXED);
            ++count;
        }
    });
    while (!started.load(std::memory_order_acquire)) sched_yield();
    std::string fault_report;
    if (affinity_error == 0) {
        fault_report = Migrate(page_, 6);
        for (int round = 0; round != 16; ++round) {
            const std::string report = Migrate(page_);
            EXPECT_NE(std::string::npos, report.find("status=ok\n")) << report;
        }
    }
    stop.store(true, std::memory_order_release);
    writer.join();
    EXPECT_EQ(0, sched_setaffinity(0, sizeof(available), &available));
    EXPECT_EQ(0, affinity_error.load());
    EXPECT_NE(std::string::npos, fault_report.find("status=ok\n")) << fault_report;
    EXPECT_NE(std::string::npos, fault_report.find("fault_wait=ok\n")) << fault_report;
    EXPECT_GT(count, 0u);
    EXPECT_EQ(count, __atomic_load_n(value, __ATOMIC_RELAXED));
}

TEST_F(PageMigration, RemoteCopyFutexAndRemapRemainUsable) {
    uint64_t written = 77, read_back = 0;
    iovec local{&written, sizeof(written)}, remote{page_, sizeof(written)};
    ASSERT_EQ(sizeof(written), process_vm_writev(getpid(), &local, 1, &remote, 1, 0));
    EXPECT_NE(std::string::npos, Migrate(page_).find("status=ok\n"));
    local = iovec{&read_back, sizeof(read_back)};
    ASSERT_EQ(sizeof(read_back), process_vm_readv(getpid(), &local, 1, &remote, 1, 0));
    EXPECT_EQ(written, read_back);
    errno = 0;
    EXPECT_EQ(-1, syscall(SYS_futex, page_, FUTEX_WAIT | FUTEX_PRIVATE_FLAG, 999, nullptr, nullptr, 0));
    EXPECT_EQ(EAGAIN, errno);
    void* moved = mremap(page_, kPage, 2 * kPage, MREMAP_MAYMOVE);
    ASSERT_NE(MAP_FAILED, moved);
    page_ = moved;
    EXPECT_EQ(written, *static_cast<uint64_t*>(page_));
    EXPECT_NE(std::string::npos, Migrate(page_).find("status=ok\n"));
    EXPECT_EQ(0, munmap(static_cast<char*>(page_) + kPage, kPage));
}


TEST_F(PageMigration, PermissionChangesAndUnmapSerializeWithMigration) {
    std::atomic<bool> start{false};
    std::string result;
    std::thread migrator([&] {
        start.store(true, std::memory_order_release);
        for (int round = 0; round != 16; ++round) {
            result = Migrate(page_);
            if (result.find("status=ok\n") == std::string::npos) return;
        }
    });
    while (!start.load(std::memory_order_acquire)) sched_yield();
    bool permissions_ok = true;
    for (int round = 0; round != 16; ++round) {
        permissions_ok &= mprotect(page_, kPage, PROT_READ) == 0;
        permissions_ok &= mprotect(page_, kPage, PROT_READ | PROT_WRITE) == 0;
    }
    migrator.join();
    EXPECT_TRUE(permissions_ok);
    EXPECT_NE(std::string::npos, result.find("status=ok\n")) << result;
    EXPECT_EQ(0x123456789abcdefULL, *static_cast<uint64_t*>(page_));

    // The command owns no user mapping: concurrent final unmap either wins
    // before admission, or follows a completed migration under the MM lock.
    start = false;
    void* target = page_;
    std::thread unmapper([&] {
        while (!start.load(std::memory_order_acquire)) sched_yield();
        permissions_ok = munmap(target, kPage) == 0;
    });
    start.store(true, std::memory_order_release);
    result = Migrate(target);
    unmapper.join();
    page_ = MAP_FAILED;
    EXPECT_TRUE(permissions_ok);
    EXPECT_TRUE(result.find("status=ok\n") != std::string::npos ||
                result.find("error=invalid_mapping\n") != std::string::npos) << result;
}

TEST_F(PageMigration, SharedMappingIsExplicitlyUnsupported) {
    void* shared = mmap(nullptr, kPage, PROT_READ | PROT_WRITE, MAP_SHARED | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, shared);
    *static_cast<volatile uint64_t*>(shared) = 31;
    const std::string result = Migrate(shared);
    EXPECT_NE(std::string::npos, result.find("error=unsupported\n")) << result;
    EXPECT_EQ(31u, *static_cast<volatile uint64_t*>(shared));
    EXPECT_EQ(0, munmap(shared, kPage));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
