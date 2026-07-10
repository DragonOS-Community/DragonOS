#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

#include <string>

namespace {

constexpr unsigned long kLoopCtlGetFree = 0x4C82;
constexpr unsigned long kLoopSetFd = 0x4C00;
constexpr unsigned long kLoopClrFd = 0x4C01;

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
        ASSERT_EQ(0, ftruncate(backing_fd_, 32 * 1024 * 1024)) << strerror(errno);
        close(backing_fd_);
        backing_fd_ = -1;

        std::string command = "/usr/sbin/mke2fs -q -t ext4 -F " + image_;
        ASSERT_EQ(0, system(command.c_str())) << "mke2fs failed";

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

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
