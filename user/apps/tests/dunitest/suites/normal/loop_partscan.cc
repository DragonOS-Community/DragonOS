#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"
#include <array>
#include <cerrno>
#include <cstdint>
#include <cstring>
#include <cstdlib>
#include <fcntl.h>
#include <linux/fs.h>
#include <linux/loop.h>
#include <string>
#include <sys/ioctl.h>
#include <sys/stat.h>
#include <sys/mount.h>
#include <sys/statfs.h>
#include <unistd.h>
#include <vector>

namespace {
struct Fd {
    int value = -1;
    ~Fd() { if (value >= 0) close(value); }
    void Reset(int fd = -1) { if (value >= 0) close(value); value = fd; }
};
void Le32(unsigned char* p, uint32_t value) {
    for (unsigned i = 0; i != 4; ++i) p[i] = value >> (8 * i);
}
void Le64(unsigned char* p, uint64_t value) {
    for (unsigned i = 0; i != 8; ++i) p[i] = value >> (8 * i);
}
uint32_t Crc32(const unsigned char* bytes, size_t size) {
    uint32_t crc = ~0U;
    for (size_t i = 0; i < size; ++i) {
        crc ^= bytes[i];
        for (unsigned bit = 0; bit != 8; ++bit) crc = (crc >> 1) ^ (0xedb88320U & (0U - (crc & 1)));
    }
    return ~crc;
}
class LoopPartscan : public testing::Test {
protected:
    static constexpr uint64_t kSectors = 8192;
    void SetUp() override {
        const char* dir = getenv("DUNITEST_LOOP_BACKING_DIR");
        std::string pattern = std::string(dir ? dir : "/tmp") + "/loop_partscan_XXXXXX";
        std::vector<char> path(pattern.begin(), pattern.end()); path.push_back(0);
        backing_.value = mkstemp(path.data());
        ASSERT_GE(backing_.value, 0) << strerror(errno);
        backing_path_ = path.data();
        ASSERT_EQ(0, ftruncate(backing_.value, kSectors * 512));
        ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 8, 16));
        Fd control {open("/dev/loop-control", O_RDWR)};
        ASSERT_GE(control.value, 0);
        const int minor = ioctl(control.value, LOOP_CTL_GET_FREE, 0);
        ASSERT_GE(minor, 0);
        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_.value = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(loop_.value, 0) << strerror(errno);
    }
    void TearDown() override {
        if (bound_) { EXPECT_EQ(0, ioctl(loop_.value, LOOP_CLR_FD, 0)) << strerror(errno); }
        loop_.Reset(); backing_.Reset();
        if (!backing_path_.empty()) { EXPECT_EQ(0, unlink(backing_path_.c_str())); }
        if (!second_path_.empty()) { EXPECT_EQ(0, unlink(second_path_.c_str())); }
    }
    void WriteMbr(int fd, uint32_t start, uint32_t count, unsigned slot = 0, uint64_t base = 0) {
        std::array<unsigned char, 512> sector {};
        unsigned char* entry = sector.data() + 446 + slot * 16;
        // Linux uses LBA fields; valid saturated CHS also exercises DragonOS's
        // existing MBR parser without depending on CHS interpretation.
        entry[1] = entry[5] = 0xfe; entry[2] = entry[6] = 0xff; entry[3] = entry[7] = 0xff;
        entry[4] = 0x83;
        Le32(entry + 8, start); Le32(entry + 12, count);
        sector[510] = 0x55; sector[511] = 0xaa;
        ASSERT_EQ(512, pwrite(fd, sector.data(), sector.size(), base));
        ASSERT_EQ(0, fsync(fd));
    }
    void Bind(bool readonly = false) {
        Fd ro;
        int fd = backing_.value;
        if (readonly) { ro.value = open(backing_path_.c_str(), O_RDONLY); ASSERT_GE(ro.value, 0); fd = ro.value; }
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_FD, fd)) << strerror(errno);
        bound_ = true;
    }
    void Flags(unsigned flags) {
        loop_info64 status {};
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_GET_STATUS64, &status));
        status.lo_flags = flags;
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_STATUS64, &status)) << strerror(errno);
    }
    void ClearAndWait() {
        for (unsigned attempt = 0; attempt != 100; ++attempt) {
            errno = 0;
            const int clear = ioctl(loop_.value, LOOP_CLR_FD, 0);
            ASSERT_TRUE(clear == 0 || errno == ENXIO) << strerror(errno);
            loop_info64 status {};
            errno = 0;
            const int get = ioctl(loop_.value, LOOP_GET_STATUS64, &status);
            if (get == -1 && errno == ENXIO) { bound_ = false; return; }
            ASSERT_EQ(0, get);
            ASSERT_NE(0U, status.lo_flags & LO_FLAGS_AUTOCLEAR);
            usleep(1000);
        }
        FAIL() << "partition probe did not release its final reference";
    }
    void ExpectFlag(bool expected) {
        loop_info64 status {};
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_GET_STATUS64, &status));
        EXPECT_EQ(expected, !!(status.lo_flags & LO_FLAGS_PARTSCAN));
    }
    std::string Part(unsigned index) { return loop_path_ + "p" + std::to_string(index); }
    void ExpectCapacity(const std::string& path, uint64_t bytes) {
        Fd fd {open(path.c_str(), O_RDONLY)};
        ASSERT_GE(fd.value, 0) << path << ": " << strerror(errno);
        uint64_t size = UINT64_MAX;
        ASSERT_EQ(0, ioctl(fd.value, BLKGETSIZE64, &size));
        EXPECT_EQ(bytes, size);
    }
    void ExpectAbsent(const std::string& path) {
        // devtmpfs deletion can finish after the loop has reached Unbound.
        // Observe namespace removal without treating that worker latency as a
        // different partition lifecycle or changing the device ourselves.
        struct stat metadata {};
        for (unsigned attempt = 0; attempt != 100; ++attempt) {
            errno = 0;
            if (stat(path.c_str(), &metadata) == -1 && errno == ENOENT) return;
            usleep(1000);
        }
        FAIL() << "partition node still published: " << path;
    }
    void Replacement(uint32_t start, uint32_t count, Fd* fd) {
        std::string pattern = backing_path_ + "_new_XXXXXX";
        std::vector<char> path(pattern.begin(), pattern.end()); path.push_back(0);
        fd->value = mkstemp(path.data()); ASSERT_GE(fd->value, 0);
        second_path_ = path.data();
        ASSERT_EQ(0, ftruncate(fd->value, kSectors * 512));
        ASSERT_NO_FATAL_FAILURE(WriteMbr(fd->value, start, count));
    }
    void WriteGpt(uint64_t sectors = kSectors, uint32_t count = 128,
                  uint64_t start = 2048, uint64_t end = 4095, bool boundary_entries = false, uint32_t logical = 512) {
        ASSERT_EQ(0, ftruncate(backing_.value, sectors * logical));
        std::array<unsigned char, 512> mbr {};
        mbr[446 + 4] = 0xee; Le32(mbr.data() + 446 + 8, 1);
        Le32(mbr.data() + 446 + 12, sectors - 1);
        mbr[510] = 0x55; mbr[511] = 0xaa;
        ASSERT_EQ(512, pwrite(backing_.value, mbr.data(), mbr.size(), 0));
        std::vector<unsigned char> entries(size_t(count) * 128, 0);
        const unsigned char linux_type[] = {0xaf,0x3d,0xc6,0x0f,0x83,0x84,0x72,0x47,0x8e,0x79,0x3d,0x69,0xd8,0x47,0x7d,0xe4};
        for (unsigned index : {0U, 254U, 255U}) {
            if (index != 0 && !boundary_entries) continue;
            ASSERT_LT(index, count);
            unsigned char* entry = entries.data() + index * 128;
            memcpy(entry, linux_type, 16); entry[16] = index + 1;
            Le64(entry + 32, start); Le64(entry + 40, end);
        }
        const uint64_t table_sectors = (entries.size() + logical - 1) / logical;
        const uint64_t backup_table = sectors - 1 - table_sectors;
        ASSERT_EQ(static_cast<ssize_t>(entries.size()), pwrite(backing_.value, entries.data(), entries.size(), 2 * logical));
        ASSERT_EQ(static_cast<ssize_t>(entries.size()), pwrite(backing_.value, entries.data(), entries.size(), backup_table * logical));
        for (bool backup : {false, true}) {
            std::vector<unsigned char> header(logical, 0);
            memcpy(header.data(), "EFI PART", 8); Le32(header.data() + 8, 0x10000); Le32(header.data() + 12, 92);
            Le64(header.data() + 24, backup ? sectors - 1 : 1); Le64(header.data() + 32, backup ? 1 : sectors - 1);
            Le64(header.data() + 40, table_sectors + 2); Le64(header.data() + 48, backup_table - 1); header[56] = 2;
            Le64(header.data() + 72, backup ? backup_table : 2); Le32(header.data() + 80, count); Le32(header.data() + 84, 128);
            Le32(header.data() + 88, Crc32(entries.data(), entries.size())); Le32(header.data() + 16, Crc32(header.data(), 92));
            ASSERT_EQ(logical, pwrite(backing_.value, header.data(), header.size(), (backup ? sectors - 1 : 1) * logical));
        }
        ASSERT_EQ(0, fsync(backing_.value));
    }
    void Corrupt(uint64_t offset) {
        unsigned char byte = 0;
        ASSERT_EQ(1, pread(backing_.value, &byte, 1, offset));
        byte ^= 0x01;
        ASSERT_EQ(1, pwrite(backing_.value, &byte, 1, offset));
        ASSERT_EQ(0, fsync(backing_.value));
    }
    Fd backing_, loop_;
    std::string backing_path_, second_path_, loop_path_;
    bool bound_ = false;
};

