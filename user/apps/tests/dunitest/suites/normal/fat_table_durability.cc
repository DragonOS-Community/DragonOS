#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cstdint>
#include <string>
#include <vector>

namespace {

struct Fd {
    int value = -1;
    ~Fd() { if (value >= 0) close(value); }
};

uint16_t Le16(const uint8_t* data) { return data[0] | (uint16_t(data[1]) << 8); }
uint32_t Le32(const uint8_t* data) {
    return uint32_t(Le16(data)) | (uint32_t(Le16(data + 2)) << 16);
}

void ReadAt(int fd, void* buffer, size_t length, off_t offset) {
    auto* bytes = static_cast<uint8_t*>(buffer);
    while (length != 0) {
        const ssize_t count = pread(fd, bytes, length, offset);
        if (count < 0 && errno == EINTR) continue;
        ASSERT_GT(count, 0) << "offset=" << offset << ": " << strerror(errno);
        bytes += count;
        offset += count;
        length -= static_cast<size_t>(count);
    }
}

void WriteAt(int fd, const void* buffer, size_t length, off_t offset) {
    const auto* bytes = static_cast<const uint8_t*>(buffer);
    while (length != 0) {
        const ssize_t count = pwrite(fd, bytes, length, offset);
        if (count < 0 && errno == EINTR) continue;
        ASSERT_GT(count, 0) << "offset=" << offset << ": " << strerror(errno);
        bytes += count;
        offset += count;
        length -= static_cast<size_t>(count);
    }
}

struct Geometry {
    uint32_t bytes_per_sector = 0;
    uint32_t sectors_per_cluster = 0;
    uint32_t reserved_sectors = 0;
    uint32_t table_count = 0;
    uint32_t table_sectors = 0;
    uint32_t clusters = 0;
    uint32_t backup_boot = 0;
    bool fat32 = false;
    uint64_t TableOffset(unsigned table) const {
        return uint64_t(reserved_sectors + table * table_sectors) * bytes_per_sector;
    }
    size_t TableBytes() const { return size_t(table_sectors) * bytes_per_sector; }
};

uint16_t Fat12Entry(const std::vector<uint8_t>& table, unsigned cluster) {
    const unsigned offset = cluster + cluster / 2;
    const uint16_t pair = Le16(table.data() + offset);
    return cluster & 1 ? pair >> 4 : pair & 0xfff;
}

void SetFat12Entry(std::vector<uint8_t>& table, unsigned cluster, uint16_t value) {
    const unsigned offset = cluster + cluster / 2;
    uint16_t pair = Le16(table.data() + offset);
    pair = cluster & 1 ? (pair & 0x000f) | (value << 4) : (pair & 0xf000) | value;
    table[offset] = pair & 0xff;
    table[offset + 1] = pair >> 8;
}

uint8_t PayloadByte(uint64_t offset) {
    return static_cast<uint8_t>((offset * 11 + (offset >> 7)) % 251);
}

class FatCopy {
public:
    ~FatCopy() {
        // The only mount and image manipulated here belong to our mkdtemp.
        if (mounted_ && umount(mount_.c_str()) != 0) {
            ADD_FAILURE() << "private FAT unmount: " << strerror(errno);
            return;
        }
        mounted_ = false;
        if (bound_ && ioctl(loop_.value, dunitest::kLoopClrFd, 0) != 0) {
            ADD_FAILURE() << "private FAT loop clear: " << strerror(errno);
            return;
        }
        if (!image_.empty()) unlink(image_.c_str());
        if (!mount_.empty()) rmdir(mount_.c_str());
        if (!root_.empty()) rmdir(root_.c_str());
    }

