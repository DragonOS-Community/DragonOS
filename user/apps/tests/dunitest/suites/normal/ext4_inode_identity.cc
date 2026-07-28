#include <gtest/gtest.h>

#include <errno.h>
#include <dirent.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <sys/wait.h>
#include <unistd.h>

#include <algorithm>
#include <atomic>
#include <chrono>
#include <string>
#include <thread>

#ifndef __NR_syncfs
#if defined(__x86_64__)
#define __NR_syncfs 306
#elif defined(__riscv) || defined(__loongarch64__)
#define __NR_syncfs 267
#else
#error "__NR_syncfs is not defined for this architecture"
#endif
#endif

#ifndef __NR_renameat2
#if defined(__x86_64__)
#define __NR_renameat2 316
#elif defined(__riscv) || defined(__loongarch64__)
#define __NR_renameat2 276
#else
#error "__NR_renameat2 is not defined for this architecture"
#endif
#endif

#ifndef RENAME_EXCHANGE
#define RENAME_EXCHANGE (1U << 1)
#endif

#ifndef RENAME_WHITEOUT
#define RENAME_WHITEOUT (1U << 2)
#endif

namespace {

constexpr unsigned long kLoopCtlGetFree = 0x4C82;
constexpr unsigned long kLoopSetFd = 0x4C00;
constexpr unsigned long kLoopClrFd = 0x4C01;

std::string FixturePath() {
    char executable[512] = {};
    ssize_t size = readlink("/proc/self/exe", executable, sizeof(executable) - 1);
    if (size <= 0) {
        return {};
    }
    std::string path(executable, static_cast<size_t>(size));
    for (int i = 0; i < 3; ++i) {
        size_t slash = path.rfind('/');
        if (slash == std::string::npos) {
            return {};
        }
        path.resize(slash);
    }
    return path + "/fixtures/ext4_inode_identity.img";
}

void CopySparseFile(const std::string& source, int destination) {
    int source_fd = open(source.c_str(), O_RDONLY);
    ASSERT_GE(source_fd, 0) << source << ": " << strerror(errno);
    char buffer[64 * 1024];
    off_t size = 0;
    for (;;) {
        ssize_t count = read(source_fd, buffer, sizeof(buffer));
        ASSERT_GE(count, 0) << strerror(errno);
        if (count == 0) {
            break;
        }
        bool all_zero = true;
        for (ssize_t i = 0; i < count; ++i) {
            if (buffer[i] != 0) {
                all_zero = false;
                break;
            }
        }
        if (all_zero) {
            ASSERT_NE(static_cast<off_t>(-1), lseek(destination, count, SEEK_CUR))
                << strerror(errno);
        } else {
            ssize_t done = 0;
            while (done < count) {
                ssize_t written = write(destination, buffer + done, count - done);
                ASSERT_GT(written, 0) << strerror(errno);
                done += written;
            }
        }
        size += count;
    }
    ASSERT_EQ(0, ftruncate(destination, size)) << strerror(errno);
    ASSERT_EQ(0, close(source_fd)) << strerror(errno);
    ASSERT_EQ(0, lseek(destination, 0, SEEK_SET)) << strerror(errno);
}

class LoopExt4 {
  public:
    ~LoopExt4() {
        if (second_mounted_) {
            umount(second_mount_point_.c_str());
        }
        if (mounted_) {
            umount(mount_point_.c_str());
        }
        if (loop_fd_ >= 0) {
            ioctl(loop_fd_, kLoopClrFd, 0);
            close(loop_fd_);
        }
        if (backing_fd_ >= 0) {
            close(backing_fd_);
        }
        if (!mount_point_.empty()) {
            rmdir(mount_point_.c_str());
        }
        if (!second_mount_point_.empty()) {
            rmdir(second_mount_point_.c_str());
        }
        if (!image_.empty()) {
            unlink(image_.c_str());
        }
    }

