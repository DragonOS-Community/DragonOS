#include <gtest/gtest.h>

#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <sched.h>
#include <string>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

namespace {

constexpr long kFchmodat2 = 452;

int Fchmodat2(int dirfd, const char* path, mode_t mode, int flags) {
    return static_cast<int>(syscall(kFchmodat2, dirfd, path, mode, flags));
}

class Fchmodat2Test : public ::testing::Test {
protected:
    void SetUp() override {
        char pattern[] = "/tmp/fchmodat2-XXXXXX";
        char* dir = mkdtemp(pattern);
        ASSERT_NE(nullptr, dir) << std::strerror(errno);
        dir_ = dir;
        file_ = dir_ + "/file";
        link_ = dir_ + "/link";
        fifo_ = dir_ + "/fifo";
        int fd = open(file_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0644);
        ASSERT_GE(fd, 0) << std::strerror(errno);
        ASSERT_EQ(0, close(fd));
        ASSERT_EQ(0, symlink("file", link_.c_str())) << std::strerror(errno);
        dirfd_ = open(dir_.c_str(), O_PATH | O_DIRECTORY);
        ASSERT_GE(dirfd_, 0) << std::strerror(errno);
    }

    void TearDown() override {
        if (dirfd_ >= 0) close(dirfd_);
        if (!fifo_.empty()) unlink(fifo_.c_str());
        if (!link_.empty()) unlink(link_.c_str());
        if (!file_.empty()) unlink(file_.c_str());
        if (!dir_.empty()) rmdir(dir_.c_str());
    }

    mode_t Mode(const std::string& path, bool follow = true) {
        struct stat st = {};
        EXPECT_EQ(0, follow ? stat(path.c_str(), &st) : lstat(path.c_str(), &st));
        return st.st_mode;
    }

    std::string dir_;
    std::string file_;
    std::string link_;
    std::string fifo_;
    int dirfd_ = -1;
};

TEST_F(Fchmodat2Test, PathAndDirfdPreserveTypeAndTruncateMode) {
    ASSERT_EQ(0, Fchmodat2(dirfd_, "file", 0600, 0)) << std::strerror(errno);
    EXPECT_EQ(static_cast<mode_t>(0600), Mode(file_) & 0777);
    EXPECT_TRUE(S_ISREG(Mode(file_)));

    // The kernel argument is umode_t; bits above 16 are truncated, and the
    // file type bits within 16 are not used as replacement permission bits.
    ASSERT_EQ(0, Fchmodat2(-1, file_.c_str(), 0100000 | 0640 | (1UL << 20), 0))
        << std::strerror(errno);
    EXPECT_EQ(static_cast<mode_t>(0640), Mode(file_) & 0777);
    EXPECT_TRUE(S_ISREG(Mode(file_)));
    ASSERT_EQ(0, Fchmodat2(dirfd_, "file", 0600, AT_SYMLINK_NOFOLLOW));
    EXPECT_EQ(static_cast<mode_t>(0600), Mode(file_) & 0777);
}

TEST_F(Fchmodat2Test, EmptyPathTargetsFdNotCwdAndAllowsOPath) {
    int fd = open(file_.c_str(), O_PATH);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    ASSERT_EQ(0, Fchmodat2(fd, "", 0620, AT_EMPTY_PATH)) << std::strerror(errno);
    EXPECT_EQ(static_cast<mode_t>(0620), Mode(file_) & 0777);

    errno = 0;
    EXPECT_EQ(-1, fchmod(fd, 0600));
    EXPECT_EQ(EBADF, errno);
    EXPECT_EQ(0, close(fd));

    const mode_t cwd_mode = Mode(".") & 07777;
    ASSERT_EQ(0, Fchmodat2(AT_FDCWD, "", cwd_mode, AT_EMPTY_PATH)) << std::strerror(errno);
    EXPECT_EQ(cwd_mode, Mode(".") & 07777);
    EXPECT_EQ(static_cast<mode_t>(0620), Mode(file_) & 0777);
}

TEST_F(Fchmodat2Test, NoFollowRejectsSymlinkWithoutChangingTarget) {
    ASSERT_EQ(0, Fchmodat2(dirfd_, "link", 0640, 0)) << std::strerror(errno);
    EXPECT_EQ(static_cast<mode_t>(0640), Mode(file_) & 0777);

    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(dirfd_, "link", 0600, AT_SYMLINK_NOFOLLOW));
    EXPECT_EQ(EOPNOTSUPP, errno);
    EXPECT_EQ(static_cast<mode_t>(0640), Mode(file_) & 0777);
    EXPECT_EQ(static_cast<mode_t>(0777), Mode(link_, false) & 0777);

    int fd = open(link_.c_str(), O_PATH | O_NOFOLLOW);
    ASSERT_GE(fd, 0);
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(fd, "", 0600, AT_EMPTY_PATH));
    EXPECT_EQ(EOPNOTSUPP, errno);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(static_cast<mode_t>(0640), Mode(file_) & 0777);
}

