#include <gtest/gtest.h>

#include "loop_ext4_test_support.h"

#include <cerrno>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <ctime>
#include <fcntl.h>
#include <limits.h>
#include <string>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <utime.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

const timespec kOldTimes[2] = {{1600000000, 0}, {1600000001, 0}};
const timespec kNewTimes[2] = {{1700000000, 0}, {1700000001, 0}};

class UtimensatSymlinkTest : public ::testing::TestWithParam<const char*> {
protected:
    void SetUp() override {
        std::string base = GetParam();
        if (base == "ext4") {
            ASSERT_NO_FATAL_FAILURE(ext4_.SetUp());
            ASSERT_NO_FATAL_FAILURE(ext4_.Mount());
            base = ext4_.mount_point();
            // The permission test drops privileges: only its real directory,
            // not the fixture's mount point, should deny traversal.
            ASSERT_EQ(0, chmod(base.c_str(), 0755)) << std::strerror(errno);
        }
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
        ASSERT_EQ(0, symlink("real", Path("alias").c_str())) << std::strerror(errno);
        ASSERT_EQ(0, utimensat(AT_FDCWD, Path("real/file").c_str(), kOldTimes, 0));
        dirfd_ = open(dir_.c_str(), O_RDONLY | O_DIRECTORY);
        ASSERT_GE(dirfd_, 0);
    }

    void TearDown() override {
        if (saved_cwd_ >= 0) {
            EXPECT_EQ(0, fchdir(saved_cwd_));
            EXPECT_EQ(0, close(saved_cwd_));
        }
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

    void EnterTestDirectory() {
        saved_cwd_ = open(".", O_RDONLY | O_DIRECTORY);
        ASSERT_GE(saved_cwd_, 0);
        ASSERT_EQ(0, fchdir(dirfd_));
        ASSERT_EQ(0, utimensat(AT_FDCWD, ".", kOldTimes, 0));
    }

    static void ExpectSameTimestamps(const char* path, const struct stat& before) {
        SCOPED_TRACE(path);
        struct stat after = {};
        ASSERT_EQ(0, stat(path, &after));
        EXPECT_EQ(before.st_atim.tv_sec, after.st_atim.tv_sec);
        EXPECT_EQ(before.st_atim.tv_nsec, after.st_atim.tv_nsec);
        EXPECT_EQ(before.st_mtim.tv_sec, after.st_mtim.tv_sec);
        EXPECT_EQ(before.st_mtim.tv_nsec, after.st_mtim.tv_nsec);
        EXPECT_EQ(before.st_ctim.tv_sec, after.st_ctim.tv_sec);
        EXPECT_EQ(before.st_ctim.tv_nsec, after.st_ctim.tv_nsec);
    }

    void MakeFinalLink(const char* target) {
        ASSERT_EQ(0, symlink(target, Path("real/link").c_str()));
        ASSERT_EQ(0, utimensat(AT_FDCWD, Path("real/link").c_str(), kOldTimes,
                              AT_SYMLINK_NOFOLLOW));
    }

    std::string dir_;
    int dirfd_ = -1;
    int saved_cwd_ = -1;
    dunitest::LoopExt4 ext4_;
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

TEST_P(UtimensatSymlinkTest, EmptyPathDoesNotMutateCwdOrFd) {
    ASSERT_NO_FATAL_FAILURE(EnterTestDirectory());
    struct stat cwd = {}, file = {};
    ASSERT_EQ(0, stat(".", &cwd));
    ASSERT_EQ(0, stat("real/file", &file));
    const int fd = open("real/file", O_RDONLY);
    ASSERT_GE(fd, 0);

    for (const int dirfd : {AT_FDCWD, dirfd_, fd, -1}) {
        for (const int flags : {0, AT_SYMLINK_NOFOLLOW}) {
            SCOPED_TRACE(::testing::Message() << "dirfd=" << dirfd << ", flags=" << flags);
            EXPECT_EQ(-1, syscall(SYS_utimensat, dirfd, "", kNewTimes, flags));
            EXPECT_EQ(ENOENT, errno);
            ExpectSameTimestamps(".", cwd);
            ExpectSameTimestamps("real/file", file);
        }
    }
    EXPECT_EQ(-1, syscall(SYS_utimensat, fd, nullptr, kNewTimes, AT_SYMLINK_NOFOLLOW));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, close(fd));

    EXPECT_EQ(-1, syscall(SYS_utimensat, AT_FDCWD, "", nullptr, 0));
    EXPECT_EQ(ENOENT, errno);
    const timespec invalid[2] = {{0, 1000000000}, kNewTimes[1]};
    EXPECT_EQ(-1, syscall(SYS_utimensat, AT_FDCWD, "", invalid, 0));
    EXPECT_EQ(ENOENT, errno);
    EXPECT_EQ(-1, syscall(SYS_utimensat, dirfd_, "real/file", invalid, 0));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, syscall(SYS_utimensat, AT_FDCWD, "", kNewTimes, -1));
    EXPECT_EQ(EINVAL, errno);

