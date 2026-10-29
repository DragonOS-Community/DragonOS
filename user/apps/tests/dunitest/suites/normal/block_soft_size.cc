#include <gtest/gtest.h>

#include "loop_ext4_test_support.h"

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
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vector>

namespace {
struct Fd {
    int value = -1;
    ~Fd() { Reset(); }
    void Reset(int value = -1) { if (this->value >= 0) close(this->value); this->value = value; }
};
struct Mount {
    std::string directory;
    bool mounted = false;
    ~Mount() {
        if (mounted) { EXPECT_EQ(0, umount2(directory.c_str(), MNT_DETACH)); }
        if (!directory.empty()) { EXPECT_EQ(0, rmdir(directory.c_str())); }
    }
};
void Le32(unsigned char* dst, uint32_t value) {
    for (unsigned i = 0; i < 4; ++i) dst[i] = value >> (8 * i);
}

class BlockSoftSize : public testing::Test {
protected:
    void SetUp() override {
        const char* directory = getenv("DUNITEST_LOOP_BACKING_DIR");
        std::string pattern = std::string(directory ? directory : "/tmp") + "/block_soft_size_XXXXXX";
        std::vector<char> path(pattern.begin(), pattern.end()); path.push_back(0);
        backing_.value = mkstemp(path.data());
        ASSERT_GE(backing_.value, 0) << strerror(errno);
        backing_path_ = path.data();
        ASSERT_EQ(0, ftruncate(backing_.value, kCapacity));
        Fd control {open("/dev/loop-control", O_RDWR)};
        ASSERT_GE(control.value, 0);
        int minor = ioctl(control.value, LOOP_CTL_GET_FREE, 0);
        ASSERT_GE(minor, 0);
        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_.value = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(loop_.value, 0) << strerror(errno);
    }
    void TearDown() override {
        if (loop_.value < 0 && !loop_path_.empty()) loop_.Reset(open(loop_path_.c_str(), O_RDWR));
        if (loop_.value >= 0) {
            loop_info64 info {};
            if (ioctl(loop_.value, LOOP_GET_STATUS64, &info) == 0) {
                EXPECT_EQ(0, ioctl(loop_.value, LOOP_CLR_FD, 0)) << strerror(errno);
            }
        }
        loop_.Reset(); backing_.Reset();
        if (!backing_path_.empty()) unlink(backing_path_.c_str());
    }
    void Bind() { ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_FD, backing_.value)) << strerror(errno); }
    void SetSize(int size, int fd = -1) {
        ASSERT_EQ(0, ioctl(fd < 0 ? loop_.value : fd, BLKBSZSET, &size)) << strerror(errno);
    }
    void ExpectSize(unsigned expected, int fd = -1) {
        struct { uint32_t value; uint32_t sentinel; } output {0, 0x3a65f190};
        ASSERT_EQ(0, ioctl(fd < 0 ? loop_.value : fd, BLKBSZGET, &output));
        EXPECT_EQ(expected, output.value);
        EXPECT_EQ(0x3a65f190U, output.sentinel);
    }
    void Reopen() {
        loop_.Reset();
        // Linux devtmpfs/udev may still hold the binding-change probe. Closing
        // all fixture references precedes observing the next first open.
        usleep(50000);
        loop_.Reset(open(loop_path_.c_str(), O_RDWR));
        ASSERT_GE(loop_.value, 0) << strerror(errno);
    }
    void ClearAndWait() {
        for (unsigned attempt = 0; attempt < 100; ++attempt) {
            int result = ioctl(loop_.value, LOOP_CLR_FD, 0);
            ASSERT_TRUE(result == 0 || errno == ENXIO) << strerror(errno);
            loop_info64 info {};
            if (ioctl(loop_.value, LOOP_GET_STATUS64, &info) == -1 && errno == ENXIO) return;
            usleep(1000);
        }
        FAIL() << "temporary partition probe retained the binding";
    }
    void PublishPartitions() {
        std::array<unsigned char, 512> mbr {};
        for (unsigned slot = 0; slot < 2; ++slot) {
            unsigned char* entry = mbr.data() + 446 + slot * 16;
            entry[4] = 0x83;
            Le32(entry + 8, 8 + slot * 16);
            Le32(entry + 12, slot ? 8 : 3);
        }
        mbr[510] = 0x55; mbr[511] = 0xaa;
        ASSERT_EQ(512, pwrite(backing_.value, mbr.data(), mbr.size(), 0));
        ASSERT_EQ(0, fsync(backing_.value));
        loop_config config {}; config.fd = backing_.value;
        config.info.lo_flags = LO_FLAGS_PARTSCAN;
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_CONFIGURE, &config)) << strerror(errno);
    }
    void MountFixture(const char* fixture, const char* type, Mount* owner) {
        ASSERT_NO_FATAL_FAILURE(dunitest::CopySparseFile(dunitest::FixturePath(fixture), backing_.value));
        ASSERT_NO_FATAL_FAILURE(Bind());
        ASSERT_NO_FATAL_FAILURE(SetSize(4096));
        ASSERT_NO_FATAL_FAILURE(ExpectSize(4096));
        char path[] = "/tmp/block_soft_mount_XXXXXX";
        ASSERT_NE(nullptr, mkdtemp(path)); owner->directory = path;
        ASSERT_EQ(0, mount(loop_path_.c_str(), path, type, MS_RDONLY, nullptr)) << strerror(errno);
        owner->mounted = true;
    }
    static constexpr off_t kCapacity = 4 * 1024 * 1024;
    Fd loop_, backing_;
    std::string loop_path_, backing_path_;
};

