#include <gtest/gtest.h>

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <fcntl.h>
#include <limits.h>
#include <string>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

const timespec kOldTimes[2] = {{1600000000, 0}, {1600000001, 0}};
const timespec kNewTimes[2] = {{1700000000, 0}, {1700000001, 0}};

class UtimensatSymlinkTest : public ::testing::TestWithParam<const char*> {
protected:
    void SetUp() override {
        char cwd[PATH_MAX];
        ASSERT_NE(nullptr, getcwd(cwd, sizeof(cwd)));
        const std::string base = std::strcmp(GetParam(), ".") == 0 ? cwd : GetParam();
        char path[PATH_MAX];
        ASSERT_LT(std::snprintf(path, sizeof(path), "%s/dunitest_utimens_XXXXXX", base.c_str()),
                  static_cast<int>(sizeof(path)));
        ASSERT_NE(nullptr, mkdtemp(path)) << std::strerror(errno);
        dir_ = path;
        ASSERT_EQ(0, mkdir(Path("real").c_str(), 0700));
        int fd = open(Path("real/file").c_str(), O_CREAT | O_EXCL | O_WRONLY, 0600);
        ASSERT_GE(fd, 0) << std::strerror(errno);
        EXPECT_EQ(7, write(fd, "payload", 7));
        EXPECT_EQ(0, close(fd));
        ASSERT_EQ(0, symlink("real", Path("alias").c_str()));
        ASSERT_EQ(0, utimensat(AT_FDCWD, Path("real/file").c_str(), kOldTimes, 0));
        dirfd_ = open(dir_.c_str(), O_RDONLY | O_DIRECTORY);
        ASSERT_GE(dirfd_, 0);
    }

    void TearDown() override {
        if (dirfd_ >= 0) close(dirfd_);
        if (dir_.empty()) return;
        for (const char* name : {"real/link", "real/file", "alias", "absolute", "loop"}) {
            unlink(Path(name).c_str());
        }
        rmdir(Path("real").c_str());
        rmdir(dir_.c_str());
    }

    std::string Path(const char* name) const { return dir_ + "/" + name; }

    void ExpectTimes(const char* name, const timespec (&times)[2]) {
        struct stat st = {};
        ASSERT_EQ(0, lstat(Path(name).c_str(), &st)) << std::strerror(errno);
        EXPECT_EQ(times[0].tv_sec, st.st_atim.tv_sec);
        EXPECT_EQ(times[1].tv_sec, st.st_mtim.tv_sec);
    }

    void MakeFinalLink(const char* target) {
        ASSERT_EQ(0, symlink(target, Path("real/link").c_str()));
        ASSERT_EQ(0, utimensat(AT_FDCWD, Path("real/link").c_str(), kOldTimes,
                              AT_SYMLINK_NOFOLLOW));
    }

    std::string dir_;
    int dirfd_ = -1;
};

TEST_P(UtimensatSymlinkTest, FollowsRelativeIntermediateLink) {
    ASSERT_EQ(0, utimensat(AT_FDCWD, Path("alias/file").c_str(), kNewTimes,
                          AT_SYMLINK_NOFOLLOW)) << std::strerror(errno);
    ExpectTimes("real/file", kNewTimes);
}

TEST_P(UtimensatSymlinkTest, PlainPathsKeepNoFollowSemantics) {
    ASSERT_EQ(0, utimensat(dirfd_, "real/file", kNewTimes, AT_SYMLINK_NOFOLLOW));
    ExpectTimes("real/file", kNewTimes);
    MakeFinalLink("file");
    ASSERT_EQ(0, utimensat(dirfd_, "real/link", kNewTimes, AT_SYMLINK_NOFOLLOW));
    ExpectTimes("real/link", kNewTimes);
    ASSERT_EQ(0, unlink(Path("real/link").c_str()));
    MakeFinalLink("missing");
    ASSERT_EQ(0, utimensat(dirfd_, "real/link", kNewTimes, AT_SYMLINK_NOFOLLOW));
    ExpectTimes("real/link", kNewTimes);
}

