// Optional, real QEMU blkdebug fault-injection checks. No synthetic errno or
// production-kernel hook is used; normal CI skips without the explicit mode.
#include <gtest/gtest.h>
#include <array>
#include <cerrno>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/statfs.h>
#include <unistd.h>

namespace {
struct Fd {
    int value = -1;
    ~Fd() { if (value >= 0) close(value); }
};
bool Mode(const char* expected) {
    const char* mode = getenv("DUNITEST_FAT_FAULT");
    return mode && std::string(mode) == expected;
}
std::string Backing() {
    const char* path = getenv("DUNITEST_FAT_BACKING_DIR");
    return path ? path : "/mnt/fat";
}
void CheckFat(const std::string& path) {
    struct statfs fs {};
    ASSERT_EQ(0, statfs(path.c_str(), &fs)) << strerror(errno);
    ASSERT_EQ(0x4d44, fs.f_type);
}
void RetrySync(int fd) {
    for (int retry = 0; retry < 3; ++retry) {
        errno = 0;
        if (fsync(fd) == 0) return;
        ASSERT_EQ(EIO, errno) << strerror(errno);
    }
    FAIL() << "same-OFD fsync did not recover within three attempts";
}
}

TEST(FatLoopFaults, OneMirrorWriteErrorIsReportedAndSameFdRecovers) {
    if (!Mode("mirror")) GTEST_SKIP() << "requires owned blkdebug FAT snapshot";
    const std::string directory = Backing();
    ASSERT_NO_FATAL_FAILURE(CheckFat(directory));
    const std::string path = directory + "/mirror-fault-" + std::to_string(getpid());
    struct Cleanup {
        std::string path;
        bool created = false;
        ~Cleanup() { if (created) unlink(path.c_str()); }
    } cleanup {path};
    Fd fd {open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
    ASSERT_GE(fd.value, 0) << strerror(errno);
    cleanup.created = true;
    std::array<unsigned char, 4096> buffer {};
    constexpr size_t kLength = 2 * 1024 * 1024;
    for (size_t offset = 0; offset < kLength; offset += buffer.size()) {
        for (size_t i = 0; i < buffer.size(); ++i) buffer[i] = (offset + i) % 251;
        ASSERT_EQ(static_cast<ssize_t>(buffer.size()), write(fd.value, buffer.data(), buffer.size()))
            << strerror(errno);
    }
    errno = 0;
    ASSERT_EQ(-1, fsync(fd.value));
    ASSERT_EQ(EIO, errno) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(RetrySync(fd.value));
    // This verifies the recovered live file; cold remount and mirror-byte
    // comparison are separate checks, not asserted by a cached read here.
    for (size_t offset = 0; offset < kLength; offset += buffer.size()) {
        ASSERT_EQ(static_cast<ssize_t>(buffer.size()),
                  pread(fd.value, buffer.data(), buffer.size(), offset)) << strerror(errno);
        for (size_t i = 0; i < buffer.size(); ++i) {
            ASSERT_EQ(static_cast<unsigned char>((offset + i) % 251), buffer[i])
                << "offset=" << offset + i;
        }
    }
}

TEST(FatLoopFaults, OnePhysicalFlushErrorIsReportedAndSameDirectoryFdRecovers) {
    if (!Mode("flush")) GTEST_SKIP() << "requires owned blkdebug FAT snapshot";
    const std::string directory = Backing();
    ASSERT_NO_FATAL_FAILURE(CheckFat(directory));
    // A new QEMU graph may elide a flush until a write changes its generation.
    // The harness must identify its *owned* scratch device explicitly. Rewrite
    // the original boot sector unchanged, without an earlier fsync consuming
    // the once-only physical-flush injection.
    const char* device_path = getenv("DUNITEST_FAT_FAULT_DEVICE");
    ASSERT_NE(nullptr, device_path) << "explicit owned scratch device required";
    ASSERT_NE('\0', *device_path);
    Fd device {open(device_path, O_RDWR)};
    ASSERT_GE(device.value, 0) << strerror(errno);
    std::array<unsigned char, 512> boot {};
    ASSERT_EQ(static_cast<ssize_t>(boot.size()), pread(device.value, boot.data(), boot.size(), 0));
    ASSERT_EQ(0x55, boot[510]);
    ASSERT_EQ(0xaa, boot[511]);
    ASSERT_EQ(0, boot[11]);
    ASSERT_EQ(2, boot[12]);  // 512-byte sectors.
    ASSERT_EQ(1, boot[13]);  // Original one-sector cluster geometry.
    ASSERT_EQ(32, boot[14]);
    ASSERT_EQ(0, boot[15]);
    ASSERT_EQ(2, boot[16]);
    ASSERT_EQ(0xf1, boot[36]);  // FAT size=1009 sectors on this owned fixture.
    ASSERT_EQ(3, boot[37]);
    ASSERT_EQ(0, boot[38]);
    ASSERT_EQ(0, boot[39]);
    ASSERT_EQ(static_cast<ssize_t>(boot.size()), pwrite(device.value, boot.data(), boot.size(), 0));
    Fd dir {open(directory.c_str(), O_RDONLY | O_DIRECTORY)};
    ASSERT_GE(dir.value, 0) << strerror(errno);
    errno = 0;
    ASSERT_EQ(-1, fsync(dir.value));
    ASSERT_EQ(EIO, errno) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(RetrySync(dir.value));
}

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