    void SetUp() {
        image_ = "/tmp/ext4_inode_identity_" + std::to_string(getpid()) + ".img";
        mount_point_ = "/tmp/ext4_inode_identity_" + std::to_string(getpid()) + "_mnt";

        ASSERT_EQ(0, mkdir(mount_point_.c_str(), 0700)) << strerror(errno);

        backing_fd_ = open(image_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
        ASSERT_GE(backing_fd_, 0) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(CopySparseFile(FixturePath(), backing_fd_));
        close(backing_fd_);
        backing_fd_ = -1;

        int control = open("/dev/loop-control", O_RDWR);
        ASSERT_GE(control, 0) << strerror(errno);
        int minor = ioctl(control, kLoopCtlGetFree, 0);
        int saved_errno = errno;
        close(control);
        ASSERT_GE(minor, 0) << strerror(saved_errno);

        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_fd_ = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(loop_fd_, 0) << strerror(errno);
        backing_fd_ = open(image_.c_str(), O_RDWR);
        ASSERT_GE(backing_fd_, 0) << strerror(errno);
        ASSERT_EQ(0, ioctl(loop_fd_, kLoopSetFd, backing_fd_)) << strerror(errno);
    }

    void Mount() {
        ASSERT_EQ(0, mount(loop_path_.c_str(), mount_point_.c_str(), "ext4", 0, nullptr))
            << strerror(errno);
        mounted_ = true;
    }

    void Unmount() {
        ASSERT_TRUE(mounted_);
        ASSERT_EQ(0, umount(mount_point_.c_str())) << strerror(errno);
        mounted_ = false;
    }

    void MountSecond() {
        ASSERT_TRUE(mounted_);
        ASSERT_FALSE(second_mounted_);
        second_mount_point_ = "/tmp/ext4_inode_identity_" + std::to_string(getpid())
            + "_second_mnt";
        ASSERT_EQ(0, mkdir(second_mount_point_.c_str(), 0700)) << strerror(errno);
        ASSERT_EQ(0, mount(loop_path_.c_str(), second_mount_point_.c_str(), "ext4", 0, nullptr))
            << strerror(errno);
        second_mounted_ = true;
    }

    void UnmountSecond() {
        ASSERT_TRUE(second_mounted_);
        ASSERT_EQ(0, umount(second_mount_point_.c_str())) << strerror(errno);
        second_mounted_ = false;
        ASSERT_EQ(0, rmdir(second_mount_point_.c_str())) << strerror(errno);
        second_mount_point_.clear();
    }

    void Detach() {
        ASSERT_TRUE(mounted_);
        ASSERT_EQ(0, umount2(mount_point_.c_str(), MNT_DETACH)) << strerror(errno);
        mounted_ = false;
        detached_ = true;
    }

    void FinishDetached() {
        ASSERT_TRUE(detached_);
        // MNT_DETACH drops the namespace edge immediately, while final
        // superblock teardown runs after the last external owner is released.
        // Do not detach the loop backing underneath that teardown.
        usleep(50 * 1000);
        bool cleared = false;
        for (int attempt = 0; attempt < 100; ++attempt) {
            if (ioctl(loop_fd_, kLoopClrFd, 0) == 0) {
                cleared = true;
                break;
            }
            ASSERT_EQ(EBUSY, errno) << strerror(errno);
            usleep(5 * 1000);
        }
        ASSERT_TRUE(cleared) << "detached ext4 mount retained the loop device";
        detached_ = false;
        close(loop_fd_);
        loop_fd_ = -1;
        close(backing_fd_);
        backing_fd_ = -1;
        ASSERT_EQ(0, rmdir(mount_point_.c_str())) << strerror(errno);
        mount_point_.clear();
        ASSERT_EQ(0, unlink(image_.c_str())) << strerror(errno);
        image_.clear();
    }

    const std::string& mount_point() const {
        return mount_point_;
    }

    const std::string& second_mount_point() const {
        return second_mount_point_;
    }

  private:
    std::string image_;
    std::string mount_point_;
    std::string second_mount_point_;
    std::string loop_path_;
    int backing_fd_ = -1;
    int loop_fd_ = -1;
    bool mounted_ = false;
    bool second_mounted_ = false;
    bool detached_ = false;
};

void WriteAll(int fd, const char* data, size_t len) {
    size_t done = 0;
    while (done < len) {
        ssize_t written = write(fd, data + done, len - done);
        ASSERT_GT(written, 0) << strerror(errno);
        done += static_cast<size_t>(written);
    }
}

// Run the concurrent part of the mmap/writeback pressure case. It deliberately
// uses no gtest assertion because the parent executes it in a child process
// with a deadline; return zero on success, otherwise an errno-style failure.
int RunMmapBufferedWriteAndFsyncStress(char* mapping, size_t page_size, int buffered_fd,
                                       int sync_fd) {
    constexpr int kIterations = 32;
    constexpr size_t kMappingHalf = 2048;
    constexpr size_t kBufferedHalf = 2048;

    std::atomic<unsigned int> ready{0};
    std::atomic<bool> start{false};
    std::atomic<bool> buffered_dirty{false};
    std::atomic<bool> syncer_entered{false};
    std::atomic<bool> stop{false};
    std::atomic<int> first_error{0};
    auto record_error = [&](int error) {
        int expected = 0;
        first_error.compare_exchange_strong(expected, error == 0 ? EIO : error);
        stop.store(true, std::memory_order_release);
    };
    auto wait_for_start = [&] {
        ready.fetch_add(1, std::memory_order_release);
        while (!start.load(std::memory_order_acquire)) {
            if (stop.load(std::memory_order_acquire)) {
                return false;
            }
            std::this_thread::yield();
        }
        return true;
    };
    auto wait_for = [&](const std::atomic<bool>& entered) {
        while (!entered.load(std::memory_order_acquire)) {
            if (stop.load(std::memory_order_acquire)) {
                return false;
            }
            std::this_thread::yield();
        }
        return true;
    };

    std::thread mapping_writer([&] {
        if (!wait_for_start() || !wait_for(syncer_entered)) {
            return;
        }
        for (int iteration = 0; iteration < kIterations; ++iteration) {
            if (madvise(mapping, page_size, MADV_DONTNEED) != 0) {
                record_error(errno);
                return;
            }
            // Mapping and buffered writes intentionally use disjoint halves
            // of one page. This keeps the invalidate domain identical without
            // introducing unrelated, non-atomic byte races.
            mapping[(iteration * 37) % kMappingHalf] = static_cast<char>(iteration + 1);
            std::this_thread::yield();
        }
    });
    std::thread buffered_writer([&] {
        if (!wait_for_start()) {
            return;
        }

        // Make the first fsync observe a dirty PageCache page before it is
        // allowed to start. Later writes are the buffered invalidate-write
        // contenders while mmap stores repeatedly take fresh write faults.
        constexpr char kBootstrapByte = 'B';
        if (pwrite(buffered_fd, &kBootstrapByte, 1, kBufferedHalf) != 1) {
            record_error(errno);
            return;
        }
        buffered_dirty.store(true, std::memory_order_release);
        if (!wait_for(syncer_entered)) {
            return;
        }
        for (int iteration = 0; iteration < kIterations; ++iteration) {
            const char byte = static_cast<char>('a' + (iteration % 26));
            const off_t offset = static_cast<off_t>(
                kBufferedHalf + ((iteration * 73 + 1) % kBufferedHalf));
            if (pwrite(buffered_fd, &byte, 1, offset) != 1) {
                record_error(errno);
                return;
            }
            std::this_thread::yield();
        }
    });
    std::thread syncer([&] {
        if (!wait_for_start() || !wait_for(buffered_dirty)) {
            return;
        }
        syncer_entered.store(true, std::memory_order_release);
        for (int iteration = 0; iteration < kIterations; ++iteration) {
            if (fsync(sync_fd) != 0) {
                record_error(errno);
                return;
            }
            std::this_thread::yield();
        }
    });

    while (ready.load(std::memory_order_acquire) != 3) {
        std::this_thread::yield();
    }
    start.store(true, std::memory_order_release);
    mapping_writer.join();
    buffered_writer.join();
    syncer.join();

    return first_error.load(std::memory_order_acquire);
}

// Exercise the mmap/writeback conflict shape without a kernel-only test hook.
// The mapper repeatedly discards its PTE so the next store takes a file-backed
// write-fault path; buffered pwrite is the invalidate-write contender; fsync
// drives PageCache snapshot/writeback. MAP_SHARED covers page_mkwrite, while
// MAP_PRIVATE covers COW. The exact A/B/C lock order is proved by PageCache's
// deterministic debug selftest; this is a bounded integration stress test of
// the real ext4 paths.
void RunMmapBufferedWriteAndFsyncProgress(int mapping_mode) {
    constexpr size_t kPageSize = 4096;
    constexpr char kInitialByte = 'I';
    constexpr char kPrivateByte = 'P';
    const bool private_mapping = mapping_mode == MAP_PRIVATE;

    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point()
        + (private_mapping ? "/private_mmap_cow_fsync_race" : "/shared_mmap_fsync_race");
    int writer_fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(writer_fd, 0) << strerror(errno);
    int sync_fd = dup(writer_fd);
    ASSERT_GE(sync_fd, 0) << strerror(errno);
    int buffered_fd = dup(writer_fd);
    ASSERT_GE(buffered_fd, 0) << strerror(errno);
    ASSERT_EQ(0, ftruncate(writer_fd, static_cast<off_t>(kPageSize))) << strerror(errno);
    ASSERT_EQ(1, pwrite(writer_fd, &kInitialByte, 1, 0)) << strerror(errno);
    ASSERT_EQ(0, fsync(writer_fd)) << strerror(errno);

    char* mapping = static_cast<char*>(
        mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, mapping_mode, writer_fd, 0));
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);

    // Establish that this mapping genuinely takes COW semantics before the
    // pressure loop. The subsequent MADV_DONTNEED makes the loop's first
    // private store fault again instead of reusing this private page.
    if (private_mapping) {
        mapping[0] = kPrivateByte;
        char on_disk = 0;
        ASSERT_EQ(1, pread(writer_fd, &on_disk, 1, 0)) << strerror(errno);
        ASSERT_EQ(kInitialByte, on_disk);
        ASSERT_EQ(0, madvise(mapping, kPageSize, MADV_DONTNEED)) << strerror(errno);
    }

    const pid_t child = fork();
    if (child < 0) {
        const int saved_errno = errno;
        FAIL() << "fork mmap/writeback stress child: " << strerror(saved_errno);
    }
    if (child == 0) {
        _exit(RunMmapBufferedWriteAndFsyncStress(mapping, kPageSize, buffered_fd, sync_fd) == 0
                  ? 0
                  : 1);
    }

    // The child is forked before the stress threads exist. The parent retains
    // the LoopExt4 fixture and its descriptors, so after a timeout it can kill
    // only the stalled workload and still perform normal unmap/unmount/loop
    // cleanup. Never use a completion notification as proof that the child
    // reached _exit(): only a successful nonblocking reap may end this loop.
    constexpr int kStressTimeoutMs = 10 * 1000;
    int child_status = 0;
    pid_t waited = 0;
    int wait_error = 0;
    const auto deadline = std::chrono::steady_clock::now()
        + std::chrono::milliseconds(kStressTimeoutMs);
    for (;;) {
        waited = waitpid(child, &child_status, WNOHANG);
        if (waited == child) {
            break;
        }
        if (waited < 0) {
            if (errno == EINTR) {
                if (std::chrono::steady_clock::now() >= deadline) {
                    waited = 0;
                    break;
                }
                continue;
            }
            wait_error = errno;
            break;
        }
        if (std::chrono::steady_clock::now() >= deadline) {
            break;
        }
        usleep(1000);
    }

    if (waited == 0) {
        if (kill(child, SIGKILL) != 0 && errno != ESRCH) {
            ADD_FAILURE() << "kill stalled mmap/writeback stress child: " << strerror(errno);
        }
        do {
            waited = waitpid(child, &child_status, 0);
        } while (waited < 0 && errno == EINTR);
        const int reap_error = waited < 0 ? errno : 0;
        EXPECT_EQ(child, waited) << "reap stalled mmap/writeback stress child: "
                                 << strerror(reap_error);
        ADD_FAILURE() << "mmap/writeback stress exceeded " << kStressTimeoutMs << " ms";
    } else if (waited < 0) {
        ADD_FAILURE() << "waitpid mmap/writeback stress child: " << strerror(wait_error);
    } else {
        EXPECT_TRUE(WIFEXITED(child_status));
        if (WIFEXITED(child_status)) {
            EXPECT_EQ(0, WEXITSTATUS(child_status));
        }
    }

    if (!private_mapping) {
        EXPECT_EQ(0, msync(mapping, kPageSize, MS_SYNC)) << strerror(errno);
    }

