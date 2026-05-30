#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <sched.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/mount.h>
#include <sys/eventfd.h>
#include <sys/socket.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/wait.h>
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

#ifndef MNT_DETACH
#define MNT_DETACH 2
#endif

int EnsureDir(const char* path) {
    struct stat st;
    if (stat(path, &st) == 0) {
        return S_ISDIR(st.st_mode) ? 0 : -1;
    }
    return mkdir(path, 0755);
}

bool MountInfoContains(const char* path) {
    FILE* f = fopen("/proc/self/mountinfo", "r");
    if (f == nullptr) {
        return false;
    }

    char line[4096];
    bool found = false;
    while (fgets(line, sizeof(line), f) != nullptr) {
        if (strstr(line, path) != nullptr) {
            found = true;
            break;
        }
    }

    fclose(f);
    return found;
}

bool ReadOneByte(int fd) {
    char c;
    return read(fd, &c, 1) == 1;
}

bool ReadByte(int fd, char* c) {
    return read(fd, c, 1) == 1;
}

bool WriteOneByte(int fd, char c) {
    return write(fd, &c, 1) == 1;
}

void CleanupSharedUmountPaths(const char* child, const char* parent, const char* root) {
    umount2(child, MNT_DETACH);
    umount2(parent, MNT_DETACH);
    rmdir(child);
    rmdir(parent);
    rmdir(root);
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

    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) != 0) {
        int saved_errno = errno;
        rmdir(mountpoint);
        rmdir(root);
        errno = saved_errno;
        GTEST_SKIP() << "mount --make-rprivate / failed: " << strerror(errno);
    }

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

        std::atomic<bool> sync_started{false};
        std::atomic<bool> umount_started{false};
        std::atomic<int> sync_after_umount_started{0};
        std::thread sync_thread([&]() {
            sync_started.store(true, std::memory_order_release);
            while (!umount_started.load(std::memory_order_acquire)) {
                sync();
                sched_yield();
            }
            for (int n = 0; n < 64; ++n) {
                sync_after_umount_started.fetch_add(1, std::memory_order_relaxed);
                sync();
                sched_yield();
            }
        });

        while (!sync_started.load(std::memory_order_acquire)) {
            sched_yield();
        }
        usleep(1000);
        umount_started.store(true, std::memory_order_release);
        int umount_ret = umount(mountpoint);
        int saved_errno = errno;
        sync_thread.join();
        errno = saved_errno;
        ASSERT_GT(sync_after_umount_started.load(std::memory_order_relaxed), 0);
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
    const char* parent = "/tmp/dunitest_shared_umount/parent";
    const char* child = "/tmp/dunitest_shared_umount/parent/child";
    const char* file_path = "/tmp/dunitest_shared_umount/parent/child/file";

    ASSERT_EQ(0, EnsureDir("/tmp")) << strerror(errno);
    ASSERT_EQ(0, EnsureDir(root)) << strerror(errno);
    ASSERT_EQ(0, EnsureDir(parent)) << strerror(errno);

    // Enter a new mount namespace first so propagation setup cannot affect other tests.
    if (unshare(CLONE_NEWNS) != 0) {
        GTEST_SKIP() << "unshare(CLONE_NEWNS) failed: " << strerror(errno);
    }

    if (mount(NULL, "/", NULL, MS_REC | MS_PRIVATE, NULL) != 0) {
        int saved_errno = errno;
        CleanupSharedUmountPaths(child, parent, root);
        errno = saved_errno;
        GTEST_SKIP() << "mount --make-rprivate / failed: " << strerror(errno);
    }

    if (mount("", parent, "ramfs", 0, NULL) != 0) {
        GTEST_SKIP() << "mount parent ramfs failed: " << strerror(errno);
    }
    ASSERT_EQ(0, EnsureDir(child)) << strerror(errno);

    if (mount(NULL, parent, NULL, MS_SHARED, NULL) != 0) {
        umount2(parent, MNT_DETACH);
        GTEST_SKIP() << "mount --make-shared parent failed: " << strerror(errno);
    }

    int mounted_pipe[2];
    int go_pipe[2];
    int done_pipe[2];
    ASSERT_EQ(0, pipe(mounted_pipe)) << strerror(errno);
    ASSERT_EQ(0, pipe(go_pipe)) << strerror(errno);
    ASSERT_EQ(0, pipe(done_pipe)) << strerror(errno);

    pid_t pid = fork();
    ASSERT_GE(pid, 0) << strerror(errno);
    if (pid == 0) {
        close(mounted_pipe[0]);
        close(go_pipe[1]);
        close(done_pipe[0]);

        int exit_code = 1;
        do {
            if (unshare(CLONE_NEWNS) != 0) {
                exit_code = 10;
                WriteOneByte(mounted_pipe[1], 'e');
                break;
            }
            if (mount("", child, "ramfs", 0, NULL) != 0) {
                exit_code = 11;
                WriteOneByte(mounted_pipe[1], 'e');
                break;
            }

            int fd = open(file_path, O_CREAT | O_WRONLY | O_TRUNC, 0644);
            if (fd >= 0) {
                if (write(fd, "shared-propagation-test", 23) != 23) {
                    close(fd);
                    exit_code = 14;
                    break;
                }
                close(fd);
            }

            if (!WriteOneByte(mounted_pipe[1], 'm')) {
                exit_code = 12;
                break;
            }
            if (!ReadOneByte(go_pipe[0])) {
                exit_code = 13;
                break;
            }

            std::atomic<bool> stop{false};
            std::thread sync_thread([&stop]() {
                while (!stop.load(std::memory_order_relaxed)) {
                    sync();
                    sched_yield();
                }
            });

            usleep(500);
            int ret = umount(child);
            int saved = errno;
            stop.store(true, std::memory_order_relaxed);
            sync_thread.join();

            if (ret != 0 && saved != EINVAL) {
                exit_code = 20;
                break;
            }
            exit_code = 0;
        } while (false);

        WriteOneByte(done_pipe[1], static_cast<char>(exit_code));
        _exit(exit_code);
    }

    close(mounted_pipe[1]);
    close(go_pipe[0]);
    close(done_pipe[1]);

    char mounted_signal = 0;
    ASSERT_TRUE(ReadByte(mounted_pipe[0], &mounted_signal));
    if (mounted_signal != 'm') {
        int status = 0;
        waitpid(pid, &status, 0);
        FAIL() << "child failed before creating propagated mount";
    }
    bool propagated_to_parent = MountInfoContains(child);

    ASSERT_TRUE(WriteOneByte(go_pipe[1], 'u'));
    ASSERT_TRUE(ReadOneByte(done_pipe[0]));

    int status = 0;
    ASSERT_EQ(pid, waitpid(pid, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status)) << "child terminated abnormally";
    ASSERT_EQ(0, WEXITSTATUS(status)) << "child failed";

    ASSERT_TRUE(propagated_to_parent)
        << "child mount was not propagated into the peer namespace";
    ASSERT_FALSE(MountInfoContains(child))
        << "propagated child mount remained in peer namespace after umount";

    CleanupSharedUmountPaths(child, parent, root);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
