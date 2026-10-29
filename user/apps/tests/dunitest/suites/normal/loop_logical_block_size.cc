#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstdint>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <linux/fs.h>
#include <linux/loop.h>
#include <string>
#include <sys/ioctl.h>
#include <unistd.h>
#include <vector>

namespace {
struct ScopedFd {
    int value;
    ~ScopedFd() { if (value >= 0) close(value); }
};
struct ScopedLoop {
    int value;
    ~ScopedLoop() {
        if (value >= 0) { ioctl(value, LOOP_CLR_FD, 0); close(value); }
    }
};
int OpenFreeLoop() {
    ScopedFd control {open("/dev/loop-control", O_RDWR)};
    if (control.value < 0) return -1;
    int minor = ioctl(control.value, LOOP_CTL_GET_FREE, 0);
    if (minor < 0) return -1;
    std::string path = "/dev/loop" + std::to_string(minor);
    return open(path.c_str(), O_RDWR);
}
class LoopLogicalBlockSize : public testing::Test {
protected:
    void SetUp() override {
        const char* dir = getenv("DUNITEST_LOOP_BACKING_DIR");
        std::string name = std::string(dir ? dir : "/tmp") + "/loop_logical_XXXXXX";
        std::vector<char> path(name.begin(), name.end()); path.push_back(0);
        backing_ = mkstemp(path.data());
        ASSERT_GE(backing_, 0) << strerror(errno);
        backing_path_ = path.data();
        ASSERT_EQ(0, ftruncate(backing_, kCapacity));
        int control = open("/dev/loop-control", O_RDWR);
        ASSERT_GE(control, 0);
        int minor = ioctl(control, LOOP_CTL_GET_FREE, 0);
        close(control);
        ASSERT_GE(minor, 0);
        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_ = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(loop_, 0) << strerror(errno);
    }
    void TearDown() override {
        if (loop_ >= 0) {
            loop_info64 info {};
            if (ioctl(loop_, LOOP_GET_STATUS64, &info) == 0) {
                EXPECT_EQ(0, ioctl(loop_, LOOP_CLR_FD, 0)) << strerror(errno);
            }
            close(loop_);
        }
        if (backing_ >= 0) close(backing_);
        if (!backing_path_.empty()) unlink(backing_path_.c_str());
    }
    void Configure(uint32_t logical, uint64_t offset = 0, uint64_t limit = 0) {
        loop_config config {};
        config.fd = backing_;
        config.block_size = logical;
        config.info.lo_offset = offset;
        config.info.lo_sizelimit = limit;
        ASSERT_EQ(0, ioctl(loop_, LOOP_CONFIGURE, &config)) << strerror(errno);
    }
    void ExpectGeometry(unsigned logical, uint64_t capacity = kCapacity) {
        for (unsigned long command : {BLKSSZGET, BLKPBSZGET, BLKIOMIN}) {
            unsigned value = 0;
            ASSERT_EQ(0, ioctl(loop_, command, &value)) << command;
            EXPECT_EQ(logical, value) << command;
        }
        uint64_t bytes = 0;
        ASSERT_EQ(0, ioctl(loop_, BLKGETSIZE64, &bytes));
        EXPECT_EQ(capacity, bytes);
    }
    void ExpectSetError(unsigned long size, int error) {
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_BLOCK_SIZE, size));
        EXPECT_EQ(error, errno) << "size=" << size;
    }
    static constexpr uint64_t kCapacity = 65536 + 512;
    int backing_ = -1, loop_ = -1;
    std::string backing_path_, loop_path_;
};

class LoopConfiguredGeometry : public LoopLogicalBlockSize,
                               public testing::WithParamInterface<unsigned> {};
TEST_P(LoopConfiguredGeometry, PublishesCompleteQueueGeometryWithoutChangingCapacity) {
    ASSERT_NO_FATAL_FAILURE(Configure(GetParam()));
    ASSERT_NO_FATAL_FAILURE(ExpectGeometry(GetParam() ? GetParam() : 512));
}
INSTANTIATE_TEST_SUITE_P(SupportedSizes, LoopConfiguredGeometry,
                        testing::Values(0U, 512U, 1024U, 2048U, 4096U));

TEST_F(LoopLogicalBlockSize, UnboundStatePrecedesInvalidSize) {
    for (unsigned long size : {0UL, 1UL, 512UL, 768UL, 4096UL, 8192UL})
        ExpectSetError(size, ENXIO);
}

