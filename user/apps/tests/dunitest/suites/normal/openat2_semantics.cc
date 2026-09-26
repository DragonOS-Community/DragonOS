#include <gtest/gtest.h>

#include <cerrno>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/mount.h>
#include <sys/vfs.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

struct OpenHow {
    uint64_t flags;
    uint64_t mode;
    uint64_t resolve;
};

constexpr long kOpenat2 = 437;
constexpr uint64_t kNoXdev = 0x01;
constexpr uint64_t kNoMagicLinks = 0x02;
constexpr uint64_t kNoSymlinks = 0x04;
constexpr uint64_t kBeneath = 0x08;
constexpr uint64_t kInRoot = 0x10;
constexpr uint64_t kCached = 0x20;

int OpenAt2(int dirfd, const char* path, const OpenHow& how,
            size_t size = sizeof(OpenHow)) {
    return static_cast<int>(syscall(kOpenat2, dirfd, path, &how, size));
}

void ExpectFailure(int dirfd, const char* path, const OpenHow& how, int error) {
    errno = 0;
    EXPECT_EQ(-1, OpenAt2(dirfd, path, how));
    EXPECT_EQ(error, errno) << path;
}

class OpenAt2Test : public ::testing::Test {
protected:
    void SetUp() override {
        char name[] = "/tmp/dunitest_openat2_XXXXXX";
        char* created = mkdtemp(name);
        ASSERT_NE(nullptr, created) << strerror(errno);
        dir_ = created;
        const std::string inside = dir_ + "/inside";
        int fd = open(inside.c_str(), O_CREAT | O_WRONLY, 0600);
        ASSERT_GE(fd, 0) << strerror(errno);
        ASSERT_EQ(0, close(fd));
        ASSERT_EQ(0, symlink("inside", (dir_ + "/relative").c_str()));
        ASSERT_EQ(0, symlink("/etc/passwd", (dir_ + "/absolute").c_str()));
        dirfd_ = open(dir_.c_str(), O_RDONLY | O_DIRECTORY);
        ASSERT_GE(dirfd_, 0) << strerror(errno);
    }

    void TearDown() override {
        if (dirfd_ >= 0) close(dirfd_);
        unlink((dir_ + "/relative").c_str());
        unlink((dir_ + "/absolute").c_str());
        unlink((dir_ + "/inside").c_str());
        rmdir(dir_.c_str());
    }

    std::string dir_;
    int dirfd_ = -1;
};

TEST_F(OpenAt2Test, ValidRequestAndStructExtension) {
    OpenHow how = {};
    int fd = OpenAt2(dirfd_, "inside", how);
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));

    errno = 0;
    EXPECT_EQ(-1, OpenAt2(dirfd_, "inside", how, sizeof(how) - 1));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, OpenAt2(dirfd_, "inside", how, 4097));
    EXPECT_EQ(E2BIG, errno);

    struct Extended {
        OpenHow how;
        uint64_t extra;
    } extended = {};
    fd = static_cast<int>(syscall(kOpenat2, dirfd_, "inside", &extended, sizeof(extended)));
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    extended.extra = 1;
    errno = 0;
    EXPECT_EQ(-1, syscall(kOpenat2, dirfd_, "inside", &extended, sizeof(extended)));
    EXPECT_EQ(E2BIG, errno);
    errno = 0;
    EXPECT_EQ(-1, syscall(kOpenat2, dirfd_, "inside", reinterpret_cast<void*>(1),
                          sizeof(how)));
    EXPECT_EQ(EFAULT, errno);
}

TEST_F(OpenAt2Test, StrictFlagValidation) {
    ExpectFailure(dirfd_, "inside", OpenHow{1ULL << 63, 0, 0}, EINVAL);
    ExpectFailure(dirfd_, "inside", OpenHow{0, 1, 0}, EINVAL);
    ExpectFailure(dirfd_, "inside", OpenHow{0, 0, 1ULL << 63}, EINVAL);
    ExpectFailure(dirfd_, "inside", OpenHow{0, 0, kBeneath | kInRoot}, EINVAL);
    ExpectFailure(dirfd_, "inside", OpenHow{O_PATH | O_RDWR, 0, 0}, EINVAL);
    ExpectFailure(dirfd_, "inside", OpenHow{O_CREAT | O_DIRECTORY, 0600, 0}, EINVAL);
    ExpectFailure(dirfd_, "inside", OpenHow{O_TMPFILE, 0600, 0}, EINVAL);

    // Linux strips this internal fanotify bit before validating open flags.
    int fd = OpenAt2(dirfd_, "inside", OpenHow{0x04000000, 0, 0});
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
}