TEST_F(LoopPartscan, ConfigureLargeLogicalMbrUsesLogicalSectorUnits) {
    loop_config config {}; config.fd = backing_.value;
    config.block_size = 4096; config.info.lo_flags = LO_FLAGS_PARTSCAN;
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_CONFIGURE, &config)) << strerror(errno);
    bound_ = true;
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 4096));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_BLOCK_SIZE, 512));
    // Changing queue geometry alone never reinterprets the published table.
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 4096));
}

TEST_F(LoopPartscan, ConfigureLargeLogicalGptReadsHeadersAndEntriesAtLogicalOffsets) {
    ASSERT_NO_FATAL_FAILURE(WriteGpt(1024, 128, 64, 127, false, 4096));
    loop_config config {}; config.fd = backing_.value;
    config.block_size = 4096; config.info.lo_flags = LO_FLAGS_PARTSCAN;
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_CONFIGURE, &config)) << strerror(errno);
    bound_ = true;
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 64 * 4096));
}

TEST_F(LoopPartscan, StickyStatusPublishesSlotNumberAndClearRemovesNodes) {
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 64, 8, 2));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(3)));
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectFlag(true));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(3), 8 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(1)));
    ASSERT_NO_FATAL_FAILURE(Flags(0));
    ASSERT_NO_FATAL_FAILURE(ExpectFlag(true));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(3), 8 * 512));
    ASSERT_NO_FATAL_FAILURE(ClearAndWait());
    loop_info64 status {}; errno = 0;
    EXPECT_EQ(-1, ioctl(loop_.value, LOOP_GET_STATUS64, &status)); EXPECT_EQ(ENXIO, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(3)));
}