    const timespec omitted[2] = {{0, UTIME_OMIT}, {0, UTIME_OMIT}};
    EXPECT_EQ(0, syscall(SYS_utimensat, -1, "", omitted, 0));
    EXPECT_EQ(0, syscall(SYS_utimensat, -1, "", omitted, -1));
    ExpectSameTimestamps(".", cwd);
    ExpectSameTimestamps("real/file", file);
}

#if defined(SYS_utime) && defined(SYS_utimes) && defined(SYS_futimesat)
TEST_P(UtimensatSymlinkTest, LegacyEmptyPathPreservesTimevalErrorPrecedence) {
    ASSERT_NO_FATAL_FAILURE(EnterTestDirectory());
    struct stat cwd = {};
    ASSERT_EQ(0, stat(".", &cwd));
    const utimbuf seconds = {kNewTimes[0].tv_sec, kNewTimes[1].tv_sec};
    EXPECT_EQ(-1, syscall(SYS_utime, "", &seconds));
    EXPECT_EQ(ENOENT, errno);
    ExpectSameTimestamps(".", cwd);

    const timeval valid[2] = {{kNewTimes[0].tv_sec, 123}, {kNewTimes[1].tv_sec, 999999}};
    const timeval invalid[2] = {{0, 1000000}, {0, 0}};
    EXPECT_EQ(-1, syscall(SYS_utimes, "", invalid));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, syscall(SYS_utimes, "", valid));
    EXPECT_EQ(ENOENT, errno);
    ExpectSameTimestamps(".", cwd);

    EXPECT_EQ(-1, syscall(SYS_futimesat, dirfd_, "", invalid));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(-1, syscall(SYS_futimesat, dirfd_, "", valid));
    EXPECT_EQ(ENOENT, errno);
    ExpectSameTimestamps(".", cwd);

    ASSERT_EQ(0, syscall(SYS_futimesat, dirfd_, "real/file", valid));
    struct stat file = {};
    ASSERT_EQ(0, stat("real/file", &file));
    EXPECT_EQ(valid[0].tv_sec, file.st_atim.tv_sec);
    EXPECT_EQ(valid[1].tv_sec, file.st_mtim.tv_sec);

    const timespec converted[2] = {{valid[0].tv_sec, 123000}, {valid[1].tv_sec, 999999000}};
    // Compare the two syscalls on the same filesystem, at its timestamp precision.
    const timespec reset[2] = {kOldTimes[0], kOldTimes[1]};
    ASSERT_EQ(0, utimensat(dirfd_, "real/file", reset, 0));
    ASSERT_EQ(0, utimensat(dirfd_, "real/file", converted, 0));
    struct stat reference = {};
    ASSERT_EQ(0, stat("real/file", &reference));
    EXPECT_EQ(reference.st_atim.tv_nsec, file.st_atim.tv_nsec);
    EXPECT_EQ(reference.st_mtim.tv_nsec, file.st_mtim.tv_nsec);
    if (std::strcmp(GetParam(), "/tmp") == 0) {
        EXPECT_EQ(converted[0].tv_nsec, file.st_atim.tv_nsec);
        EXPECT_EQ(converted[1].tv_nsec, file.st_mtim.tv_nsec);
    }
}
#endif

INSTANTIATE_TEST_SUITE_P(Filesystems, UtimensatSymlinkTest, ::testing::Values("/tmp", "ext4"),
                        [](const ::testing::TestParamInfo<const char*>& info) {
                            return info.index == 0 ? "Tmp" : "Ext4";
                        });

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