TEST_F(OpenAt2Test, BeneathAndInRoot) {
    OpenHow beneath = {0, 0, kBeneath};
    int fd = OpenAt2(dirfd_, "relative", beneath);
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    ExpectFailure(dirfd_, "../inside", beneath, EXDEV);
    ExpectFailure(dirfd_, "/etc/passwd", beneath, EXDEV);
    ExpectFailure(dirfd_, "absolute", beneath, EXDEV);

    OpenHow in_root = {0, 0, kInRoot};
    fd = OpenAt2(dirfd_, "/inside", in_root);
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    fd = OpenAt2(dirfd_, "../inside", in_root);
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    ExpectFailure(-9, "/inside", in_root, EBADF);
}

TEST_F(OpenAt2Test, SymbolicAndMagicLinks) {
    ExpectFailure(dirfd_, "relative", OpenHow{0, 0, kNoSymlinks}, ELOOP);
    int fd = OpenAt2(dirfd_, "relative", OpenHow{O_PATH | O_NOFOLLOW, 0, kNoSymlinks});
    ASSERT_GE(fd, 0) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st));
    EXPECT_TRUE(S_ISLNK(st.st_mode));
    EXPECT_EQ(0, close(fd));

    ExpectFailure(AT_FDCWD, "/proc/self/exe", OpenHow{0, 0, kNoMagicLinks}, ELOOP);
}

TEST_F(OpenAt2Test, MountAndCacheBoundaries) {
    int fd = OpenAt2(dirfd_, "inside", OpenHow{0, 0, kNoXdev});
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    fd = OpenAt2(dirfd_, ".", OpenHow{O_PATH, 0, kNoXdev});
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    ExpectFailure(AT_FDCWD, "/proc/self", OpenHow{O_PATH, 0, kNoXdev}, EXDEV);

    errno = 0;
    fd = OpenAt2(dirfd_, "inside", OpenHow{0, 0, kCached});
    if (fd >= 0) {
        EXPECT_EQ(0, close(fd));
    } else {
        EXPECT_EQ(EAGAIN, errno);
    }
    ExpectFailure(dirfd_, "inside", OpenHow{O_CREAT | O_RDWR, 0600, kCached}, EAGAIN);
}

TEST_F(OpenAt2Test, CachedLookupOnTmpfs) {
    // A tmpfs directory and its entries are authoritative in memory. The
    // test does not assume that an ext4 inode cache can never be reclaimed.
    struct statfs fs = {};
    constexpr long kTmpfsMagic = 0x01021994;
    if (statfs("/dev/shm", &fs) != 0 || fs.f_type != kTmpfsMagic) {
        GTEST_SKIP() << "/dev/shm is not tmpfs";
    }
    char path[] = "/dev/shm/dunitest_cached_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(path)) << strerror(errno);
    int parent = open(path, O_PATH | O_DIRECTORY);
    ASSERT_GE(parent, 0) << strerror(errno);

    auto cached = OpenHow{O_PATH, 0, kCached};
    int fd = OpenAt2(parent, ".", cached);
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));

    int created = openat(parent, "first", O_CREAT | O_WRONLY, 0600);
    ASSERT_GE(created, 0) << strerror(errno);
    EXPECT_EQ(0, close(created));
    fd = OpenAt2(parent, "first", cached);
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    fd = OpenAt2(parent, "first", OpenHow{O_PATH, 0, kCached | kNoXdev});
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    // Parent traversal may need a non-cached mount/namespace walk. A cached
    // implementation may either prove it safe or require a regular retry.
    errno = 0;
    fd = OpenAt2(parent, "../", cached);
    if (fd >= 0) {
        EXPECT_EQ(0, close(fd));
    } else {
        EXPECT_EQ(EAGAIN, errno);
    }

    ASSERT_EQ(0, symlinkat("first", parent, "link"));
    fd = OpenAt2(parent, "link", OpenHow{O_PATH, 0, kCached | kBeneath});
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));

    // A cached pathname must never authorize an open with stale mode bits.
    ASSERT_EQ(0, fchmodat(parent, "first", 0000, 0));
    ASSERT_EQ(0, chmod(path, 0755));
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) _exit(2);
        errno = 0;
        int denied = OpenAt2(parent, "first", OpenHow{O_RDONLY, 0, kCached});
        int saved_errno = errno;
        if (denied >= 0) close(denied);
        _exit(denied == -1 && (saved_errno == EACCES || saved_errno == EAGAIN) ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));

    // No negative dentry was populated for this name; retry without CACHED.
    ExpectFailure(parent, "missing", cached, EAGAIN);
    ASSERT_EQ(0, unlinkat(parent, "first", 0));
    ExpectFailure(parent, "first", cached, EAGAIN);

    EXPECT_EQ(0, unlinkat(parent, "link", 0));
    EXPECT_EQ(0, close(parent));
    EXPECT_EQ(0, rmdir(path));
}