    void SetUp(const char* fixture) {
        char name[] = "/tmp/fat-table-durability-XXXXXX";
        const char* created = mkdtemp(name);
        ASSERT_NE(nullptr, created) << strerror(errno);
        root_ = created;
        image_ = root_ + "/volume.img";
        mount_ = root_ + "/mount";
        ASSERT_EQ(0, mkdir(mount_.c_str(), 0700));
        backing_.value = open(image_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
        ASSERT_GE(backing_.value, 0);
        ASSERT_NO_FATAL_FAILURE(dunitest::CopySparseFile(dunitest::FixturePath(fixture), backing_.value));
        std::array<uint8_t, 512> boot {};
        ASSERT_NO_FATAL_FAILURE(ReadAt(backing_.value, boot.data(), boot.size(), 0));
        ASSERT_EQ(0xaa55, Le16(boot.data() + 510));
        geometry.bytes_per_sector = Le16(boot.data() + 11);
        geometry.sectors_per_cluster = boot[13];
        geometry.reserved_sectors = Le16(boot.data() + 14);
        geometry.table_count = boot[16];
        geometry.table_sectors = Le16(boot.data() + 22);
        geometry.fat32 = geometry.table_sectors == 0;
        if (geometry.fat32) {
            geometry.table_sectors = Le32(boot.data() + 36);
            geometry.backup_boot = Le16(boot.data() + 50);
        }
        uint32_t total = Le16(boot.data() + 19);
        if (total == 0) total = Le32(boot.data() + 32);
        ASSERT_EQ(512U, geometry.bytes_per_sector) << "preserve fixed fixture sector geometry";
        ASSERT_GT(geometry.sectors_per_cluster, 0U);
        ASSERT_EQ(2U, geometry.table_count);
        const uint32_t root_sectors = (Le16(boot.data() + 17) * 32 + 511) / 512;
        geometry.clusters = (total - geometry.reserved_sectors -
                            geometry.table_count * geometry.table_sectors - root_sectors)
                           / geometry.sectors_per_cluster;
        Fd control {open("/dev/loop-control", O_RDWR)};
        ASSERT_GE(control.value, 0) << strerror(errno);
        const int minor = ioctl(control.value, dunitest::kLoopCtlGetFree, 0);
        ASSERT_GE(minor, 0) << strerror(errno);
        device_ = "/dev/loop" + std::to_string(minor);
        loop_.value = open(device_.c_str(), O_RDWR);
        ASSERT_GE(loop_.value, 0) << strerror(errno);
        ASSERT_EQ(0, ioctl(loop_.value, dunitest::kLoopSetFd, backing_.value)) << strerror(errno);
        bound_ = true;
    }

    int TryMount() {
        const int result = mount(device_.c_str(), mount_.c_str(), "vfat", 0, nullptr);
        if (result == 0) mounted_ = true;
        return result;
    }
    void Mount() { ASSERT_EQ(0, TryMount()) << strerror(errno); }
    void Unmount() {
        ASSERT_TRUE(mounted_);
        ASSERT_EQ(0, umount(mount_.c_str())) << strerror(errno);
        mounted_ = false;
        ASSERT_EQ(0, fsync(backing_.value)) << strerror(errno);
    }
    void Rebind() {
        ASSERT_FALSE(mounted_);
        ASSERT_TRUE(bound_);
        ASSERT_EQ(0, ioctl(loop_.value, dunitest::kLoopClrFd, 0)) << strerror(errno);
        bound_ = false;
        ASSERT_EQ(0, ioctl(loop_.value, dunitest::kLoopSetFd, backing_.value)) << strerror(errno);
        bound_ = true;
    }
    void ReadTable(unsigned index, std::vector<uint8_t>* result) {
        ASSERT_LT(index, geometry.table_count);
        result->resize(geometry.TableBytes());
        ASSERT_NO_FATAL_FAILURE(ReadAt(backing_.value, result->data(), result->size(),
                                      geometry.TableOffset(index)));
    }
    void WriteTable(unsigned index, const std::vector<uint8_t>& table) {
        ASSERT_EQ(geometry.TableBytes(), table.size());
        ASSERT_NO_FATAL_FAILURE(WriteAt(backing_.value, table.data(), table.size(),
                                       geometry.TableOffset(index)));
        ASSERT_EQ(0, fsync(backing_.value));
    }
    void SetFlags(uint16_t flags) {
        ASSERT_TRUE(geometry.fat32);
        const std::array<uint8_t, 2> bytes {uint8_t(flags), uint8_t(flags >> 8)};
        ASSERT_NO_FATAL_FAILURE(WriteAt(backing_.value, bytes.data(), bytes.size(), 40));
        if (geometry.backup_boot != 0) {
            ASSERT_NO_FATAL_FAILURE(WriteAt(backing_.value, bytes.data(), bytes.size(),
                uint64_t(geometry.backup_boot) * geometry.bytes_per_sector + 40));
        }
        ASSERT_EQ(0, fsync(backing_.value));
    }
    void WritePayload(uint64_t size) {
        Fd file {open((mount_ + "/DATA.BIN").c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(file.value, 0) << strerror(errno);
        std::array<uint8_t, 32 * 1024> data {};
        for (uint64_t offset = 0; offset < size;) {
            const size_t length = std::min<uint64_t>(data.size(), size - offset);
            for (size_t byte = 0; byte < length; ++byte) data[byte] = PayloadByte(offset + byte);
            ASSERT_NO_FATAL_FAILURE(WriteAt(file.value, data.data(), length, offset));
            offset += length;
        }
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
        Fd directory {open(mount_.c_str(), O_RDONLY | O_DIRECTORY)};
        ASSERT_GE(directory.value, 0);
        ASSERT_EQ(0, fsync(directory.value)) << strerror(errno);
    }
    void VerifyPayload(uint64_t size) {
        Fd file {open((mount_ + "/DATA.BIN").c_str(), O_RDONLY)};
        ASSERT_GE(file.value, 0) << strerror(errno);
        struct stat st {};
        ASSERT_EQ(0, fstat(file.value, &st));
        ASSERT_EQ(static_cast<off_t>(size), st.st_size);
        std::array<uint8_t, 32 * 1024> data {};
        for (uint64_t offset = 0; offset < size;) {
            const size_t length = std::min<uint64_t>(data.size(), size - offset);
            ASSERT_NO_FATAL_FAILURE(ReadAt(file.value, data.data(), length, offset));
            for (size_t byte = 0; byte < length; ++byte) {
                ASSERT_EQ(PayloadByte(offset + byte), data[byte]) << "offset=" << offset + byte;
            }
            offset += length;
        }
    }
    Geometry geometry;
private:
    Fd backing_;
    Fd loop_;
    bool mounted_ = false;
    bool bound_ = false;
    std::string root_;
    std::string image_;
    std::string mount_;
    std::string device_;
};

class FatTableMirrors : public testing::TestWithParam<const char*> {};

TEST_P(FatTableMirrors, FsyncUnmountAndRemountUpdateBothCopies) {
    FatCopy fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp(GetParam()));
    std::vector<uint8_t> before, first, second;
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(0, &before));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.WritePayload(200 * 1024));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(0, &first));
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(1, &second));
    EXPECT_NE(before, first) << "allocation must change a FAT table";
    EXPECT_EQ(first, second) << "FAT mirror must survive complete sync and unmount";
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.VerifyPayload(200 * 1024));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