    // Concurrent writers have no defined last-byte ordering. Re-establish a
    // deterministic buffered-write/fsync/read boundary after the stress phase
    // so this is also a data-integrity regression test.
    constexpr char kExpected = '!';
    EXPECT_EQ(1, pwrite(writer_fd, &kExpected, 1, kPageSize - 1)) << strerror(errno);
    EXPECT_EQ(0, fsync(writer_fd)) << strerror(errno);
    char observed = 0;
    EXPECT_EQ(1, pread(writer_fd, &observed, 1, kPageSize - 1)) << strerror(errno);
    EXPECT_EQ(kExpected, observed);

    EXPECT_EQ(0, munmap(mapping, kPageSize)) << strerror(errno);
    EXPECT_EQ(0, close(buffered_fd)) << strerror(errno);
    EXPECT_EQ(0, close(sync_fd)) << strerror(errno);
    EXPECT_EQ(0, close(writer_fd)) << strerror(errno);
    EXPECT_EQ(0, unlink(path.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, StatfsReportsLinuxAbi) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    struct statfs by_path = {};
    ASSERT_EQ(0, statfs(fs.mount_point().c_str(), &by_path)) << strerror(errno);

    int fd = open(fs.mount_point().c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(fd, 0) << strerror(errno);
    struct statfs by_fd = {};
    ASSERT_EQ(0, fstatfs(fd, &by_fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    constexpr long kExt4SuperMagic = 0xEF53;
    EXPECT_EQ(kExt4SuperMagic, by_path.f_type);
    EXPECT_EQ(kExt4SuperMagic, by_fd.f_type);
    EXPECT_EQ(255, by_path.f_namelen);
    EXPECT_EQ(255, by_fd.f_namelen);
    EXPECT_GT(by_path.f_bsize, 0);
    EXPECT_EQ(by_path.f_bsize, by_fd.f_bsize);
    EXPECT_EQ(by_path.f_frsize, by_fd.f_frsize);
    EXPECT_LE(by_path.f_bfree, by_path.f_blocks);
    EXPECT_LE(by_path.f_bavail, by_path.f_bfree);
    EXPECT_LE(by_fd.f_bfree, by_fd.f_blocks);
    EXPECT_LE(by_fd.f_bavail, by_fd.f_bfree);

    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

std::string ReadFile(const char* path) {
    int fd = open(path, O_RDONLY);
    EXPECT_GE(fd, 0) << strerror(errno);
    std::string result;
    char buffer[4096];
    for (;;) {
        ssize_t count = read(fd, buffer, sizeof(buffer));
        EXPECT_GE(count, 0) << strerror(errno);
        if (count <= 0) {
            break;
        }
        result.append(buffer, static_cast<size_t>(count));
    }
    EXPECT_EQ(0, close(fd)) << strerror(errno);
    return result;
}

TEST(Ext4InodeIdentity, KernelLifecycleSelftestPasses) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    int fd = open("/sys/kernel/debug/ext4/lifecycle_selftest", O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    char report[4096] = {};
    ssize_t size = read(fd, report, sizeof(report) - 1);
    int saved_errno = errno;
    close(fd);
    ASSERT_GT(size, 0) << strerror(saved_errno);
    std::string text(report, static_cast<size_t>(size));
    EXPECT_NE(std::string::npos, text.find("status=ok\n")) << text;
    EXPECT_EQ(std::string::npos, text.find("=fail\n")) << text;
}

TEST(Ext4InodeIdentity, DelallocQueuePublishesAndRecoversInOrder) {
    constexpr size_t kPageSize = 4096;
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/delalloc_queue";
    const std::string first(kPageSize, 'A');
    const std::string second(kPageSize, 'B');
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);

    // Bytes and visible EOF are published together before writeback.
    ASSERT_EQ(static_cast<ssize_t>(first.size()),
              pwrite(fd, first.data(), first.size(), 0))
        << strerror(errno);
    struct stat visible = {};
    ASSERT_EQ(0, fstat(fd, &visible)) << strerror(errno);
    EXPECT_EQ(static_cast<off_t>(kPageSize), visible.st_size);
    std::string observed(kPageSize, '\0');
    ASSERT_EQ(static_cast<ssize_t>(observed.size()),
              pread(fd, observed.data(), observed.size(), 0))
        << strerror(errno);
    EXPECT_EQ(first, observed);

    // Followers and a partial tail remain ordered by the per-inode FIFO.
    ASSERT_EQ(static_cast<ssize_t>(second.size()),
              pwrite(fd, second.data(), second.size(), kPageSize))
        << strerror(errno);
    constexpr char kTail = 'C';
    ASSERT_EQ(1, pwrite(fd, &kTail, 1, 2 * kPageSize)) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());

    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    fd = open(path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    struct stat recovered = {};
    ASSERT_EQ(0, fstat(fd, &recovered)) << strerror(errno);
    EXPECT_EQ(static_cast<off_t>(2 * kPageSize + 1), recovered.st_size);
    std::string recovered_data(2 * kPageSize + 1, '\0');
    ASSERT_EQ(static_cast<ssize_t>(recovered_data.size()),
              pread(fd, recovered_data.data(), recovered_data.size(), 0))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(recovered_data.data(), first.data(), kPageSize));
    EXPECT_EQ(0, memcmp(recovered_data.data() + kPageSize, second.data(), kPageSize));
    EXPECT_EQ(kTail, recovered_data.back());
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, DelallocNonalignedThreePageAndSparseForwardAreZeroSafe) {
    constexpr size_t kPageSize = 4096;
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string cross_path = fs.mount_point() + "/delalloc_cross_partial";
    const size_t cross_offset = 17;
    const std::string payload(2 * kPageSize + 31, 'P');
    int fd = open(cross_path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(static_cast<ssize_t>(payload.size()),
              pwrite(fd, payload.data(), payload.size(), cross_offset))
        << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    const std::string sparse_path = fs.mount_point() + "/delalloc_sparse";
    fd = open(sparse_path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    const std::string prefix(17, 'A');
    ASSERT_EQ(static_cast<ssize_t>(prefix.size()),
              pwrite(fd, prefix.data(), prefix.size(), 0))
        << strerror(errno);
    // Keep the short head undrained while admitting the sparse successor.
    // The first PageCache snapshot is page-sized, but the head's own durable
    // EOF is only 17 bytes; one fsync must clip that submission rather than
    // fail-stopping the mount.
    const size_t sparse_offset = 3 * kPageSize + 7;
    const std::string sparse_tail(29, 'S');
    ASSERT_EQ(static_cast<ssize_t>(sparse_tail.size()),
              pwrite(fd, sparse_tail.data(), sparse_tail.size(), sparse_offset))
        << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());

    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    fd = open(cross_path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    std::string cross(cross_offset + payload.size(), '\xff');
    ASSERT_EQ(static_cast<ssize_t>(cross.size()),
              pread(fd, cross.data(), cross.size(), 0))
        << strerror(errno);
    EXPECT_TRUE(std::all_of(cross.begin(), cross.begin() + cross_offset,
                            [](char byte) { return byte == 0; }));
    EXPECT_EQ(0, memcmp(cross.data() + cross_offset, payload.data(), payload.size()));
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    fd = open(sparse_path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    std::string sparse(sparse_offset + sparse_tail.size(), '\xff');
    ASSERT_EQ(static_cast<ssize_t>(sparse.size()),
              pread(fd, sparse.data(), sparse.size(), 0))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(sparse.data(), prefix.data(), prefix.size()));
    EXPECT_TRUE(std::all_of(sparse.begin() + prefix.size(),
                            sparse.begin() + sparse_offset,
                            [](char byte) { return byte == 0; }));
    EXPECT_EQ(0, memcmp(sparse.data() + sparse_offset, sparse_tail.data(),
                        sparse_tail.size()));
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, FsyncFrozenFrontierDoesNotFollowLaterAppendWriteback) {
    constexpr size_t kPageSize = 4096;
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/fsync_frozen_frontier";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR | O_APPEND, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    const std::string page(kPageSize, 'F');
    for (int index = 0; index < 8; ++index) {
        ASSERT_EQ(static_cast<ssize_t>(page.size()), write(fd, page.data(), page.size()))
            << strerror(errno);
    }

    std::atomic<bool> stop{false};
    std::atomic<int> appends{0};
    std::atomic<int> writer_error{0};
    std::thread writer([&] {
        while (!stop.load(std::memory_order_acquire)) {
            off_t before = lseek(fd, 0, SEEK_END);
            if (before < 0
                || write(fd, page.data(), page.size())
                    != static_cast<ssize_t>(page.size())
                || sync_file_range(fd, before, page.size(), SYNC_FILE_RANGE_WRITE) != 0) {
                writer_error.store(errno == 0 ? EIO : errno, std::memory_order_release);
                return;
            }
            appends.fetch_add(1, std::memory_order_release);
            std::this_thread::yield();
        }
    });
    while (appends.load(std::memory_order_acquire) < 8
           && writer_error.load(std::memory_order_acquire) == 0) {
        std::this_thread::yield();
    }
    if (writer_error.load(std::memory_order_acquire) != 0) {
        stop.store(true, std::memory_order_release);
        writer.join();
        FAIL() << strerror(writer_error.load(std::memory_order_acquire));
    }
    const int before_fsync = appends.load(std::memory_order_acquire);

    // The writer keeps creating and actively starting higher-index
    // writeback. fsync must wait only for its frozen dirty/in-flight
    // incarnations and return without requiring the producer to stop.
    const int fsync_result = fsync(fd);
    const int fsync_error = errno;
    const int after_fsync = appends.load(std::memory_order_acquire);
    stop.store(true, std::memory_order_release);
    writer.join();

    ASSERT_EQ(0, fsync_result) << strerror(fsync_error);
    ASSERT_EQ(0, writer_error.load(std::memory_order_acquire));
    EXPECT_GE(after_fsync, before_fsync);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, DelayedAndEagerWritesPersistCtimeAcrossRemount) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/write_ctime";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    struct stat initial = {};
    ASSERT_EQ(0, fstat(fd, &initial)) << strerror(errno);

    sleep(1);
    ASSERT_EQ(1, pwrite(fd, "A", 1, 0)) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    struct stat delayed = {};
    ASSERT_EQ(0, fstat(fd, &delayed)) << strerror(errno);
    EXPECT_GT(delayed.st_ctime, initial.st_ctime);

    // This is an overwrite of an already mapped block, so it exercises the
    // eager buffered fallback rather than the append-only delayed mapper.
    // Keep a second mount alive as an independent on-disk observer: checking
    // it immediately after fsync proves that fsync itself committed the
    // timestamp, without relying on the first mount's later clean unmount.
    ASSERT_NO_FATAL_FAILURE(fs.MountSecond());
    sleep(1);
    ASSERT_EQ(1, pwrite(fd, "B", 1, 0)) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    struct stat eager = {};
    ASSERT_EQ(0, fstat(fd, &eager)) << strerror(errno);
    EXPECT_GT(eager.st_ctime, delayed.st_ctime);
    struct stat independently_observed = {};
    const std::string second_path = fs.second_mount_point() + "/write_ctime";
    ASSERT_EQ(0, stat(second_path.c_str(), &independently_observed)) << strerror(errno);
    EXPECT_EQ(eager.st_ctime, independently_observed.st_ctime);
    ASSERT_NO_FATAL_FAILURE(fs.UnmountSecond());
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());

    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_EQ(0, stat(path.c_str(), &eager)) << strerror(errno);
    EXPECT_GT(eager.st_ctime, delayed.st_ctime);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, ConcurrentAppendRecordsRemainAtomicAndDurable) {
    constexpr int kWriters = 4;
    constexpr int kRecordsPerWriter = 32;
    constexpr size_t kRecordSize = 64;
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/concurrent_append";
    int creator = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(creator, 0) << strerror(errno);
    std::atomic<bool> start{false};
    std::atomic<int> first_error{0};
    std::thread writers[kWriters];
    for (int writer = 0; writer < kWriters; ++writer) {
        writers[writer] = std::thread([&, writer] {
            int fd = open(path.c_str(), O_WRONLY | O_APPEND);
            if (fd < 0) {
                int expected = 0;
                first_error.compare_exchange_strong(expected, errno);
                return;
            }
            while (!start.load(std::memory_order_acquire)) {
            }
            char record[kRecordSize];
            memset(record, 'A' + writer, sizeof(record));
            for (int index = 0; index < kRecordsPerWriter; ++index) {
                if (write(fd, record, sizeof(record))
                    != static_cast<ssize_t>(sizeof(record))) {
                    int error = errno == 0 ? EIO : errno;
                    int expected = 0;
                    first_error.compare_exchange_strong(expected, error);
                    break;
                }
            }
            close(fd);
        });
    }
    start.store(true, std::memory_order_release);
    for (auto& writer : writers) {
        writer.join();
    }
    ASSERT_EQ(0, first_error.load()) << strerror(first_error.load());
    ASSERT_EQ(0, fsync(creator)) << strerror(errno);
    ASSERT_EQ(0, close(creator)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());

    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    int fd = open(path.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    const size_t expected_size =
        kWriters * kRecordsPerWriter * kRecordSize;
    std::string contents(expected_size, '\0');
    ASSERT_EQ(static_cast<ssize_t>(contents.size()),
              pread(fd, contents.data(), contents.size(), 0))
        << strerror(errno);
    int counts[kWriters] = {};
    for (size_t offset = 0; offset < contents.size(); offset += kRecordSize) {
        const char marker = contents[offset];
        ASSERT_GE(marker, 'A');
        ASSERT_LT(marker, 'A' + kWriters);
        EXPECT_TRUE(std::all_of(contents.begin() + offset,
                                contents.begin() + offset + kRecordSize,
                                [marker](char byte) { return byte == marker; }));
        ++counts[marker - 'A'];
    }
    for (int writer = 0; writer < kWriters; ++writer) {
        EXPECT_EQ(kRecordsPerWriter, counts[writer]);
    }
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, SameBlockDeviceCanBeMountedTwice) {
    // Linux reuses a block device's superblock for a compatible second mount.
    // DragonOS does not have that shared-superblock layer yet, but a dormant
    // delayed-allocation experiment must not turn the established mount(2)
    // behavior into unconditional EBUSY. This test intentionally asserts
    // only mount namespace admission; page-cache sharing is a separate VFS
    // design task and must not be emulated by this ext4 regression test.
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.MountSecond());
    ASSERT_NO_FATAL_FAILURE(fs.UnmountSecond());
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, OpenFileSurvivesFinalUnlink) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/open_unlink";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr char kBefore[] = "before-unlink";
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, kBefore, sizeof(kBefore) - 1));