TEST_F(LoopPartscan, LegacyStatusAlsoEnablesStickyPartscan) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    loop_info status {}; status.lo_flags = LO_FLAGS_PARTSCAN;
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_STATUS, &status));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
    status.lo_flags = 0;
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_STATUS, &status));
    ASSERT_NO_FATAL_FAILURE(ExpectFlag(true));
}

TEST_F(LoopPartscan, ConfigurePartscanScansBeforeReturning) {
    loop_config config {}; config.fd = backing_.value; config.info.lo_flags = LO_FLAGS_PARTSCAN;
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_CONFIGURE, &config)) << strerror(errno);
    bound_ = true;
    ASSERT_NO_FATAL_FAILURE(ExpectFlag(true));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
}

TEST_F(LoopPartscan, RepeatedFlagAndMappingChangeDoNotImplicitlyRescan) {
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 64, 8, 0, 4096));
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 8, 16));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_GET_STATUS64, &status));
    status.lo_offset = 4096;
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_STATUS64, &status));
    ASSERT_NO_FATAL_FAILURE(ExpectFlag(true));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
}

TEST_F(LoopPartscan, SetCapacityChangesWholeDiskButDoesNotRescanPartitionGeometry) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 64, 8));
    ASSERT_EQ(0, ftruncate(backing_.value, 2 * kSectors * 512));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_CAPACITY, 0));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(loop_path_, 2 * kSectors * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
}

