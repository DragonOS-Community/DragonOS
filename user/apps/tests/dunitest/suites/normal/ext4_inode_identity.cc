#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <atomic>
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

    const std::string& mount_point() const {
        return mount_point_;
    }

  private:
    std::string image_;
    std::string mount_point_;
    std::string loop_path_;
    int backing_fd_ = -1;
    int loop_fd_ = -1;
    bool mounted_ = false;
};

void WriteAll(int fd, const char* data, size_t len) {
    size_t done = 0;
    while (done < len) {
        ssize_t written = write(fd, data + done, len - done);
        ASSERT_GT(written, 0) << strerror(errno);
        done += static_cast<size_t>(written);
    }
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
    ASSERT_EQ(0, chdir(directory.c_str())) << strerror(errno);
    ASSERT_EQ(0, rmdir(directory.c_str())) << strerror(errno);

    struct stat current = {};
    ASSERT_EQ(0, stat(".", &current)) << strerror(errno);
    EXPECT_TRUE(S_ISDIR(current.st_mode));

    ASSERT_EQ(0, chdir("/")) << strerror(errno);
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