    struct stat before = {};
    ASSERT_EQ(0, fstat(fd, &before)) << strerror(errno);
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
    EXPECT_EQ(-1, access(path.c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno);

    ASSERT_EQ(0, lseek(fd, 0, SEEK_SET)) << strerror(errno);
    char buffer[64] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(kBefore) - 1),
              read(fd, buffer, sizeof(buffer)))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(buffer, kBefore, sizeof(kBefore) - 1));

    constexpr char kAfter[] = "-after";
    ASSERT_EQ(static_cast<off_t>(sizeof(kBefore) - 1),
              lseek(fd, 0, SEEK_END))
        << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, kAfter, sizeof(kAfter) - 1));
    ASSERT_EQ(0, fdatasync(fd)) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    struct stat unlinked = {};
    ASSERT_EQ(0, fstat(fd, &unlinked)) << strerror(errno);
    EXPECT_EQ(before.st_ino, unlinked.st_ino);
    EXPECT_EQ(0u, unlinked.st_nlink);
    EXPECT_EQ(static_cast<off_t>(sizeof(kBefore) + sizeof(kAfter) - 2),
              unlinked.st_size);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, DeletedFdSupportsTruncateAndFallocate) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/deleted_fd_mutation";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr char kPrefix[] = "retained-prefix";
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, kPrefix, sizeof(kPrefix) - 1));
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_EQ(0u, st.st_nlink);

    ASSERT_EQ(0, ftruncate(fd, 8)) << strerror(errno);
    ASSERT_EQ(0, fallocate(fd, 0, 0, 8192)) << strerror(errno);
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_EQ(8192, st.st_size);
    char data[16] = {};
    ASSERT_EQ(16, pread(fd, data, sizeof(data), 0)) << strerror(errno);
    EXPECT_EQ(0, memcmp(data, kPrefix, 8));
    for (size_t i = 8; i < sizeof(data); ++i) {
        EXPECT_EQ(0, data[i]);
    }
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, ReadAtimeIsImmediatelyVisibleAndPersistsAfterFsync) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/read_atime";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr char kData[] = "atime";
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, kData, sizeof(kData) - 1));
    const timespec old_times[2] = {{1, 0}, {2, 0}};
    ASSERT_EQ(0, futimens(fd, old_times)) << strerror(errno);

    char byte = 0;
    ASSERT_EQ(1, pread(fd, &byte, 1, 0)) << strerror(errno);
    EXPECT_EQ(kData[0], byte);

    struct stat cached {};
    ASSERT_EQ(0, fstat(fd, &cached)) << strerror(errno);
    EXPECT_GT(cached.st_atim.tv_sec, old_times[0].tv_sec);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());

    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    struct stat persisted {};
    ASSERT_EQ(0, stat(path.c_str(), &persisted)) << strerror(errno);
    EXPECT_EQ(cached.st_atim.tv_sec, persisted.st_atim.tv_sec);
    EXPECT_EQ(cached.st_atim.tv_nsec, persisted.st_atim.tv_nsec);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, RelatimeSkipsRecentAtimeNewerThanMtimeAndCtime) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/relatime_skip";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, "x", 1));

    timespec now = {};
    ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &now)) << strerror(errno);
    const timespec times[2] = {{now.tv_sec + 3600, 0}, {now.tv_sec - 3600, 0}};
    ASSERT_EQ(0, futimens(fd, times)) << strerror(errno);

    struct stat before = {};
    ASSERT_EQ(0, fstat(fd, &before)) << strerror(errno);
    char byte = 0;
    ASSERT_EQ(1, pread(fd, &byte, 1, 0)) << strerror(errno);
    struct stat after = {};
    ASSERT_EQ(0, fstat(fd, &after)) << strerror(errno);
    EXPECT_EQ(before.st_atim.tv_sec, after.st_atim.tv_sec);
    EXPECT_EQ(before.st_atim.tv_nsec, after.st_atim.tv_nsec);

    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, DirtySharedMappingSurvivesUnlinkAndFdClose) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/dirty_mapping";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, ftruncate(fd, 8192)) << strerror(errno);
    char* mapping = static_cast<char*>(
        mmap(nullptr, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    memcpy(mapping + 64, "first-page", 10);
    memcpy(mapping + 4096 + 32, "second-page", 11);
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
    ASSERT_EQ(0, msync(mapping, 8192, MS_SYNC)) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    EXPECT_EQ(0, memcmp(mapping + 64, "first-page", 10));
    memcpy(mapping + 4096 + 32, "after-close", 11);
    ASSERT_EQ(0, msync(mapping + 4096, 4096, MS_SYNC)) << strerror(errno);
    EXPECT_EQ(-1, umount(fs.mount_point().c_str()));
    EXPECT_EQ(EBUSY, errno);
    ASSERT_EQ(0, munmap(mapping, 8192)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, SharedMmapWriteSerializesWithTruncate) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/mmap_truncate_race";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(0, ftruncate(fd, 8192)) << strerror(errno);
    char* mapping = static_cast<char*>(
        mmap(nullptr, 8192, PROT_READ | PROT_WRITE, MAP_SHARED, fd, 0));
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);

    std::atomic<bool> start{false};
    std::atomic<int> first_error{0};
    auto record_error = [&](int error) {
        int expected = 0;
        first_error.compare_exchange_strong(expected, error);
    };
    std::thread writer([&] {
        while (!start.load(std::memory_order_acquire)) {
        }
        for (int iteration = 0; iteration < 64; ++iteration) {
            if (madvise(mapping, 4096, MADV_DONTNEED) != 0) {
                record_error(errno);
                return;
            }
            mapping[iteration % 64] = static_cast<char>(iteration);
            if (msync(mapping, 4096, MS_SYNC) != 0) {
                record_error(errno);
                return;
            }
        }
    });
    std::thread truncater([&] {
        while (!start.load(std::memory_order_acquire)) {
        }
        for (int iteration = 0; iteration < 64; ++iteration) {
            if (ftruncate(fd, (iteration & 1) ? 8192 : 4096) != 0) {
                record_error(errno);
                return;
            }
        }
    });
    start.store(true, std::memory_order_release);
    writer.join();
    truncater.join();
    EXPECT_EQ(0, first_error.load());
    ASSERT_EQ(0, ftruncate(fd, 8192)) << strerror(errno);
    ASSERT_EQ(0, msync(mapping, 4096, MS_SYNC)) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, munmap(mapping, 8192)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, SharedMmapBufferedWriteAndFsyncMakeProgress) {
    ASSERT_NO_FATAL_FAILURE(RunMmapBufferedWriteAndFsyncProgress(MAP_SHARED));
}