TEST_F(Fchmodat2Test, EmptyPathOnOpenFifoModifiesNamedInode) {
    ASSERT_EQ(0, mkfifo(fifo_.c_str(), 0600)) << std::strerror(errno);
    int fd = open(fifo_.c_str(), O_RDWR | O_NONBLOCK);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    ASSERT_EQ(0, Fchmodat2(fd, "", 0640, AT_EMPTY_PATH)) << std::strerror(errno);
    EXPECT_EQ(static_cast<mode_t>(0640), Mode(fifo_) & 0777);
    EXPECT_TRUE(S_ISFIFO(Mode(fifo_)));
    EXPECT_EQ(0, close(fd));

    fd = open(fifo_.c_str(), O_PATH);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(0, Fchmodat2(fd, "", 0620, AT_EMPTY_PATH)) << std::strerror(errno);
    EXPECT_EQ(static_cast<mode_t>(0620), Mode(fifo_) & 0777);
    EXPECT_EQ(0, close(fd));
}

TEST_F(Fchmodat2Test, EmptyPathKeepsUnlinkedFifoPathInodeAlive) {
    ASSERT_EQ(0, mkfifo(fifo_.c_str(), 0600)) << std::strerror(errno);
    const int fd = open(fifo_.c_str(), O_RDWR | O_NONBLOCK);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    ASSERT_EQ(0, unlink(fifo_.c_str()));

    ASSERT_EQ(0, Fchmodat2(fd, "", 0640, AT_EMPTY_PATH)) << std::strerror(errno);
    struct stat st = {};
    ASSERT_EQ(0, fstat(fd, &st)) << std::strerror(errno);
    EXPECT_TRUE(S_ISFIFO(st.st_mode));
    EXPECT_EQ(static_cast<mode_t>(0640), st.st_mode & 0777);
    EXPECT_EQ(0, close(fd));
}

TEST_F(Fchmodat2Test, ErrorsAndLegacyEntryPoints) {
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(-1, "", 0600, 0));
    EXPECT_EQ(ENOENT, errno);
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(-1, "", 0600, AT_EMPTY_PATH));
    EXPECT_EQ(EBADF, errno);
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(dirfd_, "file", 0600, 0x80000000U));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(dirfd_, reinterpret_cast<const char*>(1), 0600,
                            0x80000000U));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(dirfd_, reinterpret_cast<const char*>(1), 0600, 0));
    EXPECT_EQ(EFAULT, errno);
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(dirfd_, "file", 0600, AT_EMPTY_PATH | AT_SYMLINK_NOFOLLOW | 0x4000));
    EXPECT_EQ(EINVAL, errno);

    ASSERT_EQ(0, chmod(file_.c_str(), 0600)) << std::strerror(errno);
    ASSERT_EQ(0, fchmodat(dirfd_, "file", 0640, 0)) << std::strerror(errno);
    int fd = open(file_.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(0, fchmod(fd, 0600)) << std::strerror(errno);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(static_cast<mode_t>(0600), Mode(file_) & 0777);
}

TEST_F(Fchmodat2Test, ReadonlyMountRejectsPathAndEmptyPath) {
    if (unshare(CLONE_NEWNS) != 0) GTEST_SKIP() << std::strerror(errno);
    ASSERT_EQ(0, mount(nullptr, "/", nullptr, MS_REC | MS_PRIVATE, nullptr))
        << std::strerror(errno);

    const std::string mountpoint = dir_ + "/mnt";
    ASSERT_EQ(0, mkdir(mountpoint.c_str(), 0700));
    ASSERT_EQ(0, mount("tmpfs", mountpoint.c_str(), "tmpfs", 0, nullptr))
        << std::strerror(errno);
    const std::string mounted_file = mountpoint + "/file";
    int fd = open(mounted_file.c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << std::strerror(errno);
    ASSERT_EQ(0, close(fd));
    fd = open(mounted_file.c_str(), O_PATH);
    ASSERT_GE(fd, 0);

    ASSERT_EQ(0, mount("tmpfs", mountpoint.c_str(), "tmpfs", MS_REMOUNT | MS_RDONLY, nullptr))
        << std::strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(-1, mounted_file.c_str(), 0644, 0));
    EXPECT_EQ(EROFS, errno);
    errno = 0;
    EXPECT_EQ(-1, Fchmodat2(fd, "", 0644, AT_EMPTY_PATH));
    EXPECT_EQ(EROFS, errno);
    EXPECT_EQ(static_cast<mode_t>(0600), Mode(mounted_file) & 0777);
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, umount(mountpoint.c_str())) << std::strerror(errno);
    EXPECT_EQ(0, rmdir(mountpoint.c_str()));
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