TEST_P(UtimensatSymlinkTest, FollowsAbsoluteIntermediateLink) {
    ASSERT_EQ(0, symlink(Path("real").c_str(), Path("absolute").c_str()));
    ASSERT_EQ(0, utimensat(AT_FDCWD, Path("absolute/file").c_str(), kNewTimes,
                          AT_SYMLINK_NOFOLLOW)) << std::strerror(errno);
    ExpectTimes("real/file", kNewTimes);
}

TEST_P(UtimensatSymlinkTest, ResolvesRelativePathFromDirfd) {
    ASSERT_EQ(0, utimensat(dirfd_, "alias/file", kNewTimes, AT_SYMLINK_NOFOLLOW))
        << std::strerror(errno);
    ExpectTimes("real/file", kNewTimes);
}

TEST_P(UtimensatSymlinkTest, UpdatesFinalLinkNotTarget) {
    MakeFinalLink("file");
    ASSERT_EQ(0, utimensat(AT_FDCWD, Path("alias/link").c_str(), kNewTimes,
                          AT_SYMLINK_NOFOLLOW)) << std::strerror(errno);
    ExpectTimes("real/link", kNewTimes);
    ExpectTimes("real/file", kOldTimes);
    struct stat st = {};
    ASSERT_EQ(0, lstat(Path("real/link").c_str(), &st));
    EXPECT_TRUE(S_ISLNK(st.st_mode));
    ASSERT_EQ(0, stat(Path("real/file").c_str(), &st));
    EXPECT_EQ(7, st.st_size);
}

TEST_P(UtimensatSymlinkTest, UpdatesDanglingFinalLinkWithoutCreatingTarget) {
    MakeFinalLink("missing");
    ASSERT_EQ(0, utimensat(dirfd_, "alias/link", kNewTimes, AT_SYMLINK_NOFOLLOW))
        << std::strerror(errno);
    ExpectTimes("real/link", kNewTimes);
    struct stat st = {};
    EXPECT_EQ(-1, lstat(Path("real/missing").c_str(), &st));
    EXPECT_EQ(ENOENT, errno);
}

TEST_P(UtimensatSymlinkTest, DefaultFlagsStillFollowFinalLink) {
    MakeFinalLink("file");
    ASSERT_EQ(0, utimensat(dirfd_, "alias/link", kNewTimes, 0));
    ExpectTimes("real/file", kNewTimes);
    // Resolving the symlink may update its atime, but must not update its mtime.
    struct stat st = {};
    ASSERT_EQ(0, lstat(Path("real/link").c_str(), &st));
    EXPECT_EQ(kOldTimes[1].tv_sec, st.st_mtim.tv_sec);
}

TEST_P(UtimensatSymlinkTest, TrailingSlashFollowsDirectoryLink) {
    ASSERT_EQ(0, utimensat(dirfd_, "alias", kOldTimes, AT_SYMLINK_NOFOLLOW));
    ASSERT_EQ(0, utimensat(dirfd_, "alias/", kNewTimes, AT_SYMLINK_NOFOLLOW))
        << std::strerror(errno);
    ExpectTimes("real", kNewTimes);
    struct stat st = {};
    ASSERT_EQ(0, lstat(Path("alias").c_str(), &st));
    EXPECT_TRUE(S_ISLNK(st.st_mode));
    EXPECT_EQ(kOldTimes[1].tv_sec, st.st_mtim.tv_sec);
}

TEST_P(UtimensatSymlinkTest, IntermediateLoopReturnsEloop) {
    ASSERT_EQ(0, symlink("loop", Path("loop").c_str()));
    errno = 0;
    EXPECT_EQ(-1, utimensat(dirfd_, "loop/file", kNewTimes, AT_SYMLINK_NOFOLLOW));
    EXPECT_EQ(ELOOP, errno);
}

TEST_P(UtimensatSymlinkTest, TrailingSlashRejectsNonDirectoryTargets) {
    MakeFinalLink("file");
    errno = 0;
    EXPECT_EQ(-1, utimensat(dirfd_, "alias/link/", kNewTimes, AT_SYMLINK_NOFOLLOW));
    EXPECT_EQ(ENOTDIR, errno);
    ASSERT_EQ(0, unlink(Path("real/link").c_str()));
    MakeFinalLink("missing");
    errno = 0;
    EXPECT_EQ(-1, utimensat(dirfd_, "alias/link/", kNewTimes, AT_SYMLINK_NOFOLLOW));
    EXPECT_EQ(ENOENT, errno);
}

