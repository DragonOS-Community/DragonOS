#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <atomic>
#include <string>
#include <thread>

#ifndef __NR_syncfs
#if defined(__x86_64__)
#define __NR_syncfs 306
#elif defined(__riscv) || defined(__loongarch64)
#define __NR_syncfs 267
#else
#error "__NR_syncfs is not defined for this architecture"
#endif
#endif

namespace {

#ifndef CLONE_NEWNS
#define CLONE_NEWNS 0x00020000
#endif

#ifndef MS_REC
#define MS_REC 16384
#endif

#ifndef MS_PRIVATE
#define MS_PRIVATE (1 << 18)
#endif

#ifndef MS_SHARED
#define MS_SHARED (1 << 20)
#endif

int EnsureDir(const char* path) {
    struct stat st;
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

long RawSyncfs(int fd) {
    return syscall(__NR_syncfs, fd);
}

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/tmp/dunitest_syncfs_XXXXXX";
        fd_ = mkstemp(tmpl);
        if (fd_ >= 0) {
            path_ = tmpl;
        }
    }

    ~TempFile() {
        if (fd_ >= 0) {
            close(fd_);
        }
        if (!path_.empty()) {
            unlink(path_.c_str());
        }
    }

    TempFile(const TempFile&) = delete;
    TempFile& operator=(const TempFile&) = delete;

    bool valid() const {
        return fd_ >= 0;
    }

    int fd() const {
        return fd_;
    }

    const char* path() const {
        return path_.c_str();
    }

    bool write_test_data() const {
        constexpr char kData[] = "DragonOS syncfs dunitest data\n";
        return write(fd_, kData, sizeof(kData) - 1) == static_cast<ssize_t>(sizeof(kData) - 1);
    }

  private:
    std::string path_;
    int fd_ = -1;
};

void ExpectSyncfsSucceeds(int fd) {
    errno = 0;
    EXPECT_EQ(0, RawSyncfs(fd));
    EXPECT_EQ(0, errno) << "got errno=" << errno << " (" << strerror(errno) << ")";
}

void ExpectSyncfsErrno(int fd, int expected_errno) {
    errno = 0;
    EXPECT_EQ(-1, RawSyncfs(fd));
    EXPECT_EQ(expected_errno, errno) << "got errno=" << errno << " (" << strerror(errno) << ")";
}

}  // namespace

TEST(SyncfsSemantics, InvalidFdReturnsEbadf) {
    ExpectSyncfsErrno(-1, EBADF);
}

TEST(SyncfsSemantics, RegularFileSucceedsAndPreservesOffset) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    ASSERT_TRUE(file.write_test_data()) << "write failed: " << strerror(errno);
    ASSERT_EQ(7, lseek(file.fd(), 7, SEEK_SET));

    ExpectSyncfsSucceeds(file.fd());
    EXPECT_EQ(7, lseek(file.fd(), 0, SEEK_CUR));
}

TEST(SyncfsSemantics, DirectoryFdSucceeds) {
    int dir_fd = open("/tmp", O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dir_fd, 0) << "open directory failed: " << strerror(errno);

    ExpectSyncfsSucceeds(dir_fd);

    close(dir_fd);
}

#ifdef O_PATH
TEST(SyncfsSemantics, OPathFdReturnsEbadf) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    int path_fd = open(file.path(), O_PATH);
    ASSERT_GE(path_fd, 0) << "open O_PATH failed: " << strerror(errno);

    ExpectSyncfsErrno(path_fd, EBADF);

    close(path_fd);
}
#endif

TEST(SyncfsSemantics, PipeFdsSucceed) {
    int pipefd[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipefd)) << "pipe failed: " << strerror(errno);

    ExpectSyncfsSucceeds(pipefd[0]);
    ExpectSyncfsSucceeds(pipefd[1]);

    close(pipefd[0]);
    close(pipefd[1]);
}

TEST(SyncfsSemantics, EventFdSucceeds) {
    int fd = eventfd(0, 0);
    ASSERT_GE(fd, 0) << "eventfd failed: " << strerror(errno);

    ExpectSyncfsSucceeds(fd);

    close(fd);
}

TEST(SyncfsSemantics, SocketPairFdsSucceed) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds))
            << "socketpair failed: " << strerror(errno);

    ExpectSyncfsSucceeds(fds[0]);
    ExpectSyncfsSucceeds(fds[1]);

    close(fds[0]);
    close(fds[1]);
}