INSTANTIATE_TEST_SUITE_P(FixedFixtures, FatTableMirrors, testing::Values(
    "fat12_directory.img", "fat_stat_blocks.img", "fat32_stat_blocks.img"));

TEST(FatTableDurability, Fat12PackedEntry341CrossesSectorAndPreservesNeighbor) {
    FatCopy fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("fat12_directory.img"));
    ASSERT_EQ(1U, fs.geometry.sectors_per_cluster);
    ASSERT_GE(fs.geometry.clusters, 655U);
    std::vector<uint8_t> before, first, second;
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(0, &before));
    ASSERT_LT((fs.geometry.clusters + 1) * 3 / 2 + 1, before.size());
    // A legal private bad-cluster map forces the 200KiB payload to consume all
    // 400 free clusters (256..655). Entry341 must be used and its successor or
    // EOC has a nonzero high byte; this does not depend on allocator ordering.
    for (unsigned cluster = 2; cluster < fs.geometry.clusters + 2; ++cluster) {
        SetFat12Entry(before, cluster, cluster >= 256 && cluster <= 655 ? 0 : 0xff7);
    }
    ASSERT_NO_FATAL_FAILURE(fs.WriteTable(0, before));
    ASSERT_NO_FATAL_FAILURE(fs.WriteTable(1, before));
    ASSERT_EQ(0U, Fat12Entry(before, 340));
    ASSERT_EQ(0U, Fat12Entry(before, 341));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.WritePayload(200 * 1024));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(0, &first));
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(1, &second));
    ASSERT_EQ(first, second);
    EXPECT_GE(Fat12Entry(first, 340), 256U);
    EXPECT_GE(Fat12Entry(first, 341), 256U);
    EXPECT_NE(before[511], first[511]);
    EXPECT_NE(before[512], first[512]);
    EXPECT_EQ(0xff7U, Fat12Entry(first, 255));
    EXPECT_EQ(0xff7U, Fat12Entry(first, 656));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.VerifyPayload(200 * 1024));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(FatTableDurability, Fat32DisabledMirroringOnlyUpdatesActiveTableOne) {
    FatCopy fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("fat32_stat_blocks.img"));
    ASSERT_TRUE(fs.geometry.fat32);
    ASSERT_EQ(1U, fs.geometry.sectors_per_cluster);
    ASSERT_NO_FATAL_FAILURE(fs.SetFlags(0x81));
    std::vector<uint8_t> inactive_before, active_before, inactive_after, active_after;
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(0, &inactive_before));
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(1, &active_before));
    // The inactive table may contain stale data. Give it a distinct marker and
    // require byte-for-byte preservation, not merely equality with table1.
    inactive_before[12] = 0xf7;
    inactive_before[13] = 0xff;
    inactive_before[14] = 0xff;
    inactive_before[15] = 0x0f;
    ASSERT_NO_FATAL_FAILURE(fs.WriteTable(0, inactive_before));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.WritePayload(200 * 1024));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(0, &inactive_after));
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(1, &active_after));
    EXPECT_EQ(inactive_before, inactive_after);
    EXPECT_NE(active_before, active_after);
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.VerifyPayload(200 * 1024));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