TEST_F(BlockSoftSize, QueryUsesIntegerWidthAndSupportsAllValidSizes) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    for (int size : {512, 1024, 2048, 4096}) {
        ASSERT_NO_FATAL_FAILURE(SetSize(size));
        ASSERT_NO_FATAL_FAILURE(ExpectSize(size));
    }
}

TEST_F(BlockSoftSize, InvalidSizePreservesCurrentSize) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(SetSize(1024));
    for (int size : {-1, 0, 256, 768, 8192}) {
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_.value, BLKBSZSET, &size));
        EXPECT_EQ(EINVAL, errno);
        ASSERT_NO_FATAL_FAILURE(ExpectSize(1024));
    }
}

TEST_F(BlockSoftSize, PointerErrorsPrecedeSizeValidation) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_.value, BLKBSZSET, nullptr));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_.value, BLKBSZSET, reinterpret_cast<void*>(1)));
    EXPECT_EQ(EFAULT, errno);
}

TEST_F(BlockSoftSize, UnprivilegedCapabilityCheckPrecedesNullPointer) {
    ASSERT_EQ(0U, geteuid()) << "fixture requires block-device administration";
    ASSERT_NO_FATAL_FAILURE(Bind());
    pid_t child = fork(); ASSERT_GE(child, 0);
    if (child == 0) {
        if (setuid(65534) != 0) _exit(2);
        errno = 0;
        int result = ioctl(loop_.value, BLKBSZSET, nullptr);
        _exit(result == -1 && errno == EACCES ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(BlockSoftSize, OrdinaryMultipleOpenDescriptionsMayChangeSharedSize) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    Fd other {open(loop_path_.c_str(), O_RDWR)};
    ASSERT_GE(other.value, 0);
    ASSERT_NO_FATAL_FAILURE(SetSize(512));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512, other.value));
    ASSERT_NO_FATAL_FAILURE(SetSize(2048, other.value));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(2048));
}

TEST_F(BlockSoftSize, LogicalChangeDoesNotRewriteRetainedSoftButLimitsNewSet) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(SetSize(512));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_BLOCK_SIZE, 4096));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512));
    Fd other {open(loop_path_.c_str(), O_RDWR)};
    ASSERT_GE(other.value, 0);
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512, other.value));
    int size = 512;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_.value, BLKBSZSET, &size));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_NO_FATAL_FAILURE(SetSize(4096));
}

