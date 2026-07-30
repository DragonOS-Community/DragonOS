#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <stdio.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {

constexpr unsigned long kLoopCtlAdd = 0x4C80;
constexpr unsigned long kLoopCtlRemove = 0x4C81;
constexpr unsigned long kLoopCtlGetFree = 0x4C82;
constexpr unsigned long kLoopSetFd = 0x4C00;
constexpr unsigned long kLoopClrFd = 0x4C01;
constexpr unsigned long kLoopSetStatus64 = 0x4C04;
constexpr unsigned long kLoopGetStatus64 = 0x4C05;
constexpr uint32_t kLoopFlagAutoclear = 1U << 2;
constexpr size_t kBackingSize = 1024 * 1024;

struct LoopInfo64 {
    uint64_t device;
    uint64_t inode;
    uint64_t rdevice;
    uint64_t offset;
    uint64_t size_limit;
    uint32_t number;
    uint32_t encrypt_type;
    uint32_t encrypt_key_size;
    uint32_t flags;
    uint8_t file_name[64];
    uint8_t crypt_name[64];
    uint8_t encrypt_key[32];
    uint64_t init[2];
};

class LoopDevice {
public:
    ~LoopDevice() {
        if (loop_fd_ >= 0) {
            ioctl(loop_fd_, kLoopClrFd, 0);
            close(loop_fd_);
        }
        if (backing_fd_ >= 0) {
            close(backing_fd_);
        }
        if (control_fd_ >= 0) {
            if (owns_registration_ && minor_ >= 0) {
                ioctl(control_fd_, kLoopCtlRemove, minor_);
            }
            close(control_fd_);
        }
        if (!backing_path_.empty()) {
            unlink(backing_path_.c_str());
        }
    }

    void SetUp(int loop_open_flags, bool create_new = true) {
        backing_path_ =
            "/tmp/dunitest_loop_semantics_" + std::to_string(static_cast<long long>(getpid()));
        backing_fd_ = open(backing_path_.c_str(), O_CREAT | O_TRUNC | O_RDWR, 0600);
        ASSERT_GE(backing_fd_, 0) << strerror(errno);
        ASSERT_EQ(ftruncate(backing_fd_, static_cast<off_t>(kBackingSize)), 0) << strerror(errno);

        control_fd_ = open("/dev/loop-control", O_RDWR);
        ASSERT_GE(control_fd_, 0) << strerror(errno);

        if (create_new) {
            minor_ = ioctl(control_fd_, kLoopCtlAdd, UINT32_MAX);
            owns_registration_ = true;
        } else {
            minor_ = ioctl(control_fd_, kLoopCtlGetFree, 0);
        }
        ASSERT_GE(minor_, 0) << strerror(errno);

        char path[64];
        snprintf(path, sizeof(path), "/dev/loop%d", minor_);
        loop_path_ = path;
        loop_fd_ = open(loop_path_.c_str(), loop_open_flags);
        ASSERT_GE(loop_fd_, 0) << strerror(errno);
        ASSERT_EQ(ioctl(loop_fd_, kLoopSetFd, backing_fd_), 0) << strerror(errno);
    }

    void CloseOriginalBackingFd() {
        ASSERT_GE(backing_fd_, 0);
        ASSERT_EQ(close(backing_fd_), 0) << strerror(errno);
        backing_fd_ = -1;
    }

    void GrowBackingFile() {
        ASSERT_GE(backing_fd_, 0);
        ASSERT_EQ(ftruncate(backing_fd_, static_cast<off_t>(2 * kBackingSize)), 0)
            << strerror(errno);
    }

    void ClearLoop() {
        ASSERT_GE(loop_fd_, 0);
        ASSERT_EQ(ioctl(loop_fd_, kLoopClrFd, 0), 0) << strerror(errno);
    }

    void CloseLoopFd() {
        ASSERT_GE(loop_fd_, 0);
        ASSERT_EQ(close(loop_fd_), 0) << strerror(errno);
        loop_fd_ = -1;
    }

    void RemoveLoop() {
        ASSERT_GE(control_fd_, 0);
        ASSERT_GE(minor_, 0);
        ASSERT_TRUE(owns_registration_);
        ASSERT_EQ(ioctl(control_fd_, kLoopCtlRemove, minor_), 0) << strerror(errno);
        minor_ = -1;
        owns_registration_ = false;
    }