TEST(FatTableDurability, Fat32RejectsOutOfRangeActiveTable) {
    FatCopy fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("fat32_stat_blocks.img"));
    ASSERT_NO_FATAL_FAILURE(fs.SetFlags(0x82));  // Two tables, valid indices0/1.
    errno = 0;
    EXPECT_EQ(-1, fs.TryMount());
    EXPECT_EQ(EINVAL, errno);
}

TEST(FatTableDurability, Fat12NearLimitNextLinksSurviveColdRemount) {
    FatCopy fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("fat12_near_limit.img"));
    ASSERT_FALSE(fs.geometry.fat32);
    ASSERT_EQ(1U, fs.geometry.sectors_per_cluster);
    ASSERT_LE(fs.geometry.clusters, 0xff4U);
    const unsigned last = fs.geometry.clusters + 1;
    ASSERT_GE(last, 0xff1U);
    ASSERT_LT(last, 0xff7U);
    std::vector<uint8_t> initial, first, second;
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(0, &initial));
    ASSERT_LT(last * 3 / 2 + 1, initial.size());
    // Only the final four legal entries (0xfef..0xff2 in the fixed
    // fixture) are free. Three allocated clusters necessarily include a
    // next link in 0xff0..0xff2, not a BAD/EOC marker. Linux treats these
    // pointers as valid when they are within the volume's cluster bound.
    for (unsigned cluster = 2; cluster <= last; ++cluster) {
        SetFat12Entry(initial, cluster, cluster >= 0xfef ? 0 : 0xff7);
    }
    ASSERT_NO_FATAL_FAILURE(fs.WriteTable(0, initial));
    ASSERT_NO_FATAL_FAILURE(fs.WriteTable(1, initial));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.WritePayload(3 * 512));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(0, &first));
    ASSERT_NO_FATAL_FAILURE(fs.ReadTable(1, &second));
    ASSERT_EQ(first, second);
    bool has_near_limit_link = false;
    unsigned allocated = 0;
    for (unsigned cluster = 0xfef; cluster <= last; ++cluster) {
        const unsigned entry = Fat12Entry(first, cluster);
        if (entry != 0) ++allocated;
        if (entry >= 0xff0 && entry <= last) has_near_limit_link = true;
    }
    ASSERT_EQ(3U, allocated);
    ASSERT_TRUE(has_near_limit_link);
    // Rebind advances loop generation and requires the old mount holder to
    // have gone away. Re-reading the cold FAT cannot use cached decoded links.
    ASSERT_NO_FATAL_FAILURE(fs.Rebind());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_NO_FATAL_FAILURE(fs.VerifyPayload(3 * 512));
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