TEST_F(BlockSoftSize, CapacityRefreshPreservesRetainedSizeUntilFinalClose) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(SetSize(1024));
    ASSERT_EQ(0, ftruncate(backing_.value, 1536));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_CAPACITY, 0));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(1024));
    ASSERT_NO_FATAL_FAILURE(Reopen());
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512));
}

TEST_F(BlockSoftSize, FirstOpenUsesLargestDivisorUpToPage) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    for (off_t capacity : {off_t(1536), off_t(2048), off_t(4096), off_t(0)}) {
        ASSERT_EQ(0, ftruncate(backing_.value, capacity));
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_CAPACITY, 0));
        ASSERT_NO_FATAL_FAILURE(Reopen());
        unsigned expected = capacity == 1536 ? 512 : capacity == 2048 ? 2048 : 4096;
        ASSERT_NO_FATAL_FAILURE(ExpectSize(expected));
    }
}

TEST_F(BlockSoftSize, ClearAndRebindDoNotResetRetainedNodeSize) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(SetSize(2048));
    ASSERT_NO_FATAL_FAILURE(ClearAndWait());
    ASSERT_NO_FATAL_FAILURE(ExpectSize(2048));
    ASSERT_EQ(0, ftruncate(backing_.value, 1536));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(ExpectSize(2048));
    ASSERT_NO_FATAL_FAILURE(Reopen());
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512));
}

TEST_F(BlockSoftSize, PartitionHasOwnSizeAndKeepsWholeOpenLifetime) {
    ASSERT_NO_FATAL_FAILURE(PublishPartitions());
    ASSERT_NO_FATAL_FAILURE(SetSize(1024));
    Fd partition {open((loop_path_ + "p1").c_str(), O_RDWR)};
    ASSERT_GE(partition.value, 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512, partition.value));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(1024));
    ASSERT_NO_FATAL_FAILURE(Reopen());
    // The partition's propagated whole reference prevents reinitialization.
    ASSERT_NO_FATAL_FAILURE(ExpectSize(1024));
    partition.Reset();
    ASSERT_NO_FATAL_FAILURE(Reopen());
    ASSERT_NO_FATAL_FAILURE(ExpectSize(4096));
}

class BlockSoftPartitionClaims : public BlockSoftSize, public testing::WithParamInterface<bool> {};

TEST_P(BlockSoftPartitionClaims, MountedPartitionClaimsItsNodeAndWholeButNotAnotherPartition) {
    Fd fixture {open(dunitest::FixturePath().c_str(), O_RDONLY)};
    ASSERT_GE(fixture.value, 0);
    struct stat metadata {}; ASSERT_EQ(0, fstat(fixture.value, &metadata));
    constexpr uint32_t start = 2048;
    // Exercise both ordinary siblings and a distinct node inside the mounted
    // partition: Linux claims node identities, not physical intervals.
    const uint32_t second_start = GetParam() ? start : start + metadata.st_size / 512 + 8;
    ASSERT_EQ(0, ftruncate(backing_.value, (start + metadata.st_size / 512 + 16) * 512));
    std::array<char, 65536> data {};
    for (off_t offset = 0; offset < metadata.st_size;) {
        ssize_t count = pread(fixture.value, data.data(), data.size(), offset);
        ASSERT_GT(count, 0);
        ASSERT_EQ(count, pwrite(backing_.value, data.data(), count, start * 512 + offset));
        offset += count;
    }
    std::array<unsigned char, 512> mbr {};
    for (unsigned slot = 0; slot < 2; ++slot) {
        unsigned char* entry = mbr.data() + 446 + slot * 16;
        entry[4] = 0x83; Le32(entry + 8, slot ? second_start : start);
        Le32(entry + 12, slot ? 8 : metadata.st_size / 512);
    }
    mbr[510] = 0x55; mbr[511] = 0xaa;
    ASSERT_EQ(512, pwrite(backing_.value, mbr.data(), mbr.size(), 0));
    ASSERT_EQ(0, fsync(backing_.value));
    loop_config config {}; config.fd = backing_.value; config.info.lo_flags = LO_FLAGS_PARTSCAN;
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_CONFIGURE, &config));
    Fd first {open((loop_path_ + "p1").c_str(), O_RDWR)};
    Fd other {open((loop_path_ + "p2").c_str(), O_RDWR)};
    ASSERT_GE(first.value, 0); ASSERT_GE(other.value, 0);
    Mount owner;
    char path[] = "/tmp/block_soft_partition_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(path)); owner.directory = path;
    ASSERT_EQ(0, mount((loop_path_ + "p1").c_str(), path, "ext4", MS_RDONLY, nullptr));
    owner.mounted = true;
    for (int target : {loop_.value, first.value}) {
        for (int size : {512, 4096, 768}) {
            errno = 0;
            EXPECT_EQ(-1, ioctl(target, BLKBSZSET, &size));
            EXPECT_EQ(EBUSY, errno);
        }
    }
    ASSERT_NO_FATAL_FAILURE(SetSize(512, other.value));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512, other.value));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(4096, first.value));
}