TEST_F(LoopPartscan, ChangeFdRescansReadOnlyMapping) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    // Do not open the old partition here: deferred fput would make this a
    // busy-rescan case, covered separately below.
    struct stat old_node {}; ASSERT_EQ(0, stat(Part(1).c_str(), &old_node));
    Fd replacement;
    ASSERT_NO_FATAL_FAILURE(Replacement(64, 8, &replacement));
    Fd readonly {open(second_path_.c_str(), O_RDONLY)}; ASSERT_GE(readonly.value, 0);
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_CHANGE_FD, readonly.value)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 8 * 512));
}

TEST_F(LoopPartscan, BusyPartitionKeepsOldGeometryWhenChangeFdScanFails) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    Fd partition {open(Part(1).c_str(), O_RDONLY)}; ASSERT_GE(partition.value, 0);
    Fd replacement;
    ASSERT_NO_FATAL_FAILURE(Replacement(64, 8, &replacement));
    unsigned char old_range_data = 0x8a;
    ASSERT_EQ(1, pwrite(replacement.value, &old_range_data, 1, 8 * 512));
    ASSERT_EQ(0, fsync(replacement.value));
    Fd readonly {open(second_path_.c_str(), O_RDONLY)}; ASSERT_GE(readonly.value, 0);
    // The mapping change succeeds even though the partition rescan is busy.
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_CHANGE_FD, readonly.value)) << strerror(errno);
    uint64_t size = 0;
    ASSERT_EQ(0, ioctl(partition.value, BLKGETSIZE64, &size)); EXPECT_EQ(16 * 512U, size);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
    // The retained old partition keeps its old LBA range, while its I/O uses
    // the newly installed whole-disk mapping rather than the discarded file.
    unsigned char observed = 0;
    ASSERT_EQ(1, pread(partition.value, &observed, 1, 0));
    EXPECT_EQ(old_range_data, observed);
    partition.Reset();
    // Repeating PARTSCAN does not retry a failed scan.
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
    // Linux may defer the final fput after close. Retry only the explicit
    // operation that is documented to rescan, until the old opener is gone.
    bool rescanned = false;
    for (unsigned attempt = 0; attempt != 100; ++attempt) {
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_CHANGE_FD, readonly.value));
        {
            Fd probe {open(Part(1).c_str(), O_RDONLY)}; ASSERT_GE(probe.value, 0);
            ASSERT_EQ(0, ioctl(probe.value, BLKGETSIZE64, &size));
        }
        if (size == 8 * 512) { rescanned = true; break; }
        EXPECT_EQ(16 * 512U, size);
        usleep(1000);
    }
    EXPECT_TRUE(rescanned);
}

TEST_F(LoopPartscan, PartitionOpenerDefersClearAndLastCloseDropsNodes) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    Fd partition {open(Part(1).c_str(), O_RDONLY)}; ASSERT_GE(partition.value, 0);
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_CLR_FD, 0));
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_GET_STATUS64, &status));
    EXPECT_NE(0U, status.lo_flags & LO_FLAGS_AUTOCLEAR);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
    loop_.Reset(); bound_ = false;
    std::array<unsigned char, 512> bytes {};
    EXPECT_EQ(512, pread(partition.value, bytes.data(), bytes.size(), 0));
    partition.Reset();
    // Only observe completion; never clear another mapping or remove a device.
    for (unsigned attempt = 0; attempt < 100; ++attempt) {
        loop_.Reset(open(loop_path_.c_str(), O_RDWR)); ASSERT_GE(loop_.value, 0);
        errno = 0;
        if (ioctl(loop_.value, LOOP_GET_STATUS64, &status) == -1 && errno == ENXIO) break;
        loop_.Reset(); usleep(1000);
    }
    errno = 0; EXPECT_EQ(-1, ioctl(loop_.value, LOOP_GET_STATUS64, &status)); EXPECT_EQ(ENXIO, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(1)));
}

