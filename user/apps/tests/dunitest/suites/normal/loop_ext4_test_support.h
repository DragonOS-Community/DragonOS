#pragma once

#include <gtest/gtest.h>

#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>

namespace dunitest {

constexpr unsigned long kLoopCtlGetFree = 0x4C82;
constexpr unsigned long kLoopSetFd = 0x4C00;
constexpr unsigned long kLoopClrFd = 0x4C01;

inline std::string FixturePath(const char* name = "ext4_inode_identity.img") {
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
    return path + "/fixtures/" + name;
}

inline void CopySparseFile(const std::string& source, int destination) {
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
        if (second_mounted_) {
            umount(second_mount_point_.c_str());
        }
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
        if (!second_mount_point_.empty()) {
            rmdir(second_mount_point_.c_str());
        }
        if (!image_.empty()) {
            unlink(image_.c_str());
        }
    }

    void SetUp(const char* fixture = "ext4_inode_identity.img") {
        image_ = "/tmp/ext4_inode_identity_" + std::to_string(getpid()) + ".img";
        mount_point_ = "/tmp/ext4_inode_identity_" + std::to_string(getpid()) + "_mnt";

        ASSERT_EQ(0, mkdir(mount_point_.c_str(), 0700)) << strerror(errno);

        backing_fd_ = open(image_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
        ASSERT_GE(backing_fd_, 0) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(CopySparseFile(FixturePath(fixture), backing_fd_));
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

    void Mount(unsigned long flags = 0) {
        ASSERT_EQ(0, mount(loop_path_.c_str(), mount_point_.c_str(), "ext4", flags, nullptr))
            << strerror(errno);
        mounted_ = true;
    }

    void Unmount() {
        ASSERT_TRUE(mounted_);
        ASSERT_EQ(0, umount(mount_point_.c_str())) << strerror(errno);
        mounted_ = false;
    }

    void RequestAutoclearAndCloseLoop() {
        ASSERT_GE(loop_fd_, 0);
        ASSERT_EQ(0, ioctl(loop_fd_, kLoopClrFd, 0)) << strerror(errno);
        ASSERT_EQ(0, close(loop_fd_)) << strerror(errno);
        loop_fd_ = -1;
    }

    void ExpectLoopUnbound() const {
        int probe = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(probe, 0) << strerror(errno);
        unsigned char block[512] = {};
        errno = 0;
        EXPECT_EQ(-1, pread(probe, block, sizeof(block), 0));
        EXPECT_EQ(ENODEV, errno);
        EXPECT_EQ(0, close(probe));
    }

    void MountSecond(const char* options = nullptr) {
        ASSERT_TRUE(mounted_ || detached_);
        ASSERT_FALSE(second_mounted_);
        second_mount_point_ = "/tmp/ext4_inode_identity_" + std::to_string(getpid())
            + "_second_mnt";
        ASSERT_EQ(0, mkdir(second_mount_point_.c_str(), 0700)) << strerror(errno);
        ASSERT_EQ(0, mount(loop_path_.c_str(), second_mount_point_.c_str(), "ext4", 0, options))
            << strerror(errno);
        second_mounted_ = true;
    }

    void UnmountSecond() {
        ASSERT_TRUE(second_mounted_);
        ASSERT_EQ(0, umount(second_mount_point_.c_str())) << strerror(errno);
        second_mounted_ = false;
        ASSERT_EQ(0, rmdir(second_mount_point_.c_str())) << strerror(errno);
        second_mount_point_.clear();
    }

    void Detach() {
        ASSERT_TRUE(mounted_);
        ASSERT_EQ(0, umount2(mount_point_.c_str(), MNT_DETACH)) << strerror(errno);
        mounted_ = false;
        detached_ = true;
    }

    void FinishDetached() {
        ASSERT_TRUE(detached_);
        // MNT_DETACH drops the namespace edge immediately, while final
        // superblock teardown runs after the last external owner is released.
        // Do not detach the loop backing underneath that teardown.
        usleep(50 * 1000);
        bool cleared = false;
        for (int attempt = 0; attempt < 100; ++attempt) {
            if (ioctl(loop_fd_, kLoopClrFd, 0) == 0) {
                cleared = true;
                break;
            }
            ASSERT_EQ(EBUSY, errno) << strerror(errno);
            usleep(5 * 1000);
        }
        ASSERT_TRUE(cleared) << "detached ext4 mount retained the loop device";
        detached_ = false;
        close(loop_fd_);
        loop_fd_ = -1;
        close(backing_fd_);
        backing_fd_ = -1;
        ASSERT_EQ(0, rmdir(mount_point_.c_str())) << strerror(errno);
        mount_point_.clear();
        ASSERT_EQ(0, unlink(image_.c_str())) << strerror(errno);
        image_.clear();
    }

    const std::string& loop_path() const {
        return loop_path_;
    }

    const std::string& mount_point() const {
        return mount_point_;
    }

    const std::string& second_mount_point() const {
        return second_mount_point_;
    }

  private:
    std::string image_;
    std::string mount_point_;
    std::string second_mount_point_;
    std::string loop_path_;
    int backing_fd_ = -1;
    int loop_fd_ = -1;
    bool mounted_ = false;
    bool second_mounted_ = false;
    bool detached_ = false;
};

}  // namespace dunitest