TEST_F(OpenAt2Test, CachedExt4NeverReturnsReplacedInode) {
    struct statfs fs = {};
    constexpr long kExt4Magic = 0xEF53;
    if (statfs("/root", &fs) != 0 || fs.f_type != kExt4Magic) {
        GTEST_SKIP() << "/root is not ext4";
    }
    char path[] = "/root/dunitest_cached_ext4_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(path)) << strerror(errno);
    int parent = open(path, O_PATH | O_DIRECTORY);
    ASSERT_GE(parent, 0) << strerror(errno);
    int first = openat(parent, "name", O_CREAT | O_WRONLY, 0600);
    ASSERT_GE(first, 0) << strerror(errno);
    struct stat before = {};
    ASSERT_EQ(0, fstat(first, &before));
    ASSERT_EQ(0, close(first));

    auto cached = OpenHow{O_PATH, 0, kCached};
    errno = 0;
    int fd = OpenAt2(parent, "name", cached);
    if (fd >= 0) {
        struct stat observed = {};
        ASSERT_EQ(0, fstat(fd, &observed));
        EXPECT_EQ(before.st_ino, observed.st_ino);
        EXPECT_EQ(0, close(fd));
    } else {
        EXPECT_EQ(EAGAIN, errno);
    }

    int replacement = openat(parent, "replacement", O_CREAT | O_WRONLY, 0600);
    ASSERT_GE(replacement, 0) << strerror(errno);
    struct stat after = {};
    ASSERT_EQ(0, fstat(replacement, &after));
    ASSERT_NE(before.st_ino, after.st_ino);
    ASSERT_EQ(0, close(replacement));
    ASSERT_EQ(0, renameat(parent, "replacement", parent, "name"));
    errno = 0;
    fd = OpenAt2(parent, "name", cached);
    if (fd >= 0) {
        struct stat observed = {};
        ASSERT_EQ(0, fstat(fd, &observed));
        EXPECT_EQ(after.st_ino, observed.st_ino);
        EXPECT_EQ(0, close(fd));
    } else {
        EXPECT_EQ(EAGAIN, errno);
    }

    EXPECT_EQ(0, unlinkat(parent, "name", 0));
    EXPECT_EQ(0, close(parent));
    EXPECT_EQ(0, rmdir(path));
}

TEST_F(OpenAt2Test, CachedCannotCreateOrTruncate) {
    const std::string inside = dir_ + "/inside";
    int writer = open(inside.c_str(), O_WRONLY);
    ASSERT_GE(writer, 0) << strerror(errno);
    ASSERT_EQ(3, write(writer, "old", 3));
    ASSERT_EQ(0, close(writer));

    ExpectFailure(dirfd_, "new", OpenHow{O_CREAT | O_RDWR, 0600, kCached}, EAGAIN);
    struct stat st = {};
    EXPECT_EQ(-1, fstatat(dirfd_, "new", &st, 0));
    EXPECT_EQ(ENOENT, errno);
    ExpectFailure(dirfd_, "inside", OpenHow{O_WRONLY | O_TRUNC, 0, kCached}, EAGAIN);
    ASSERT_EQ(0, fstatat(dirfd_, "inside", &st, 0));
    EXPECT_EQ(3, st.st_size);
    ExpectFailure(dirfd_, ".", OpenHow{O_TMPFILE | O_RDWR, 0600, kCached}, EAGAIN);
}