INSTANTIATE_TEST_SUITE_P(SeparateAndOverlapping, BlockSoftPartitionClaims, testing::Bool());

class BlockSoftGranule : public BlockSoftSize, public testing::WithParamInterface<int> {};
TEST_P(BlockSoftGranule, WritesExactlyTheConfiguredSoftBlockIncarnation) {
    std::array<char, 4096> initial; initial.fill('A');
    ASSERT_EQ(4096, pwrite(backing_.value, initial.data(), initial.size(), 0));
    ASSERT_EQ(0, fsync(backing_.value));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(SetSize(GetParam()));
    std::array<char, 4096> cached {};
    ASSERT_EQ(4096, pread(loop_.value, cached.data(), cached.size(), 0));
    std::array<char, 512> neighbor; neighbor.fill('B');
    ASSERT_EQ(512, pwrite(backing_.value, neighbor.data(), neighbor.size(), 512));
    ASSERT_EQ(0, fsync(backing_.value));
    std::array<char, 512> first; first.fill('C');
    ASSERT_EQ(512, pwrite(loop_.value, first.data(), first.size(), 0));
    ASSERT_EQ(0, fsync(loop_.value));
    std::array<char, 1024> result {};
    ASSERT_EQ(1024, pread(backing_.value, result.data(), result.size(), 0));
    for (size_t i = 0; i < 512; ++i) EXPECT_EQ('C', result[i]);
    for (size_t i = 512; i < 1024; ++i) EXPECT_EQ(GetParam() == 512 ? 'B' : 'A', result[i]);
}
INSTANTIATE_TEST_SUITE_P(SoftGranules, BlockSoftGranule, testing::Values(512, 1024, 4096));

TEST_F(BlockSoftSize, ChangingSizeFlushesOldGranuleBeforeDiscardingCache) {
    std::array<char, 4096> initial; initial.fill('A');
    ASSERT_EQ(4096, pwrite(backing_.value, initial.data(), initial.size(), 0));
    ASSERT_EQ(0, fsync(backing_.value));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(SetSize(512));
    std::array<char, 4096> cache {};
    ASSERT_EQ(4096, pread(loop_.value, cache.data(), cache.size(), 0));
    std::array<char, 512> neighbor; neighbor.fill('B');
    ASSERT_EQ(512, pwrite(backing_.value, neighbor.data(), neighbor.size(), 512));
    ASSERT_EQ(0, fsync(backing_.value));
    std::array<char, 512> first; first.fill('C');
    ASSERT_EQ(512, pwrite(loop_.value, first.data(), first.size(), 0));
    ASSERT_NO_FATAL_FAILURE(SetSize(1024));
    std::array<char, 1024> result {};
    ASSERT_EQ(1024, pread(backing_.value, result.data(), result.size(), 0));
    for (size_t i = 0; i < 512; ++i) EXPECT_EQ('C', result[i]);
    for (size_t i = 512; i < 1024; ++i) EXPECT_EQ('B', result[i]);
    // Changed soft size invalidates even the formerly clean stale neighbor.
    ASSERT_EQ(1024, pread(loop_.value, result.data(), result.size(), 0));
    for (size_t i = 512; i < 1024; ++i) EXPECT_EQ('B', result[i]);
}

