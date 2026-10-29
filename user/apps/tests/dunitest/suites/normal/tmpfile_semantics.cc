#include <gtest/gtest.h>

#include <cerrno>
#include <atomic>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <thread>
#include <sys/stat.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

namespace {

struct OpenHow {
    unsigned long long flags;
    unsigned long long mode;
    unsigned long long resolve;
};

int OpenAt2(int dirfd, const char* path, const OpenHow& how) {
    return static_cast<int>(syscall(437, dirfd, path, &how, sizeof(how)));
}

TEST(TmpfileSemantics, UnnamedRegularFileInTargetFilesystem) {
    char dir[] = "/root/dunitest_tmpfile_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir)) << strerror(errno);
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0) << strerror(errno);

    int fd = OpenAt2(dirfd, ".", OpenHow{O_TMPFILE | O_RDWR, 0600, 0});
    if (fd < 0) {
        ADD_FAILURE() << "O_TMPFILE: " << strerror(errno);
        EXPECT_EQ(0, close(dirfd));
        EXPECT_EQ(0, rmdir(dir));
        return;
    }
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st));
    EXPECT_TRUE(S_ISREG(st.st_mode));
    EXPECT_EQ(0UL, st.st_nlink);
    const char payload[] = "dragonos-tmpfile";
    EXPECT_EQ(static_cast<ssize_t>(sizeof(payload)), write(fd, payload, sizeof(payload)));
    EXPECT_EQ(0, lseek(fd, 0, SEEK_SET));
    char readback[sizeof(payload)] = {};
    EXPECT_EQ(static_cast<ssize_t>(sizeof(readback)), read(fd, readback, sizeof(readback)));
    EXPECT_STREQ(payload, readback);

    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, close(dirfd));
    EXPECT_EQ(0, rmdir(dir));
}

TEST(TmpfileSemantics, PublishOnceAndExclusiveNeverPublishes) {
    char dir[] = "/root/dunitest_tmpfile_link_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir)) << strerror(errno);
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0) << strerror(errno);

    int fd = OpenAt2(dirfd, ".", OpenHow{O_TMPFILE | O_RDWR, 0600, 0});
    if (fd < 0) {
        ADD_FAILURE() << "O_TMPFILE: " << strerror(errno);
        close(dirfd);
        rmdir(dir);
        return;
    }
    ASSERT_EQ(0, linkat(fd, "", dirfd, "published", AT_EMPTY_PATH)) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st));
    EXPECT_EQ(1UL, st.st_nlink);
    int named = openat(dirfd, "published", O_RDONLY);
    ASSERT_GE(named, 0) << strerror(errno);
    struct stat named_stat = {};
    EXPECT_EQ(0, fstat(named, &named_stat));
    EXPECT_EQ(st.st_ino, named_stat.st_ino);
    EXPECT_EQ(0, close(named));

    int exclusive = OpenAt2(dirfd, ".", OpenHow{O_TMPFILE | O_RDWR | O_EXCL, 0600, 0});
    ASSERT_GE(exclusive, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, linkat(exclusive, "", dirfd, "forbidden", AT_EMPTY_PATH));
    EXPECT_EQ(ENOENT, errno);
    EXPECT_EQ(0, close(exclusive));
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, unlinkat(dirfd, "published", 0));
    EXPECT_EQ(0, close(dirfd));
    EXPECT_EQ(0, rmdir(dir));
}

TEST(TmpfileSemantics, TmpfsUsesSelectedFilesystem) {
    char dir[] = "/dev/shm/dunitest_tmpfile_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir)) << strerror(errno);
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0) << strerror(errno);
    int fd = openat(dirfd, ".", O_TMPFILE | O_RDWR, 0600);
    if (fd < 0) {
        ADD_FAILURE() << "tmpfs O_TMPFILE: " << strerror(errno);
        close(dirfd);
        rmdir(dir);
        return;
    }
    struct stat file_stat = {};
    struct stat dir_stat = {};
    ASSERT_EQ(0, fstat(fd, &file_stat));
    ASSERT_EQ(0, fstat(dirfd, &dir_stat));
    EXPECT_EQ(dir_stat.st_dev, file_stat.st_dev);
    EXPECT_EQ(0UL, file_stat.st_nlink);
    ASSERT_EQ(0, linkat(fd, "", dirfd, "published", AT_EMPTY_PATH)) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    int named = openat(dirfd, "published", O_RDWR);
    EXPECT_GE(named, 0) << strerror(errno);
    if (named >= 0) close(named);
    EXPECT_EQ(0, unlinkat(dirfd, "published", 0));
    EXPECT_EQ(0, close(dirfd));
    EXPECT_EQ(0, rmdir(dir));
}

TEST(TmpfileSemantics, DeletedOrdinaryFileCannotBeResurrected) {
    char dir[] = "/root/dunitest_unlinked_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir));
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0);
    int fd = openat(dirfd, "old", O_CREAT | O_EXCL | O_RDWR, 0600);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(0, unlinkat(dirfd, "old", 0));
    errno = 0;
    EXPECT_EQ(-1, linkat(fd, "", dirfd, "resurrected", AT_EMPTY_PATH));
    EXPECT_EQ(ENOENT, errno);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, close(dirfd));
    EXPECT_EQ(0, rmdir(dir));
}