TEST_F(LoopLogicalBlockSize, InvalidConfigureLeavesDeviceUnbound) {
    for (unsigned size : {1U, 768U, 8192U}) {
        loop_config config {}; config.fd = backing_; config.block_size = size;
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_, LOOP_CONFIGURE, &config));
        EXPECT_EQ(EINVAL, errno);
        loop_info64 info {};
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_, LOOP_GET_STATUS64, &info));
        EXPECT_EQ(ENXIO, errno);
    }
}

TEST_F(LoopLogicalBlockSize, BadBackingDescriptorPrecedesInvalidSize) {
    loop_config config {}; config.fd = UINT32_MAX; config.block_size = 768;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_CONFIGURE, &config));
    EXPECT_EQ(EBADF, errno);
}

TEST_F(LoopLogicalBlockSize, SetValidatesFullWidthAndPreservesGeometryOnFailure) {
    ASSERT_NO_FATAL_FAILURE(Configure(1024));
    for (unsigned long size : {0UL, 1UL, 768UL, 8192UL, (1UL << 32) + 512}) {
        ExpectSetError(size, EINVAL);
        ASSERT_NO_FATAL_FAILURE(ExpectGeometry(1024));
    }
}

TEST_F(LoopLogicalBlockSize, RuntimeUpdatesAllQueueLimitsButPreservesSoftBlockSize) {
    ASSERT_NO_FATAL_FAILURE(Configure(512));
    unsigned initial_soft = 0;
    ASSERT_EQ(0, ioctl(loop_, BLKBSZGET, &initial_soft));
    for (unsigned size : {4096U, 1024U, 2048U, 512U, 512U}) {
        ASSERT_EQ(0, ioctl(loop_, LOOP_SET_BLOCK_SIZE, size));
        ASSERT_NO_FATAL_FAILURE(ExpectGeometry(size));
        unsigned soft = 0;
        ASSERT_EQ(0, ioctl(loop_, BLKBSZGET, &soft));
        EXPECT_EQ(initial_soft, soft);
    }
}

TEST_F(LoopLogicalBlockSize, ByteOffsetAndLimitRemainByteParametersWithLargeLogicalSize) {
    ASSERT_NO_FATAL_FAILURE(Configure(4096, 513, 1701));
    ASSERT_NO_FATAL_FAILURE(ExpectGeometry(4096, 1536));
    loop_info64 info {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &info));
    EXPECT_EQ(513U, info.lo_offset);
    EXPECT_EQ(1701U, info.lo_sizelimit);
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_CAPACITY, 0));
    ASSERT_NO_FATAL_FAILURE(ExpectGeometry(4096, 1536));
}

TEST_F(LoopLogicalBlockSize, AlreadyOpenDirectDescriptionUsesUpdatedLogicalSize) {
    ASSERT_NO_FATAL_FAILURE(Configure(512));
    ScopedFd direct {open(loop_path_.c_str(), O_RDWR | O_DIRECT)};
    ASSERT_GE(direct.value, 0);
    alignas(4096) std::array<char, 8192> storage;
    char* data = storage.data();
    memset(data, 'L', storage.size());
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_BLOCK_SIZE, 4096));
    errno = 0;
    EXPECT_EQ(-1, pwrite(direct.value, data, 512, 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, pread(direct.value, data, 4096, 512));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, pwrite(direct.value, data + 1, 4096, 0));
    EXPECT_EQ(EINVAL, errno);
    // Memory DMA alignment stays 512 even when request geometry is 4096.
    EXPECT_EQ(4096, pwrite(direct.value, data + 512, 4096, 0));
    EXPECT_EQ(4096, pread(direct.value, data + 512, 4096, 0));
    EXPECT_EQ(4096, pwrite(direct.value, data, 4096, 0));
    EXPECT_EQ(4096, pread(direct.value, data, 4096, 0));
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_BLOCK_SIZE, 1024));
    EXPECT_EQ(1024, pwrite(direct.value, data, 1024, 1024));
    EXPECT_EQ(1024, pread(direct.value, data, 1024, 1024));
}