    int loop_fd() const { return loop_fd_; }
    int control_fd() const { return control_fd_; }
    int minor() const { return minor_; }
    const std::string& loop_path() const { return loop_path_; }
    const std::string& backing_path() const { return backing_path_; }

private:
    int control_fd_ = -1;
    int backing_fd_ = -1;
    int loop_fd_ = -1;
    int minor_ = -1;
    bool owns_registration_ = false;
    std::string loop_path_;
    std::string backing_path_;
};

TEST(LoopSemantics, GetFreeReturnsRegisteredDevice) {
    int control_fd = open("/dev/loop-control", O_RDWR);
    ASSERT_GE(control_fd, 0) << strerror(errno);
    int minor = ioctl(control_fd, kLoopCtlGetFree, 0);
    ASSERT_GE(minor, 0) << strerror(errno);

    char path[64];
    snprintf(path, sizeof(path), "/dev/loop%d", minor);
    int loop_fd = open(path, O_RDWR);
    ASSERT_GE(loop_fd, 0) << "LOOP_CTL_GET_FREE returned an unregistered device: "
                          << strerror(errno);

    errno = 0;
    EXPECT_EQ(ioctl(control_fd, kLoopCtlAdd, minor), -1);
    EXPECT_EQ(errno, EEXIST);
    EXPECT_EQ(close(loop_fd), 0) << strerror(errno);
    EXPECT_EQ(close(control_fd), 0) << strerror(errno);
}

TEST(LoopSemantics, SyncIoRetainsBackingOpenFileDescription) {
    LoopDevice device;
    ASSERT_NO_FATAL_FAILURE(device.SetUp(O_RDWR | O_SYNC));
    ASSERT_NO_FATAL_FAILURE(device.CloseOriginalBackingFd());

    int dsync_fd = open(device.loop_path().c_str(), O_RDWR | O_DSYNC);
    ASSERT_GE(dsync_fd, 0) << strerror(errno);

    unsigned char sync_data[512] = {};
    unsigned char dsync_data[512] = {};
    memset(sync_data, 0x5a, sizeof(sync_data));
    memset(dsync_data, 0xa5, sizeof(dsync_data));
    ASSERT_EQ(pwrite(device.loop_fd(), sync_data, sizeof(sync_data), 0),
              static_cast<ssize_t>(sizeof(sync_data)))
        << strerror(errno);
    ASSERT_EQ(pwrite(dsync_fd, dsync_data, sizeof(dsync_data), 512),
              static_cast<ssize_t>(sizeof(dsync_data)))
        << strerror(errno);
    ASSERT_EQ(fsync(device.loop_fd()), 0) << strerror(errno);
    ASSERT_EQ(fdatasync(dsync_fd), 0) << strerror(errno);
    ASSERT_EQ(close(dsync_fd), 0) << strerror(errno);

    int verify_fd = open(device.backing_path().c_str(), O_RDONLY);
    ASSERT_GE(verify_fd, 0) << strerror(errno);
    unsigned char observed[1024] = {};
    ASSERT_EQ(pread(verify_fd, observed, sizeof(observed), 0),
              static_cast<ssize_t>(sizeof(observed)))
        << strerror(errno);
    EXPECT_EQ(memcmp(observed, sync_data, sizeof(sync_data)), 0);
    EXPECT_EQ(memcmp(observed + 512, dsync_data, sizeof(dsync_data)), 0);
    EXPECT_EQ(close(verify_fd), 0) << strerror(errno);
}

TEST(LoopSemantics, ControlRemoveRejectsBoundDeviceWithOpener) {
    LoopDevice device;
    ASSERT_NO_FATAL_FAILURE(device.SetUp(O_RDWR, true));

    errno = 0;
    EXPECT_EQ(ioctl(device.control_fd(), kLoopCtlRemove, device.minor()), -1);
    EXPECT_EQ(errno, EBUSY);

    unsigned char block[512] = {};
    EXPECT_EQ(pread(device.loop_fd(), block, sizeof(block), 0),
              static_cast<ssize_t>(sizeof(block)))
        << "failed removal attempt damaged the bound device: " << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(device.ClearLoop());
    errno = 0;
    EXPECT_EQ(ioctl(device.loop_fd(), kLoopClrFd, 0), -1);
    EXPECT_EQ(errno, ENXIO);

    errno = 0;
    EXPECT_EQ(ioctl(device.control_fd(), kLoopCtlRemove, device.minor()), -1);
    EXPECT_EQ(errno, EBUSY)
        << "an unbound device must remain registered while its last opener exists";

    ASSERT_NO_FATAL_FAILURE(device.CloseLoopFd());
}