TEST_F(OpenAt2Test, NoXdevAbsoluteLinkAndPinnedDirectory) {
    const std::string source = dir_ + "/source";
    const std::string target = dir_ + "/target";
    ASSERT_EQ(0, mkdir(source.c_str(), 0700));
    ASSERT_EQ(0, mkdir(target.c_str(), 0700));
    ASSERT_EQ(0, symlink("/etc/passwd", (source + "/jump").c_str()));
    int pinned_fd = open(target.c_str(), O_RDONLY | O_DIRECTORY);
    ASSERT_GE(pinned_fd, 0);

    if (mount(source.c_str(), target.c_str(), nullptr, MS_BIND, nullptr) != 0) {
        // An unprivileged Linux host cannot create a test mount. The DragonOS
        // guest runs this same case with mount privileges.
        EXPECT_EQ(EPERM, errno);
    } else {
        struct stat pinned_st = {};
        ASSERT_EQ(0, fstat(pinned_fd, &pinned_st));
        int fd = OpenAt2(pinned_fd, ".", OpenHow{O_PATH, 0, kNoXdev});
        EXPECT_GE(fd, 0) << strerror(errno);
        if (fd >= 0) {
            struct stat opened_st = {};
            EXPECT_EQ(0, fstat(fd, &opened_st));
            EXPECT_EQ(pinned_st.st_dev, opened_st.st_dev);
            EXPECT_EQ(pinned_st.st_ino, opened_st.st_ino);
            EXPECT_EQ(0, close(fd));
        }
        errno = 0;
        fd = OpenAt2(pinned_fd, ".", OpenHow{O_PATH, 0, kNoXdev | kCached});
        if (fd >= 0) {
            struct stat opened_st = {};
            EXPECT_EQ(0, fstat(fd, &opened_st));
            EXPECT_EQ(pinned_st.st_dev, opened_st.st_dev);
            EXPECT_EQ(pinned_st.st_ino, opened_st.st_ino);
            EXPECT_EQ(0, close(fd));
        } else {
            EXPECT_EQ(EAGAIN, errno);
        }
        int mounted_fd = open(target.c_str(), O_RDONLY | O_DIRECTORY);
        EXPECT_GE(mounted_fd, 0);
        if (mounted_fd >= 0) {
            ExpectFailure(mounted_fd, "jump", OpenHow{O_PATH, 0, kNoXdev}, EXDEV);
            EXPECT_EQ(0, close(mounted_fd));
        }
        EXPECT_EQ(0, umount2(target.c_str(), 0));
    }
    EXPECT_EQ(0, close(pinned_fd));
    EXPECT_EQ(0, unlink((source + "/jump").c_str()));
    EXPECT_EQ(0, rmdir(source.c_str()));
    EXPECT_EQ(0, rmdir(target.c_str()));
}

TEST_F(OpenAt2Test, ScopedCreateAndLiteralInternalWhitespace) {
    const std::string spaced = dir_ + "/name with space";
    int fd = OpenAt2(dirfd_, "name with space",
                     OpenHow{O_CREAT | O_EXCL | O_RDWR, 0600, kBeneath});
    ASSERT_GE(fd, 0) << strerror(errno);
    EXPECT_EQ(0, close(fd));
    struct stat st = {};
    EXPECT_EQ(0, stat(spaced.c_str(), &st));
    ExpectFailure(dirfd_, "name with space",
                  OpenHow{O_CREAT | O_EXCL | O_RDWR, 0600, kBeneath}, EEXIST);
    ExpectFailure(dirfd_, "../outside", OpenHow{O_CREAT | O_RDWR, 0600, kBeneath},
                  EXDEV);
    EXPECT_EQ(0, unlink(spaced.c_str()));
}

TEST_F(OpenAt2Test, TmpfileRequiresDirectoryAndCreatesUnnamedFile) {
    errno = 0;
    int fd = OpenAt2(dirfd_, "inside", OpenHow{O_TMPFILE | O_RDWR, 0600, 0});
    EXPECT_EQ(-1, fd);
    EXPECT_EQ(ENOTDIR, errno);

    errno = 0;
    fd = OpenAt2(dirfd_, ".", OpenHow{O_TMPFILE | O_RDWR, 0600, 0});
    ASSERT_GE(fd, 0) << strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st));
    EXPECT_TRUE(S_ISREG(st.st_mode));
    EXPECT_EQ(0UL, st.st_nlink);
    EXPECT_EQ(0, close(fd));
}

TEST_F(OpenAt2Test, AccessModeThreeStillChecksReadAndWritePermission) {
    const std::string inside = dir_ + "/inside";
    ASSERT_EQ(0, chmod(dir_.c_str(), 0755));
    ASSERT_EQ(0, chmod(inside.c_str(), 0000));
    pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(1000) != 0 || setuid(1000) != 0) _exit(2);
        errno = 0;
        int fd = OpenAt2(dirfd_, "inside", OpenHow{3, 0, 0});
        int saved_errno = errno;
        if (fd >= 0) close(fd);
        _exit(fd == -1 && saved_errno == EACCES ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(0, chmod(inside.c_str(), 0600));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