TEST_F(LoopLogicalBlockSize, DirectChecksClippedTailButEofPrecedesAlignment) {
    ASSERT_NO_FATAL_FAILURE(Configure(4096));
    ScopedFd direct {open(loop_path_.c_str(), O_RDWR | O_DIRECT)};
    ASSERT_GE(direct.value, 0);
    alignas(4096) std::array<char, 8192> storage {};
    char* data = storage.data();
    for (off_t offset : {off_t(65536), off_t(61440)}) {
        errno = 0;
        EXPECT_EQ(-1, pwrite(direct.value, data, 8192, offset));
        EXPECT_EQ(EINVAL, errno);
        errno = 0;
        EXPECT_EQ(-1, pread(direct.value, data, 8192, offset));
        EXPECT_EQ(EINVAL, errno);
    }
    EXPECT_EQ(0, pread(direct.value, data, 4096, kCapacity));
    errno = 0;
    EXPECT_EQ(-1, pwrite(direct.value, data, 4096, kCapacity));
    EXPECT_EQ(ENOSPC, errno);
}

TEST_F(LoopLogicalBlockSize, GeometryReevaluatesDirectBackingOpenIntent) {
    ASSERT_NO_FATAL_FAILURE(Configure(4096));
    ScopedFd backing {open(loop_path_.c_str(), O_RDWR | O_DIRECT)};
    ASSERT_GE(backing.value, 0);
    ScopedLoop outer {OpenFreeLoop()};
    ASSERT_GE(outer.value, 0);
    loop_config config {}; config.fd = backing.value;
    // Raw block inodes have no filesystem superblock backing device; default
    // remains 512 even when that raw device currently has 4096-byte geometry.
    ASSERT_EQ(0, ioctl(outer.value, LOOP_CONFIGURE, &config));
    unsigned logical = 0;
    ASSERT_EQ(0, ioctl(outer.value, BLKSSZGET, &logical));
    EXPECT_EQ(512U, logical);
    for (unsigned size : {512U, 4096U, 512U, 4096U}) {
        ASSERT_EQ(0, ioctl(outer.value, LOOP_SET_BLOCK_SIZE, size));
        loop_info64 status {};
        ASSERT_EQ(0, ioctl(outer.value, LOOP_GET_STATUS64, &status));
        EXPECT_EQ(size == 4096, !!(status.lo_flags & LO_FLAGS_DIRECT_IO));
    }
}

TEST_F(LoopLogicalBlockSize, DisabledExplicitDirectIntentDoesNotPersistAsHiddenRequest) {
    ASSERT_NO_FATAL_FAILURE(Configure(4096));
    ScopedLoop outer {OpenFreeLoop()};
    ASSERT_GE(outer.value, 0);
    loop_config config {}; config.fd = loop_; config.block_size = 512;
    config.info.lo_flags = LO_FLAGS_DIRECT_IO;
    ASSERT_EQ(0, ioctl(outer.value, LOOP_CONFIGURE, &config));
    ASSERT_EQ(0, ioctl(outer.value, LOOP_SET_BLOCK_SIZE, 4096));
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(outer.value, LOOP_GET_STATUS64, &status));
    EXPECT_EQ(0U, status.lo_flags & LO_FLAGS_DIRECT_IO);
    ASSERT_EQ(0, ioctl(outer.value, LOOP_SET_DIRECT_IO, 1));
    ASSERT_EQ(0, ioctl(outer.value, LOOP_GET_STATUS64, &status));
    EXPECT_NE(0U, status.lo_flags & LO_FLAGS_DIRECT_IO);
    ASSERT_EQ(0, ioctl(outer.value, LOOP_SET_BLOCK_SIZE, 512));
    ASSERT_EQ(0, ioctl(outer.value, LOOP_SET_BLOCK_SIZE, 4096));
    ASSERT_EQ(0, ioctl(outer.value, LOOP_GET_STATUS64, &status));
    EXPECT_EQ(0U, status.lo_flags & LO_FLAGS_DIRECT_IO);
}

TEST_F(LoopLogicalBlockSize, LogicalChangeDrainsOldDirtyDataBeforePublishing) {
    ASSERT_NO_FATAL_FAILURE(Configure(512));
    std::array<char, 512> bytes; bytes.fill('Q');
    ASSERT_EQ(512, pwrite(loop_, bytes.data(), bytes.size(), 4096));
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_BLOCK_SIZE, 4096));
    bytes.fill(0);
    ASSERT_EQ(512, pread(backing_, bytes.data(), bytes.size(), 4096));
    for (char byte : bytes) EXPECT_EQ('Q', byte);
}
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