TEST(LoopSemantics, BusyClearDefersUntilLastOpener) {
    LoopDevice device;
    ASSERT_NO_FATAL_FAILURE(device.SetUp(O_RDWR));

    int extra_fd = open(device.loop_path().c_str(), O_RDWR);
    ASSERT_GE(extra_fd, 0) << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(device.ClearLoop());

    unsigned char block[512] = {};
    EXPECT_EQ(pread(device.loop_fd(), block, sizeof(block), 0),
              static_cast<ssize_t>(sizeof(block)))
        << "deferred clear detached a busy loop device: " << strerror(errno);

    ASSERT_EQ(close(extra_fd), 0) << strerror(errno);
    EXPECT_EQ(pread(device.loop_fd(), block, sizeof(block), 0),
              static_cast<ssize_t>(sizeof(block)))
        << "autoclear ran before the final opener closed: " << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(device.CloseLoopFd());

    int probe_fd = open(device.loop_path().c_str(), O_RDWR);
    ASSERT_GE(probe_fd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(pread(probe_fd, block, sizeof(block), 0), -1);
    EXPECT_EQ(errno, ENODEV);
    ASSERT_EQ(close(probe_fd), 0) << strerror(errno);

    ASSERT_NO_FATAL_FAILURE(device.RemoveLoop());
}

TEST(LoopSemantics, Status64UpdatesAutoclearFlag) {
    LoopDevice device;
    ASSERT_NO_FATAL_FAILURE(device.SetUp(O_RDWR));

    LoopInfo64 status = {};
    ASSERT_EQ(ioctl(device.loop_fd(), kLoopGetStatus64, &status), 0) << strerror(errno);
    EXPECT_EQ(status.flags & kLoopFlagAutoclear, 0U);

    ASSERT_NO_FATAL_FAILURE(device.GrowBackingFile());
    unsigned char block[512] = {};
    errno = 0;
    EXPECT_EQ(pread(device.loop_fd(), block, sizeof(block), kBackingSize), -1)
        << "backing growth changed loop capacity without LOOP_SET_CAPACITY";

    status.flags |= kLoopFlagAutoclear;
    ASSERT_EQ(ioctl(device.loop_fd(), kLoopSetStatus64, &status), 0) << strerror(errno);
    memset(&status, 0, sizeof(status));
    ASSERT_EQ(ioctl(device.loop_fd(), kLoopGetStatus64, &status), 0) << strerror(errno);
    EXPECT_EQ(status.flags & kLoopFlagAutoclear, kLoopFlagAutoclear);
    errno = 0;
    EXPECT_EQ(pread(device.loop_fd(), block, sizeof(block), kBackingSize), -1)
        << "flag-only LOOP_SET_STATUS64 unexpectedly refreshed loop capacity";

    status.flags &= ~kLoopFlagAutoclear;
    ASSERT_EQ(ioctl(device.loop_fd(), kLoopSetStatus64, &status), 0) << strerror(errno);
    memset(&status, 0, sizeof(status));
    ASSERT_EQ(ioctl(device.loop_fd(), kLoopGetStatus64, &status), 0) << strerror(errno);
    EXPECT_EQ(status.flags & kLoopFlagAutoclear, 0U);
}

TEST(LoopSemantics, DeviceNodeRejectsUnprivilegedOpen) {
    LoopDevice device;
    ASSERT_NO_FATAL_FAILURE(device.SetUp(O_RDWR));

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) {
            _exit(2);
        }
        errno = 0;
        const int fd = open(device.loop_path().c_str(), O_RDONLY);
        if (fd >= 0) {
            close(fd);
            _exit(3);
        }
        _exit(errno == EACCES ? 0 : 4);
    }

    int status = 0;
    ASSERT_EQ(waitpid(child, &status, 0), child) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(WEXITSTATUS(status), 0);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