TEST_P(UtimensatSymlinkTest, IntermediateDirectoryRequiresSearchPermission) {
    ASSERT_EQ(0, chmod(dir_.c_str(), 0755));
    ASSERT_EQ(0, chmod(Path("real").c_str(), 0000));
    pid_t child = fork();
    if (child == 0) {
        if (geteuid() == 0 && (setgid(65534) != 0 || setuid(65534) != 0)) _exit(2);
        errno = 0;
        int rc = utimensat(dirfd_, "alias/file", kNewTimes, AT_SYMLINK_NOFOLLOW);
        _exit(rc == -1 && errno == EACCES ? 0 : 1);
    }
    if (child > 0) {
        int status = 0;
        EXPECT_EQ(child, waitpid(child, &status, 0));
        EXPECT_TRUE(WIFEXITED(status));
        EXPECT_EQ(0, WEXITSTATUS(status));
    }
    // Restore access even if fork/wait failed so fixture cleanup remains safe.
    EXPECT_EQ(0, chmod(Path("real").c_str(), 0700));
    ASSERT_GE(child, 0) << std::strerror(errno);
    ExpectTimes("real/file", kOldTimes);
}

TEST_P(UtimensatSymlinkTest, InvalidIntermediateComponentsKeepTheirErrors) {
    errno = 0;
    EXPECT_EQ(-1, utimensat(dirfd_, "real/file/child", kNewTimes, AT_SYMLINK_NOFOLLOW));
    EXPECT_EQ(ENOTDIR, errno);
    errno = 0;
    EXPECT_EQ(-1, utimensat(dirfd_, "alias/missing/child", kNewTimes, AT_SYMLINK_NOFOLLOW));
    EXPECT_EQ(ENOENT, errno);
    ExpectTimes("real/file", kOldTimes);
}

TEST_P(UtimensatSymlinkTest, OmitPreservesUnselectedTimestamp) {
    const timespec times[2] = {{0, UTIME_OMIT}, kNewTimes[1]};
    ASSERT_EQ(0, utimensat(dirfd_, "alias/file", times, AT_SYMLINK_NOFOLLOW));
    const timespec expected[2] = {kOldTimes[0], kNewTimes[1]};
    ExpectTimes("real/file", expected);
    const timespec omit[2] = {{0, UTIME_OMIT}, {0, UTIME_OMIT}};
    EXPECT_EQ(0, utimensat(dirfd_, "missing/child", omit, AT_SYMLINK_NOFOLLOW));
}

TEST_P(UtimensatSymlinkTest, NullTimesUpdateFinalLinkOnly) {
    MakeFinalLink("file");
    timespec before = {}, after = {};
    ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &before));
    ASSERT_EQ(0, utimensat(dirfd_, "alias/link", nullptr, AT_SYMLINK_NOFOLLOW));
    ASSERT_EQ(0, clock_gettime(CLOCK_REALTIME, &after));
    struct stat st = {};
    ASSERT_EQ(0, lstat(Path("real/link").c_str(), &st));
    EXPECT_GE(st.st_mtim.tv_sec, before.tv_sec);
    EXPECT_LE(st.st_mtim.tv_sec, after.tv_sec);
    ExpectTimes("real/file", kOldTimes);
}

TEST_P(UtimensatSymlinkTest, FutimensFdPathRemainsUnchanged) {
    int fd = open(Path("real/file").c_str(), O_WRONLY);
    ASSERT_GE(fd, 0);
    EXPECT_EQ(0, futimens(fd, kNewTimes));
    EXPECT_EQ(0, close(fd));
    ExpectTimes("real/file", kNewTimes);
}

INSTANTIATE_TEST_SUITE_P(Filesystems, UtimensatSymlinkTest, ::testing::Values("/tmp", "."),
                        [](const ::testing::TestParamInfo<const char*>& info) {
                            return info.index == 0 ? "Tmp" : "Cwd";
                        });

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