TEST_F(LoopPartscan, MountedPartitionPinsMappingUntilUnmount) {
    Fd fixture {open(dunitest::FixturePath().c_str(), O_RDONLY)};
    ASSERT_GE(fixture.value, 0) << strerror(errno);
    struct stat metadata {}; ASSERT_EQ(0, fstat(fixture.value, &metadata));
    ASSERT_EQ(0, metadata.st_size % 512);
    const uint64_t start = 2048;
    ASSERT_EQ(0, ftruncate(backing_.value, start * 512 + metadata.st_size));
    std::array<unsigned char, 65536> bytes {};
    for (off_t offset = 0; offset < metadata.st_size;) {
        ssize_t count = pread(fixture.value, bytes.data(), bytes.size(), offset);
        ASSERT_GT(count, 0);
        ASSERT_EQ(count, pwrite(backing_.value, bytes.data(), count, start * 512 + offset));
        offset += count;
    }
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, start, metadata.st_size / 512));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    struct Mount {
        std::string path;
        bool mounted = false;
        ~Mount() {
            if (mounted) { EXPECT_EQ(0, umount2(path.c_str(), MNT_DETACH)); }
            if (!path.empty()) { EXPECT_EQ(0, rmdir(path.c_str())); }
        }
    } owner;
    char directory[] = "/tmp/loop_partscan_mount_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(directory)); owner.path = directory;
    ASSERT_EQ(0, mount(Part(1).c_str(), owner.path.c_str(), "ext4", MS_RDONLY, nullptr)) << strerror(errno);
    owner.mounted = true;
    for (unsigned size : {512U, 4096U, 768U}) {
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_.value, LOOP_SET_BLOCK_SIZE, size));
        EXPECT_EQ(EBUSY, errno) << "mounted partition claim precedes size validation";
    }
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_CLR_FD, 0));
    loop_info64 status {}; ASSERT_EQ(0, ioctl(loop_.value, LOOP_GET_STATUS64, &status));
    EXPECT_NE(0U, status.lo_flags & LO_FLAGS_AUTOCLEAR);
    loop_.Reset(); bound_ = false;
    struct statfs filesystem {}; ASSERT_EQ(0, statfs(owner.path.c_str(), &filesystem));
    EXPECT_EQ(0xef53, filesystem.f_type);
    ASSERT_EQ(0, umount(owner.path.c_str())) << strerror(errno); owner.mounted = false;
    for (unsigned attempt = 0; attempt != 100; ++attempt) {
        loop_.Reset(open(loop_path_.c_str(), O_RDWR)); ASSERT_GE(loop_.value, 0);
        errno = 0;
        if (ioctl(loop_.value, LOOP_GET_STATUS64, &status) == -1 && errno == ENXIO) break;
        loop_.Reset(); usleep(1000);
    }
    errno = 0; EXPECT_EQ(-1, ioctl(loop_.value, LOOP_GET_STATUS64, &status)); EXPECT_EQ(ENXIO, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(1)));
}

TEST_F(LoopPartscan, MissingSignatureIsSuccessfulScanWithoutPartitions) {
    std::array<unsigned char, 512> zero {};
    ASSERT_EQ(512, pwrite(backing_.value, zero.data(), zero.size(), 0)); ASSERT_EQ(0, fsync(backing_.value));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectFlag(true));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(1)));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(loop_path_, kSectors * 512));
}

TEST_F(LoopPartscan, GptUsesRealEntriesInsteadOfPublishingProtectiveMbr) {
    ASSERT_NO_FATAL_FAILURE(WriteGpt());
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 2048 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(2)));
}

TEST_F(LoopPartscan, GptDamagedPrimaryHeaderDoesNotTrustBackupByDefault) {
    ASSERT_NO_FATAL_FAILURE(WriteGpt());
    ASSERT_NO_FATAL_FAILURE(Corrupt(512 + 16));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(1)));
}

