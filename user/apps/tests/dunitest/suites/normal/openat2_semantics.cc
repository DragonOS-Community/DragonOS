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
        int fd = OpenAt2(pinned_fd, ".", OpenHow{O_PATH, 0, kNoXdev});
        EXPECT_GE(fd, 0) << strerror(errno);
        if (fd >= 0) {
            EXPECT_EQ(0, close(fd));
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