TEST(SyncUmountLifetime, ConcurrentSyncAndUnmountPrivateRamfs) {
    const char* root = "/tmp/dunitest_sync_umount";
    const char* mountpoint = "/tmp/dunitest_sync_umount/mnt";
    const char* file_path = "/tmp/dunitest_sync_umount/mnt/file";

    ASSERT_EQ(0, EnsureDir("/tmp")) << strerror(errno);
    ASSERT_EQ(0, EnsureDir(root)) << strerror(errno);
    ASSERT_EQ(0, EnsureDir(mountpoint)) << strerror(errno);

    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << "unshare(CLONE_NEWNS) failed: " << strerror(errno);
    }

    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);

    for (int i = 0; i < 8; ++i) {
        if (mount("", mountpoint, "ramfs", 0, NULL) != 0) {
            GTEST_SKIP() << "mount ramfs failed: " << strerror(errno);
        }

        int fd = open(file_path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        if (fd < 0) {
            int saved_errno = errno;
            umount(mountpoint);
            errno = saved_errno;
            FAIL() << "open test file failed: " << strerror(errno);
        }
        if (write(fd, "data", 4) != 4) {
            int saved_errno = errno;
            close(fd);
            umount(mountpoint);
            errno = saved_errno;
            FAIL() << "write failed: " << strerror(errno);
        }
        close(fd);

        std::atomic<bool> stop{false};
        std::thread sync_thread([&stop]() {
            while (!stop.load(std::memory_order_relaxed)) {
                sync();
                sched_yield();
            }
        });

        usleep(1000);
        int umount_ret = umount(mountpoint);
        int saved_errno = errno;
        stop.store(true, std::memory_order_relaxed);
        sync_thread.join();
        errno = saved_errno;
        ASSERT_EQ(0, umount_ret) << "umount failed: " << strerror(errno);
    }

    rmdir(mountpoint);
    rmdir(root);
}

// Verify that umount under shared mount propagation does not deadlock.
// Call chain triggered by this test:
//   umount() -> propagate_umount() -> umount_at_peer()
// Before the fix, umount_at_peer called sync_filesystem() on a child sharing the
// same umount_lock, causing a same-thread self-deadlock (writer trying to acquire reader).
TEST(SyncUmountLifetime, SharedMountPropagationUmountNoDeadlock) {
    const char* root = "/tmp/dunitest_shared_umount";
    const char* mountpoint = "/tmp/dunitest_shared_umount/mnt";
    const char* file_path = "/tmp/dunitest_shared_umount/mnt/file";

    ASSERT_EQ(0, EnsureDir("/tmp")) << strerror(errno);
    ASSERT_EQ(0, EnsureDir(root)) << strerror(errno);
    ASSERT_EQ(0, EnsureDir(mountpoint)) << strerror(errno);

    // Enter a new mount namespace to avoid affecting other tests.
    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << "unshare(CLONE_NEWNS) failed: " << strerror(errno);
    }

    // Mark root as shared so subsequent mounts inherit shared propagation.
    if (mount(NULL, "/", NULL, MS_REC | MS_SHARED, NULL) != 0) {
        // Skip (not fail) if MS_SHARED is unsupported.
        GTEST_SKIP() << "mount --make-rshared failed: " << strerror(errno);
    }

    for (int i = 0; i < 4; ++i) {
        if (mount("", mountpoint, "ramfs", 0, NULL) != 0) {
            GTEST_SKIP() << "mount ramfs failed: " << strerror(errno);
        }

        // Write data to produce dirty pages, ensuring sync has real work during umount.
        int fd = open(file_path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
        if (fd >= 0) {
            write(fd, "shared-propagation-test", 23);
            close(fd);
        }

        // Concurrent sync + umount: if a deadlock exists, this will hang forever.
        std::atomic<bool> stop{false};
        std::thread sync_thread([&stop]() {
            while (!stop.load(std::memory_order_relaxed)) {
                sync();
                sched_yield();
            }
        });

        usleep(500);
        int ret = umount(mountpoint);
        int saved = errno;
        stop.store(true, std::memory_order_relaxed);
        sync_thread.join();

        // umount should succeed (or EINVAL if already removed by propagation).
        if (ret != 0 && saved != EINVAL) {
            errno = saved;
            FAIL() << "umount failed on iteration " << i << ": " << strerror(errno);
        }
    }

    // Restore to private to avoid affecting subsequent tests.
    mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL);
    rmdir(mountpoint);
    rmdir(root);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