TEST_F(LoopPartscan, GptDamagedPrimaryEntryArrayDoesNotTrustBackupByDefault) {
    ASSERT_NO_FATAL_FAILURE(WriteGpt());
    ASSERT_NO_FATAL_FAILURE(Corrupt(2 * 512 + 40));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(1)));
}

TEST_F(LoopPartscan, GptValidPrimarySurvivesDamagedBackup) {
    ASSERT_NO_FATAL_FAILURE(WriteGpt());
    ASSERT_NO_FATAL_FAILURE(Corrupt((kSectors - 1) * 512 + 16));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 2048 * 512));
}

TEST_F(LoopPartscan, GptBothInvalidCopiesNeverPublishProtectiveMbr) {
    ASSERT_NO_FATAL_FAILURE(WriteGpt());
    ASSERT_NO_FATAL_FAILURE(Corrupt(512 + 16));
    ASSERT_NO_FATAL_FAILURE(Corrupt((kSectors - 1) * 512 + 16));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectFlag(true));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(1)));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(loop_path_, kSectors * 512));
}

TEST_F(LoopPartscan, GptMaximumTableChecksumsAllEntriesAndBoundsPublishedMinors) {
    // Linux 6.6 efi.c caps table bytes at KMALLOC_MAX_SIZE, 4 MiB on x86_64.
    // The parser must checksum entries beyond publishable minor255 as well.
    ASSERT_NO_FATAL_FAILURE(WriteGpt(24576, 32768, 9000, 9015, true));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 16 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(255), 16 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(256)));
}

TEST_F(LoopPartscan, GptOversizedTableIsRejectedWithoutProtectivePartition) {
    ASSERT_NO_FATAL_FAILURE(WriteGpt(24576, 32769, 9000, 9015));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectFlag(true));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(1)));
}

TEST_F(LoopPartscan, ExtendedMbrUsesRelativeLinksAndLogicalPartitionNumbers) {
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 2048, 2048));
    unsigned char kind = 0x0f;
    ASSERT_EQ(1, pwrite(backing_.value, &kind, 1, 446 + 4));
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 1, 16, 0, 2048 * 512));
    std::array<unsigned char, 16> link {};
    link[4] = 0x0f; Le32(link.data() + 8, 64); Le32(link.data() + 12, 1984);
    ASSERT_EQ(16, pwrite(backing_.value, link.data(), link.size(), 2048 * 512 + 462));
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 1, 8, 0, 2112 * 512));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 2 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(5), 16 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(6), 8 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(2)));
}

TEST_F(LoopPartscan, ExtendedMbrEmptyCyclicLinkTerminatesWithoutFakeLogicalPartitions) {
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, 2048, 2048));
    unsigned char kind = 0x0f;
    ASSERT_EQ(1, pwrite(backing_.value, &kind, 1, 446 + 4));
    std::array<unsigned char, 512> ebr {};
    ebr[462 + 4] = 0x0f;
    // The next link is relative to the extended base: zero loops to this EBR.
    Le32(ebr.data() + 462 + 8, 0); Le32(ebr.data() + 462 + 12, 2048);
    ebr[510] = 0x55; ebr[511] = 0xaa;
    ASSERT_EQ(512, pwrite(backing_.value, ebr.data(), ebr.size(), 2048 * 512));
    ASSERT_EQ(0, fsync(backing_.value));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 2 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectAbsent(Part(5)));
}

TEST_F(LoopPartscan, MbrLbaEntryWithZeroChsIsClippedAtPublishedEnd) {
    ASSERT_NO_FATAL_FAILURE(WriteMbr(backing_.value, kSectors - 8, 32));
    std::array<unsigned char, 3> zero {};
    ASSERT_EQ(3, pwrite(backing_.value, zero.data(), zero.size(), 446 + 1));
    ASSERT_EQ(3, pwrite(backing_.value, zero.data(), zero.size(), 446 + 5));
    ASSERT_EQ(0, fsync(backing_.value));
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(Flags(LO_FLAGS_PARTSCAN));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(Part(1), 8 * 512));
}
} // namespace
int main(int argc, char** argv) { testing::InitGoogleTest(&argc, argv); return RUN_ALL_TESTS(); }