TEST(Ext4InodeIdentity, PrivateMmapCowBufferedWriteAndFsyncMakeProgress) {
    ASSERT_NO_FATAL_FAILURE(RunMmapBufferedWriteAndFsyncProgress(MAP_PRIVATE));
}

TEST(Ext4InodeIdentity, BufferedPartialTailWritebackSerializesWithTruncate) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/buffered_tail_truncate_race";
    int writer_fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(writer_fd, 0) << strerror(errno);
    int sync_fd = dup(writer_fd);
    ASSERT_GE(sync_fd, 0) << strerror(errno);

    constexpr off_t kPageSize = 4096;
    constexpr off_t kTailSize = kPageSize + 37;
    constexpr off_t kTailByte = kTailSize - 1;
    ASSERT_EQ(0, ftruncate(writer_fd, kPageSize)) << strerror(errno);

    std::atomic<bool> start{false};
    std::atomic<int> first_error{0};
    auto record_error = [&](int error) {
        int expected = 0;
        first_error.compare_exchange_strong(expected, error);
    };
    std::thread writer([&] {
        while (!start.load(std::memory_order_acquire)) {
        }
        for (int iteration = 0; iteration < 32; ++iteration) {
            if (ftruncate(writer_fd, kPageSize) != 0) {
                record_error(errno);
                return;
            }
            const char byte = static_cast<char>('a' + (iteration % 26));
            if (pwrite(writer_fd, &byte, 1, kTailByte) != 1) {
                record_error(errno);
                return;
            }
        }
    });
    std::thread syncer([&] {
        while (!start.load(std::memory_order_acquire)) {
        }
        for (int iteration = 0; iteration < 32; ++iteration) {
            if (fsync(sync_fd) != 0) {
                record_error(errno);
                return;
            }
        }
    });
    start.store(true, std::memory_order_release);
    writer.join();
    syncer.join();

    EXPECT_EQ(0, first_error.load());
    ASSERT_EQ(0, ftruncate(writer_fd, kTailSize)) << strerror(errno);
    constexpr char kExpected = '!';
    ASSERT_EQ(1, pwrite(writer_fd, &kExpected, 1, kTailByte)) << strerror(errno);
    ASSERT_EQ(0, fsync(writer_fd)) << strerror(errno);

    struct stat st = {};
    ASSERT_EQ(0, fstat(writer_fd, &st)) << strerror(errno);
    EXPECT_EQ(kTailSize, st.st_size);
    constexpr size_t kExtendedBytes = static_cast<size_t>(kTailSize - kPageSize);
    char observed[kExtendedBytes] = {};
    ASSERT_EQ(static_cast<ssize_t>(kExtendedBytes),
              pread(writer_fd, observed, kExtendedBytes, kPageSize))
        << strerror(errno);
    for (size_t offset = 0; offset + 1 < kExtendedBytes; ++offset) {
        EXPECT_EQ('\0', observed[offset]) << "unexpected data at tail offset " << offset;
    }
    EXPECT_EQ(kExpected, observed[kExtendedBytes - 1]);

    ASSERT_EQ(0, close(sync_fd)) << strerror(errno);
    ASSERT_EQ(0, close(writer_fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, FinalAndNonFinalHardLinkRemoval) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string first = fs.mount_point() + "/hardlink_first";
    const std::string second = fs.mount_point() + "/hardlink_second";
    int fd = open(first.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr char kData[] = "hard-link-lifetime";
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, kData, sizeof(kData) - 1));
    ASSERT_EQ(0, link(first.c_str(), second.c_str())) << strerror(errno);
    ASSERT_EQ(0, unlink(first.c_str())) << strerror(errno);
    struct stat fd_stat = {};
    struct stat path_stat = {};
    ASSERT_EQ(0, fstat(fd, &fd_stat)) << strerror(errno);
    ASSERT_EQ(0, stat(second.c_str(), &path_stat)) << strerror(errno);
    EXPECT_EQ(1u, fd_stat.st_nlink);
    EXPECT_EQ(fd_stat.st_ino, path_stat.st_ino);

    ASSERT_EQ(0, unlink(second.c_str())) << strerror(errno);
    ASSERT_EQ(0, fstat(fd, &fd_stat)) << strerror(errno);
    EXPECT_EQ(0u, fd_stat.st_nlink);
    EXPECT_EQ(-1, access(second.c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno);
    char data[sizeof(kData)] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(kData) - 1),
              pread(fd, data, sizeof(kData) - 1, 0))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(data, kData, sizeof(kData) - 1));
    ASSERT_EQ(1, pwrite(fd, "!", 1, sizeof(kData) - 1)) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, LazyUnmountPreservesLiveUnlinkedFd) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/lazy_unmount";
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr char kData[] = "before-detach";
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, kData, sizeof(kData) - 1));
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
    EXPECT_EQ(-1, umount(fs.mount_point().c_str()));
    EXPECT_EQ(EBUSY, errno);
    ASSERT_NO_FATAL_FAILURE(fs.Detach());

    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st)) << strerror(errno);
    EXPECT_EQ(0u, st.st_nlink);
    ASSERT_EQ(1, pwrite(fd, "!", 1, sizeof(kData) - 1)) << strerror(errno);
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    char data[sizeof(kData)] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(kData) - 1),
              pread(fd, data, sizeof(kData) - 1, 0))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(data, kData, sizeof(kData) - 1));
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.FinishDetached());
}