TEST_F(BlockSoftSize, FatMountNegotiatesSectorSizeAndRetainedUnmountKeepsIt) {
    Mount owner;
    ASSERT_NO_FATAL_FAILURE(MountFixture("fat_stat_blocks.img", "vfat", &owner));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512));
    for (int size : {512, 4096, 768}) {
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_.value, BLKBSZSET, &size));
        EXPECT_EQ(EBUSY, errno);
    }
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_.value, BLKBSZSET, reinterpret_cast<void*>(1)));
    EXPECT_EQ(EFAULT, errno);
    ASSERT_EQ(0, umount(owner.directory.c_str())); owner.mounted = false;
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512));
    ASSERT_NO_FATAL_FAILURE(Reopen());
    ASSERT_NO_FATAL_FAILURE(ExpectSize(4096));
}

TEST_F(BlockSoftSize, ForeignExt4MountCannotChangeLiveFatGeometryOrRawCache) {
    Mount fat;
    ASSERT_NO_FATAL_FAILURE(MountFixture("fat_stat_blocks.img", "vfat", &fat));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512));
    unsigned char cached = 0;
    ASSERT_EQ(1, pread(loop_.value, &cached, 1, 400));
    const unsigned char replacement = cached ^ 0x5a;
    // Boot-code padding is irrelevant to the mounted filesystem. This lower
    // alias update makes an unintended raw-cache retirement observable.
    ASSERT_EQ(1, pwrite(backing_.value, &replacement, 1, 400));
    ASSERT_EQ(0, fsync(backing_.value));
    Mount other;
    char path[] = "/tmp/block_soft_foreign_ext4_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(path)); other.directory = path;
    errno = 0;
    const int result = mount(loop_path_.c_str(), path, "ext4", MS_RDONLY, nullptr);
    const int error = errno; other.mounted = result == 0;
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EBUSY, error);
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512));
    unsigned char after = 0;
    ASSERT_EQ(1, pread(loop_.value, &after, 1, 400));
    EXPECT_EQ(cached, after);
}

TEST_F(BlockSoftSize, ForeignFatMountCannotChangeLiveExt4Geometry) {
    Mount ext4;
    ASSERT_NO_FATAL_FAILURE(MountFixture("ext4_inode_identity.img", "ext4", &ext4));
    ASSERT_NO_FATAL_FAILURE(ExpectSize(4096));
    Mount other;
    char path[] = "/tmp/block_soft_foreign_fat_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(path)); other.directory = path;
    errno = 0;
    const int result = mount(loop_path_.c_str(), path, "vfat", MS_RDONLY, nullptr);
    const int error = errno; other.mounted = result == 0;
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EBUSY, error);
    ASSERT_NO_FATAL_FAILURE(ExpectSize(4096));
}

TEST_F(BlockSoftSize, Ext4MountNegotiatesItsSupportedFourKilobyteBlockSize) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(SetSize(512));
    ASSERT_NO_FATAL_FAILURE(ClearAndWait());
    Mount owner;
    // Linux mount changes an already-open node rather than treating a prior
    // BLKBSZSET value as the filesystem's block geometry.
    ASSERT_NO_FATAL_FAILURE(dunitest::CopySparseFile(dunitest::FixturePath(), backing_.value));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(ExpectSize(512));
    char path[] = "/tmp/block_soft_ext4_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(path)); owner.directory = path;
    ASSERT_EQ(0, mount(loop_path_.c_str(), path, "ext4", MS_RDONLY, nullptr)) << strerror(errno);
    owner.mounted = true;
    ASSERT_NO_FATAL_FAILURE(ExpectSize(4096));
    ASSERT_EQ(0, umount(path)); owner.mounted = false;
    ASSERT_NO_FATAL_FAILURE(ExpectSize(4096));
}
}  // namespace
int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