TEST(TmpfileSemantics, CannotPublishAcrossMounts) {
    char dir[] = "/root/dunitest_cross_mount_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir));
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0);
    int other = open("/dev/shm", O_RDONLY | O_DIRECTORY);
    ASSERT_GE(other, 0);
    int fd = openat(dirfd, ".", O_TMPFILE | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, linkat(fd, "", other, "dunitest_wrong_mount", AT_EMPTY_PATH));
    EXPECT_EQ(EXDEV, errno);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, close(other));
    EXPECT_EQ(0, close(dirfd));
    EXPECT_EQ(0, rmdir(dir));
}

TEST(TmpfileSemantics, DupAndTruncateRetainUnnamedFile) {
    char dir[] = "/root/dunitest_tmpfile_dup_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir));
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0);
    int fd = openat(dirfd, ".", O_TMPFILE | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    int duplicate = dup(fd);
    ASSERT_GE(duplicate, 0);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, ftruncate(duplicate, 8192));
    struct stat st = {};
    EXPECT_EQ(0, fstat(duplicate, &st));
    EXPECT_EQ(8192, st.st_size);
    EXPECT_EQ(0UL, st.st_nlink);
    void* mapping = mmap(nullptr, 4096, PROT_READ | PROT_WRITE, MAP_SHARED, duplicate, 0);
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    static_cast<char*>(mapping)[0] = 'X';
    EXPECT_EQ(0, msync(mapping, 4096, MS_SYNC));
    char observed = 0;
    EXPECT_EQ(1, pread(duplicate, &observed, 1, 0));
    EXPECT_EQ('X', observed);
    EXPECT_EQ(0, fsync(duplicate));
    EXPECT_EQ(0, close(duplicate));
    EXPECT_EQ('X', static_cast<char*>(mapping)[0]);
    EXPECT_EQ(0, munmap(mapping, 4096));
    EXPECT_EQ(0, close(dirfd));
    EXPECT_EQ(0, rmdir(dir));
}

TEST(TmpfileSemantics, RemovedTmpfsDirectoryStillCreatesUnnamedFile) {
    char dir[] = "/dev/shm/dunitest_dead_tmpfile_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir));
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0);
    ASSERT_EQ(0, rmdir(dir));
    int fd = openat(dirfd, ".", O_TMPFILE | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    struct stat st = {};
    EXPECT_EQ(0, fstat(fd, &st));
    EXPECT_EQ(0UL, st.st_nlink);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, close(dirfd));
}

TEST(TmpfileSemantics, RemovedExt4DirectoryRejectsCreation) {
    char dir[] = "/root/dunitest_dead_ext4_tmpfile_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir));
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0);
    ASSERT_EQ(0, rmdir(dir));
    errno = 0;
    EXPECT_EQ(-1, openat(dirfd, ".", O_TMPFILE | O_RDWR, 0600));
    EXPECT_EQ(EPERM, errno);
    EXPECT_EQ(0, close(dirfd));
}

TEST(TmpfileSemantics, ProcFdPathAndSymlinkFollowingLink) {
    char dir[] = "/root/dunitest_proc_tmpfile_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir));
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0);
    int fd = openat(dirfd, ".", O_TMPFILE | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    char proc_path[64];
    snprintf(proc_path, sizeof(proc_path), "/proc/self/fd/%d", fd);
    char target[256] = {};
    ssize_t n = readlink(proc_path, target, sizeof(target) - 1);
    ASSERT_GT(n, 0) << strerror(errno);
    std::string rendered(target, n);
    EXPECT_NE(std::string::npos, rendered.find("/#"));
    EXPECT_NE(std::string::npos, rendered.find(" (deleted)"));
    ASSERT_EQ(0, linkat(AT_FDCWD, proc_path, dirfd, "via-proc", AT_SYMLINK_FOLLOW))
        << strerror(errno);
    struct stat st = {};
    EXPECT_EQ(0, stat((std::string(dir) + "/via-proc").c_str(), &st));
    EXPECT_EQ(1UL, st.st_nlink);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, unlinkat(dirfd, "via-proc", 0));
    EXPECT_EQ(0, close(dirfd));
    EXPECT_EQ(0, rmdir(dir));
}

TEST(TmpfileSemantics, ProcFdLinkRacingLastCloseNeverUsesReclaimedInode) {
    char dir[] = "/root/dunitest_proc_close_race_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(dir));
    int dirfd = open(dir, O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dirfd, 0);

    for (int i = 0; i < 16; ++i) {
        int fd = openat(dirfd, ".", O_TMPFILE | O_RDWR, 0600);
        ASSERT_GE(fd, 0) << strerror(errno);
        char proc_path[64];
        snprintf(proc_path, sizeof(proc_path), "/proc/self/fd/%d", fd);
        std::atomic<bool> begin{false};
        int result = -1;
        int link_error = 0;
        std::thread linker([&] {
            while (!begin.load(std::memory_order_acquire)) std::this_thread::yield();
            result = linkat(AT_FDCWD, proc_path, dirfd, "published", AT_SYMLINK_FOLLOW);
            link_error = errno;
        });
        begin.store(true, std::memory_order_release);
        EXPECT_EQ(0, close(fd));
        linker.join();
        if (result == 0) {
            struct stat st = {};
            EXPECT_EQ(0, fstatat(dirfd, "published", &st, 0));
            EXPECT_TRUE(S_ISREG(st.st_mode));
            EXPECT_EQ(1UL, st.st_nlink);
            EXPECT_EQ(0, unlinkat(dirfd, "published", 0));
        } else {
            // Closing before procfd resolution may remove the source link.
            EXPECT_EQ(ENOENT, link_error);
        }
    }
    EXPECT_EQ(0, close(dirfd));
    EXPECT_EQ(0, rmdir(dir));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