TEST(Ext4InodeIdentity, DirtyClosedUnlinkSyncfsCompletes) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/dirty_closed_unlink";
    int sync_fd = open(fs.mount_point().c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(sync_fd, 0) << strerror(errno);
    int fd = open(path.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr char kPayload[] = "dirty-without-pre-fsync";
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, kPayload, sizeof(kPayload) - 1));
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
    ASSERT_EQ(0, syscall(__NR_syncfs, sync_fd)) << strerror(errno);
    ASSERT_EQ(0, close(sync_fd)) << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, FsyncBeforeFinalCloseReclaimsUnlinkedBlocks) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    for (int iteration = 0; iteration < 3; ++iteration) {
        struct statvfs before = {};
        ASSERT_EQ(0, statvfs(fs.mount_point().c_str(), &before)) << strerror(errno);

        const std::string path = fs.mount_point() + "/fsync_unlinked_reclaim_" +
                                 std::to_string(iteration);
        int fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
        ASSERT_GE(fd, 0) << strerror(errno);
        char block[64 * 1024];
        memset(block, 0x5a, sizeof(block));
        for (int i = 0; i < 8; ++i) {
            ASSERT_NO_FATAL_FAILURE(WriteAll(fd, block, sizeof(block)));
        }
        ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);
        ASSERT_EQ(0, fsync(fd)) << strerror(errno);
        ASSERT_EQ(0, close(fd)) << strerror(errno);

        struct statvfs after = {};
        bool reclaimed = false;
        for (int i = 0; i < 20; ++i) {
            ASSERT_EQ(0, statvfs(fs.mount_point().c_str(), &after)) << strerror(errno);
            if (after.f_bfree >= before.f_bfree) {
                reclaimed = true;
                break;
            }
            usleep(5 * 1000);
        }
        EXPECT_TRUE(reclaimed) << "iteration=" << iteration
                               << " free blocks before=" << before.f_bfree
                               << " after=" << after.f_bfree;
    }

    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, RecreatedPathIsIndependentFromLiveUnlinkedInode) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/recreated";
    int old_fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(old_fd, 0) << strerror(errno);
    constexpr char kOld[] = "old-lifetime";
    ASSERT_NO_FATAL_FAILURE(WriteAll(old_fd, kOld, sizeof(kOld) - 1));
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);

    int new_fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(new_fd, 0) << strerror(errno);
    constexpr char kNew[] = "new-lifetime-data";
    ASSERT_NO_FATAL_FAILURE(WriteAll(new_fd, kNew, sizeof(kNew) - 1));

    struct stat old_stat = {};
    struct stat new_stat = {};
    ASSERT_EQ(0, fstat(old_fd, &old_stat)) << strerror(errno);
    ASSERT_EQ(0, fstat(new_fd, &new_stat)) << strerror(errno);
    EXPECT_NE(old_stat.st_ino, new_stat.st_ino);
    EXPECT_EQ(0u, old_stat.st_nlink);
    EXPECT_EQ(1u, new_stat.st_nlink);
    ASSERT_EQ(0, lseek(old_fd, 0, SEEK_SET)) << strerror(errno);
    char old_data[sizeof(kOld)] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(kOld) - 1),
              read(old_fd, old_data, sizeof(old_data)))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(old_data, kOld, sizeof(kOld) - 1));
    ASSERT_EQ(0, fsync(old_fd)) << strerror(errno);
    ASSERT_EQ(0, fsync(new_fd)) << strerror(errno);
    ASSERT_EQ(0, close(old_fd)) << strerror(errno);
    ASSERT_EQ(0, close(new_fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(path.c_str())) << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, ConcurrentWriteUnlinkSyncAndClose) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string path = fs.mount_point() + "/concurrent_unlink";
    int owner_fd = open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(owner_fd, 0) << strerror(errno);
    int writer_fd = dup(owner_fd);
    int sync_fd = dup(owner_fd);
    int close_fd = dup(owner_fd);
    int drain_fd = open(fs.mount_point().c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(writer_fd, 0) << strerror(errno);
    ASSERT_GE(sync_fd, 0) << strerror(errno);
    ASSERT_GE(close_fd, 0) << strerror(errno);
    ASSERT_GE(drain_fd, 0) << strerror(errno);

    std::atomic<bool> start{false};
    std::atomic<int> first_error{0};
    auto record_error = [&](int error) {
        int expected = 0;
        first_error.compare_exchange_strong(expected, error);
    };
    std::thread writer([&] {
        while (!start.load(std::memory_order_acquire)) {
        }
        constexpr char kChunk[] = "concurrent-data";
        for (int i = 0; i < 64; ++i) {
            if (pwrite(writer_fd, kChunk, sizeof(kChunk) - 1,
                       static_cast<off_t>(i * (sizeof(kChunk) - 1))) !=
                static_cast<ssize_t>(sizeof(kChunk) - 1)) {
                record_error(errno);
                break;
            }
            if ((i & 7) == 0 && fdatasync(writer_fd) != 0) {
                record_error(errno);
                break;
            }
        }
        if (close(writer_fd) != 0) {
            record_error(errno);
        }
    });
    std::thread syncer([&] {
        while (!start.load(std::memory_order_acquire)) {
        }
        for (int i = 0; i < 16; ++i) {
            if (syscall(__NR_syncfs, sync_fd) != 0) {
                record_error(errno);
                break;
            }
        }
        if (close(sync_fd) != 0) {
            record_error(errno);
        }
    });
    std::thread closer([&] {
        while (!start.load(std::memory_order_acquire)) {
        }
        if (close(close_fd) != 0) {
            record_error(errno);
        }
    });

    start.store(true, std::memory_order_release);
    int unlink_result = unlink(path.c_str());
    int unlink_errno = errno;
    int owner_close_result = close(owner_fd);
    int owner_close_errno = errno;
    writer.join();
    syncer.join();
    closer.join();
    ASSERT_EQ(0, unlink_result) << strerror(unlink_errno);
    ASSERT_EQ(0, owner_close_result) << strerror(owner_close_errno);
    EXPECT_EQ(0, first_error.load());
    ASSERT_EQ(0, syscall(__NR_syncfs, drain_fd)) << strerror(errno);
    ASSERT_EQ(0, close(drain_fd)) << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, EmptyPathRelinkCancelsDeferredReclaim) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string old_path = fs.mount_point() + "/relink_source";
    const std::string new_path = fs.mount_point() + "/relink_target";
    int fd = open(old_path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(fd, 0) << strerror(errno);
    constexpr char kPayload[] = "relinked-data";
    ASSERT_NO_FATAL_FAILURE(WriteAll(fd, kPayload, sizeof(kPayload) - 1));
    ASSERT_EQ(0, fsync(fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(old_path.c_str())) << strerror(errno);

    constexpr int kAtEmptyPath = 0x1000;
    ASSERT_EQ(0, linkat(fd, "", AT_FDCWD, new_path.c_str(), kAtEmptyPath))
        << strerror(errno);
    struct stat linked = {};
    ASSERT_EQ(0, stat(new_path.c_str(), &linked)) << strerror(errno);
    EXPECT_EQ(1u, linked.st_nlink);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    EXPECT_EQ(std::string(kPayload, sizeof(kPayload) - 1),
              ReadFile(new_path.c_str()));
    ASSERT_EQ(0, unlink(new_path.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, CurrentDirectorySurvivesRmdir) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string directory = fs.mount_point() + "/removed_cwd";
    ASSERT_EQ(0, mkdir(directory.c_str(), 0755)) << strerror(errno);
    int directory_fd = open(directory.c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(directory_fd, 0) << strerror(errno);
    struct stat before = {};
    ASSERT_EQ(0, fstat(directory_fd, &before)) << strerror(errno);
    ASSERT_EQ(0, chdir(directory.c_str())) << strerror(errno);
    ASSERT_EQ(0, rmdir(directory.c_str())) << strerror(errno);

    struct stat current = {};
    ASSERT_EQ(0, stat(".", &current)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(current.st_mode));
    struct stat removed = {};
    ASSERT_EQ(0, fstat(directory_fd, &removed)) << strerror(errno);
    EXPECT_EQ(before.st_ino, removed.st_ino);
    EXPECT_EQ(0u, removed.st_nlink);

    ASSERT_EQ(0, chdir("/")) << strerror(errno);
    ASSERT_EQ(0, close(directory_fd)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, DirtyOpenRenameTargetSurvivesReplacement) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string source = fs.mount_point() + "/rename_source";
    const std::string target = fs.mount_point() + "/rename_target";
    int source_fd = open(source.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    int target_fd = open(target.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(source_fd, 0) << strerror(errno);
    ASSERT_GE(target_fd, 0) << strerror(errno);
    constexpr char kSource[] = "new-path-data";
    constexpr char kTarget[] = "old-open-data";
    ASSERT_NO_FATAL_FAILURE(WriteAll(source_fd, kSource, sizeof(kSource) - 1));
    ASSERT_NO_FATAL_FAILURE(WriteAll(target_fd, kTarget, sizeof(kTarget) - 1));
    struct stat old_target = {};
    ASSERT_EQ(0, fstat(target_fd, &old_target)) << strerror(errno);

    ASSERT_EQ(0, rename(source.c_str(), target.c_str())) << strerror(errno);
    EXPECT_EQ(std::string(kSource, sizeof(kSource) - 1), ReadFile(target.c_str()));
    struct stat replaced = {};
    ASSERT_EQ(0, fstat(target_fd, &replaced)) << strerror(errno);
    EXPECT_EQ(old_target.st_ino, replaced.st_ino);
    EXPECT_EQ(0u, replaced.st_nlink);
    ASSERT_EQ(0, lseek(target_fd, 0, SEEK_SET)) << strerror(errno);
    char buffer[sizeof(kTarget)] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(kTarget) - 1),
              read(target_fd, buffer, sizeof(buffer)))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(buffer, kTarget, sizeof(kTarget) - 1));
    constexpr char kSuffix[] = "-after-rename";
    ASSERT_EQ(static_cast<off_t>(sizeof(kTarget) - 1),
              lseek(target_fd, 0, SEEK_END));
    ASSERT_NO_FATAL_FAILURE(WriteAll(target_fd, kSuffix, sizeof(kSuffix) - 1));
    ASSERT_EQ(0, fdatasync(target_fd)) << strerror(errno);
    ASSERT_EQ(0, close(target_fd)) << strerror(errno);
    ASSERT_EQ(0, close(source_fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(target.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, RenameReplacementPreservesRemainingHardLink) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string source = fs.mount_point() + "/hardlink_source";
    const std::string target = fs.mount_point() + "/hardlink_target";
    const std::string alias = fs.mount_point() + "/hardlink_alias";
    int target_fd = open(target.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(target_fd, 0) << strerror(errno);
    constexpr char kOld[] = "linked-target";
    ASSERT_NO_FATAL_FAILURE(WriteAll(target_fd, kOld, sizeof(kOld) - 1));
    ASSERT_EQ(0, link(target.c_str(), alias.c_str())) << strerror(errno);
    int source_fd = open(source.c_str(), O_CREAT | O_EXCL | O_WRONLY, 0644);
    ASSERT_GE(source_fd, 0) << strerror(errno);
    ASSERT_EQ(0, close(source_fd)) << strerror(errno);

    ASSERT_EQ(0, rename(source.c_str(), target.c_str())) << strerror(errno);
    EXPECT_EQ(std::string(kOld, sizeof(kOld) - 1), ReadFile(alias.c_str()));
    struct stat remaining = {};
    ASSERT_EQ(0, fstat(target_fd, &remaining)) << strerror(errno);
    EXPECT_EQ(1u, remaining.st_nlink);
    ASSERT_EQ(0, unlink(alias.c_str())) << strerror(errno);
    ASSERT_EQ(0, fstat(target_fd, &remaining)) << strerror(errno);
    EXPECT_EQ(0u, remaining.st_nlink);
    ASSERT_EQ(0, fsync(target_fd)) << strerror(errno);
    ASSERT_EQ(0, close(target_fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(target.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, RenameExchangeDoesNotUnlinkEitherInode) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string left = fs.mount_point() + "/exchange_left";
    const std::string right = fs.mount_point() + "/exchange_right";
    int left_fd = open(left.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    int right_fd = open(right.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(left_fd, 0) << strerror(errno);
    ASSERT_GE(right_fd, 0) << strerror(errno);
    constexpr char kLeft[] = "left";
    constexpr char kRight[] = "right";
    ASSERT_NO_FATAL_FAILURE(WriteAll(left_fd, kLeft, sizeof(kLeft) - 1));
    ASSERT_NO_FATAL_FAILURE(WriteAll(right_fd, kRight, sizeof(kRight) - 1));
    ASSERT_EQ(0, syscall(__NR_renameat2, AT_FDCWD, left.c_str(), AT_FDCWD,
                         right.c_str(), RENAME_EXCHANGE))
        << strerror(errno);
    EXPECT_EQ(std::string(kRight, sizeof(kRight) - 1), ReadFile(left.c_str()));
    EXPECT_EQ(std::string(kLeft, sizeof(kLeft) - 1), ReadFile(right.c_str()));
    struct stat left_stat = {};
    struct stat right_stat = {};
    ASSERT_EQ(0, fstat(left_fd, &left_stat)) << strerror(errno);
    ASSERT_EQ(0, fstat(right_fd, &right_stat)) << strerror(errno);
    EXPECT_EQ(1u, left_stat.st_nlink);
    EXPECT_EQ(1u, right_stat.st_nlink);
    ASSERT_EQ(0, close(left_fd)) << strerror(errno);
    ASSERT_EQ(0, close(right_fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(left.c_str())) << strerror(errno);
    ASSERT_EQ(0, unlink(right.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, RenameWhiteoutPreservesTargetAndLeaksNoTemporaryName) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string source = fs.mount_point() + "/whiteout_source";
    const std::string target = fs.mount_point() + "/whiteout_target";
    int source_fd = open(source.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    int target_fd = open(target.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(source_fd, 0) << strerror(errno);
    ASSERT_GE(target_fd, 0) << strerror(errno);
    constexpr char kSource[] = "whiteout-new";
    constexpr char kTarget[] = "whiteout-old";
    ASSERT_NO_FATAL_FAILURE(WriteAll(source_fd, kSource, sizeof(kSource) - 1));
    ASSERT_NO_FATAL_FAILURE(WriteAll(target_fd, kTarget, sizeof(kTarget) - 1));

    ASSERT_EQ(0, syscall(__NR_renameat2, AT_FDCWD, source.c_str(), AT_FDCWD,
                         target.c_str(), RENAME_WHITEOUT))
        << strerror(errno);
    EXPECT_EQ(std::string(kSource, sizeof(kSource) - 1), ReadFile(target.c_str()));
    struct stat whiteout = {};
    ASSERT_EQ(0, lstat(source.c_str(), &whiteout)) << strerror(errno);
    EXPECT_TRUE(S_ISCHR(whiteout.st_mode));
    struct stat replaced = {};
    ASSERT_EQ(0, fstat(target_fd, &replaced)) << strerror(errno);
    EXPECT_EQ(0u, replaced.st_nlink);

    DIR* directory = opendir(fs.mount_point().c_str());
    ASSERT_NE(nullptr, directory) << strerror(errno);
    bool leaked_temporary = false;
    constexpr char kTemporaryPrefix[] = ".dragonos-whiteout-";
    while (dirent* entry = readdir(directory)) {
        if (strncmp(entry->d_name, kTemporaryPrefix,
                    sizeof(kTemporaryPrefix) - 1) == 0) {
            leaked_temporary = true;
        }
    }
    ASSERT_EQ(0, closedir(directory)) << strerror(errno);
    EXPECT_FALSE(leaked_temporary);

    ASSERT_EQ(0, fdatasync(target_fd)) << strerror(errno);
    ASSERT_EQ(0, close(target_fd)) << strerror(errno);
    ASSERT_EQ(0, close(source_fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(source.c_str())) << strerror(errno);
    ASSERT_EQ(0, unlink(target.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, RemountedHardLinksShareCanonicalInodeAndPageCache) {
    LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string dir_a = fs.mount_point() + "/a";
    const std::string dir_b = fs.mount_point() + "/b";
    const std::string source = dir_a + "/source";
    const std::string alias = dir_b + "/alias";

    ASSERT_EQ(0, mkdir(dir_a.c_str(), 0755)) << strerror(errno);
    ASSERT_EQ(0, mkdir(dir_b.c_str(), 0755)) << strerror(errno);
    int create_fd = open(source.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
    ASSERT_GE(create_fd, 0) << strerror(errno);
    constexpr char kInitial[] = "initial";
    ASSERT_NO_FATAL_FAILURE(WriteAll(create_fd, kInitial, sizeof(kInitial) - 1));
    ASSERT_EQ(0, fsync(create_fd)) << strerror(errno);
    ASSERT_EQ(0, close(create_fd)) << strerror(errno);
    ASSERT_EQ(0, link(source.c_str(), alias.c_str())) << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    struct stat source_stat = {};
    struct stat alias_stat = {};
    ASSERT_EQ(0, stat(source.c_str(), &source_stat)) << strerror(errno);
    ASSERT_EQ(0, stat(alias.c_str(), &alias_stat)) << strerror(errno);
    EXPECT_EQ(source_stat.st_dev, alias_stat.st_dev);
    EXPECT_EQ(source_stat.st_ino, alias_stat.st_ino);
    EXPECT_EQ(2u, source_stat.st_nlink);

    int alias_path_fd = open(alias.c_str(), O_RDONLY);
    ASSERT_GE(alias_path_fd, 0) << strerror(errno);
    const std::string proc_fd =
        "/proc/self/fd/" + std::to_string(alias_path_fd);
    char link_target[512] = {};
    ssize_t link_size =
        readlink(proc_fd.c_str(), link_target, sizeof(link_target) - 1);
    ASSERT_GT(link_size, 0) << strerror(errno);
    EXPECT_EQ(alias, std::string(link_target, static_cast<size_t>(link_size)));
    ASSERT_EQ(0, close(alias_path_fd)) << strerror(errno);

    int source_fd = open(source.c_str(), O_RDWR | O_TRUNC);
    ASSERT_GE(source_fd, 0) << strerror(errno);
    int alias_fd = open(alias.c_str(), O_RDONLY);
    ASSERT_GE(alias_fd, 0) << strerror(errno);

    constexpr char kUpdated[] = "shared-page-cache";
    ASSERT_NO_FATAL_FAILURE(WriteAll(source_fd, kUpdated, sizeof(kUpdated) - 1));
    ASSERT_EQ(0, lseek(alias_fd, 0, SEEK_SET)) << strerror(errno);
    char buffer[sizeof(kUpdated)] = {};
    ASSERT_EQ(static_cast<ssize_t>(sizeof(kUpdated) - 1),
              read(alias_fd, buffer, sizeof(kUpdated) - 1))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(buffer, kUpdated, sizeof(kUpdated) - 1));

    // The dirty mapping belongs to the canonical inode. Dropping one alias must
    // neither truncate it nor expose an eviction state to the surviving alias.
    ASSERT_EQ(0, unlink(alias.c_str())) << strerror(errno);
    ASSERT_EQ(0, lseek(source_fd, 0, SEEK_SET)) << strerror(errno);
    memset(buffer, 0, sizeof(buffer));
    ASSERT_EQ(static_cast<ssize_t>(sizeof(kUpdated) - 1),
              read(source_fd, buffer, sizeof(kUpdated) - 1))
        << strerror(errno);
    EXPECT_EQ(0, memcmp(buffer, kUpdated, sizeof(kUpdated) - 1));
    ASSERT_EQ(0, fstat(source_fd, &source_stat)) << strerror(errno);
    EXPECT_EQ(1u, source_stat.st_nlink);

    ASSERT_EQ(0, fsync(source_fd)) << strerror(errno);
    ASSERT_EQ(0, close(alias_fd)) << strerror(errno);
    ASSERT_EQ(0, close(source_fd)) << strerror(errno);

    ASSERT_EQ(0, unlink(source.c_str())) << strerror(errno);
    ASSERT_EQ(0, rmdir(dir_a.c_str())) << strerror(errno);
    ASSERT_EQ(0, rmdir(dir_b.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(Ext4InodeIdentity, BindAliasAndMapsShareDeletedDentryState) {
    const std::string base = "/tmp/dentry_bind_" + std::to_string(getpid());
    const std::string source_dir = base + "/source";
    const std::string bind_dir = base + "/bind";
    const std::string source = source_dir + "/file";
    const std::string keeper = source_dir + "/keeper";
    const std::string alias = bind_dir + "/file";

    ASSERT_EQ(0, mkdir(base.c_str(), 0700)) << strerror(errno);
    ASSERT_EQ(0, mkdir(source_dir.c_str(), 0700)) << strerror(errno);
    ASSERT_EQ(0, mkdir(bind_dir.c_str(), 0700)) << strerror(errno);
    int create_fd = open(source.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
    ASSERT_GE(create_fd, 0) << strerror(errno);
    ASSERT_EQ(0, ftruncate(create_fd, 4096)) << strerror(errno);
    ASSERT_EQ(0, close(create_fd)) << strerror(errno);
    ASSERT_EQ(0, link(source.c_str(), keeper.c_str())) << strerror(errno);
    ASSERT_EQ(0, mount(source_dir.c_str(), bind_dir.c_str(), nullptr, MS_BIND, nullptr))
        << strerror(errno);

    int alias_fd = open(alias.c_str(), O_RDWR);
    ASSERT_GE(alias_fd, 0) << strerror(errno);
    void* mapping = mmap(nullptr, 4096, PROT_READ, MAP_PRIVATE, alias_fd, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    ASSERT_EQ(0, unlink(source.c_str())) << strerror(errno);

    char target[512] = {};
    const std::string proc_fd = "/proc/self/fd/" + std::to_string(alias_fd);
    ssize_t target_size = readlink(proc_fd.c_str(), target, sizeof(target) - 1);
    ASSERT_GT(target_size, 0) << strerror(errno);
    const std::string deleted_path = alias + " (deleted)";
    EXPECT_EQ(deleted_path,
              std::string(target, static_cast<size_t>(target_size)));
    const std::string maps = ReadFile("/proc/self/maps");
    EXPECT_NE(std::string::npos, maps.find(deleted_path)) << maps;

    ASSERT_EQ(0, link(keeper.c_str(), source.c_str())) << strerror(errno);
    int recreated_fd = open(alias.c_str(), O_RDONLY);
    ASSERT_GE(recreated_fd, 0) << strerror(errno);
    const std::string recreated_proc_fd =
        "/proc/self/fd/" + std::to_string(recreated_fd);
    memset(target, 0, sizeof(target));
    target_size =
        readlink(recreated_proc_fd.c_str(), target, sizeof(target) - 1);
    ASSERT_GT(target_size, 0) << strerror(errno);
    EXPECT_EQ(alias, std::string(target, static_cast<size_t>(target_size)));
    ASSERT_EQ(0, close(recreated_fd)) << strerror(errno);

    ASSERT_EQ(0, munmap(mapping, 4096)) << strerror(errno);
    ASSERT_EQ(0, close(alias_fd)) << strerror(errno);
    ASSERT_EQ(0, unlink(source.c_str())) << strerror(errno);
    ASSERT_EQ(0, unlink(keeper.c_str())) << strerror(errno);
    ASSERT_EQ(0, umount(bind_dir.c_str())) << strerror(errno);
    ASSERT_EQ(0, rmdir(bind_dir.c_str())) << strerror(errno);
    ASSERT_EQ(0, rmdir(source_dir.c_str())) << strerror(errno);
    ASSERT_EQ(0, rmdir(base.c_str())) << strerror(errno);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
