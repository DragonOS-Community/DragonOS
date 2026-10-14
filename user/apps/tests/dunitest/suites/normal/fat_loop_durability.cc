#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <time.h>
#include <unistd.h>

#include <algorithm>
#include <array>
#include <cstdint>
#include <string>
#include <vector>

namespace {

constexpr uint64_t kMiB = 1024 * 1024;
constexpr long kFatMagic = 0x4d44;

struct Fd {
    int value = -1;
    ~Fd() { if (value >= 0) close(value); }
    void Close() {
        ASSERT_GE(value, 0);
        ASSERT_EQ(0, close(value)) << strerror(errno);
        value = -1;
    }
};

uint64_t Now() {
    timespec value {};
    if (clock_gettime(CLOCK_MONOTONIC, &value) != 0) return 0;
    return uint64_t(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}

void PrintBlockSnapshot(const char* phase, const char* stage) {
    Fd file {open("/sys/kernel/debug/block/virtio_blk_stats", O_RDONLY)};
    if (file.value < 0) return;  // Optional instrumentation, not a CI prerequisite.
    std::string report;
    std::array<char, 4096> buffer {};
    while (report.size() < 64 * 1024) {
        const ssize_t count = read(file.value, buffer.data(), buffer.size());
        if (count <= 0) break;
        report.append(buffer.data(), static_cast<size_t>(count));
    }
    size_t begin = 0;
    while (begin < report.size()) {
        const size_t end = report.find('\n', begin);
        const std::string line = report.substr(begin, end == std::string::npos ? end : end - begin);
        if (line.compare(0, 11, "device=vdb ") == 0) {
            printf("[ FAT-LOOP ] stats %s %s %s\n", phase, stage, line.c_str());
        }
        if (end == std::string::npos) break;
        begin = end + 1;
    }
}

// Stage boundaries allow the external snapshot harness to separate preparation,
// allocation, data I/O, filesystem sync, mount and journal teardown stalls.
class Stage {
public:
    explicit Stage(const char* name) : name_(name), start_(Now()) {
        printf("[ FAT-LOOP ] begin %s\n", name_);
        PrintBlockSnapshot("begin", name_);
        fflush(stdout);
    }
    ~Stage() {
        printf("[ FAT-LOOP ] end %s elapsed_us=%llu\n", name_,
               static_cast<unsigned long long>((Now() - start_) / 1000));
        PrintBlockSnapshot("end", name_);
        fflush(stdout);
    }
private:
    const char* name_;
    uint64_t start_;
};

std::string MakeDirectory(const std::string& parent, const char* prefix) {
    std::string name = parent + "/" + prefix + "XXXXXX";
    std::vector<char> buffer(name.begin(), name.end());
    buffer.push_back('\0');
    if (!mkdtemp(buffer.data())) return {};
    return buffer.data();
}

uint8_t Pattern(uint64_t offset) {
    return static_cast<uint8_t>((offset * 37 + (offset >> 9) * 11) % 251);
}

void WritePattern(int fd, uint64_t offset, uint64_t length) {
    ASSERT_EQ(static_cast<off_t>(offset), lseek(fd, offset, SEEK_SET)) << strerror(errno);
    std::array<uint8_t, 64 * 1024> data {};
    while (length != 0) {
        const size_t chunk = static_cast<size_t>(std::min<uint64_t>(length, data.size()));
        for (size_t index = 0; index < chunk; ++index) data[index] = Pattern(offset + index);
        size_t done = 0;
        while (done != chunk) {
            const ssize_t count = write(fd, data.data() + done, chunk - done);
            if (count < 0 && errno == EINTR) continue;
            ASSERT_GT(count, 0) << strerror(errno);
            done += static_cast<size_t>(count);
        }
        offset += chunk;
        length -= chunk;
    }
}

void VerifyPattern(const std::string& path, uint64_t length) {
    Fd fd {open(path.c_str(), O_RDONLY)};
    ASSERT_GE(fd.value, 0) << path << ": " << strerror(errno);
    struct stat st {};
    ASSERT_EQ(0, fstat(fd.value, &st)) << strerror(errno);
    ASSERT_EQ(static_cast<off_t>(length), st.st_size);
    std::array<uint8_t, 64 * 1024> data {};
    for (uint64_t offset = 0; offset < length;) {
        const size_t chunk = static_cast<size_t>(std::min<uint64_t>(length - offset, data.size()));
        size_t done = 0;
        while (done != chunk) {
            const ssize_t count = pread(fd.value, data.data() + done, chunk - done, offset + done);
            if (count < 0 && errno == EINTR) continue;
            ASSERT_GT(count, 0) << strerror(errno);
            done += static_cast<size_t>(count);
        }
        for (size_t index = 0; index < chunk; ++index) {
            ASSERT_EQ(Pattern(offset + index), data[index]) << "offset=" << offset + index;
        }
        offset += chunk;
    }
    ASSERT_NO_FATAL_FAILURE(fd.Close());
}

void SyncDirectory(const std::string& path) {
    Fd dir {open(path.c_str(), O_RDONLY | O_DIRECTORY)};
    ASSERT_GE(dir.value, 0) << path << ": " << strerror(errno);
    // Linux FAT directories use fat_file_fsync, including the device flush.
    ASSERT_EQ(0, fsync(dir.value)) << path << ": " << strerror(errno);
}

class PrivateLoop {
public:
    ~PrivateLoop() { Close(); }

    void Bind(const std::string& image, const std::string& mount_point) {
        ASSERT_FALSE(bound_);
        image_ = image;
        mount_point_ = mount_point;
        Fd control {open("/dev/loop-control", O_RDWR)};
        ASSERT_GE(control.value, 0) << strerror(errno);
        const int minor = ioctl(control.value, dunitest::kLoopCtlGetFree, 0);
        ASSERT_GE(minor, 0) << strerror(errno);
        device_ = "/dev/loop" + std::to_string(minor);
        loop_.value = open(device_.c_str(), O_RDWR);
        ASSERT_GE(loop_.value, 0) << device_ << ": " << strerror(errno);
        backing_.value = open(image_.c_str(), O_RDWR);
        ASSERT_GE(backing_.value, 0) << strerror(errno);
        ASSERT_EQ(0, ioctl(loop_.value, dunitest::kLoopSetFd, backing_.value)) << strerror(errno);
        bound_ = true;
    }

    void Mount(const char* filesystem) {
        ASSERT_TRUE(bound_);
        ASSERT_FALSE(mounted_);
        // No nobarrier option, no read-only mount, no downgrading on failure.
        ASSERT_EQ(0, TryMount(filesystem))
            << "default " << filesystem << " mount: " << strerror(errno);
    }

    int TryMount(const char* filesystem) {
        const int result = mount(device_.c_str(), mount_point_.c_str(), filesystem, 0, nullptr);
        if (result == 0) mounted_ = true;
        return result;
    }

    void Unmount() {
        ASSERT_TRUE(mounted_);
        ASSERT_EQ(0, umount(mount_point_.c_str())) << strerror(errno);
        mounted_ = false;
    }

    void Unbind() {
        ASSERT_FALSE(mounted_);
        ASSERT_TRUE(bound_);
        ASSERT_EQ(0, ioctl(loop_.value, dunitest::kLoopClrFd, 0)) << strerror(errno);
        bound_ = false;
        ASSERT_NO_FATAL_FAILURE(loop_.Close());
        ASSERT_NO_FATAL_FAILURE(backing_.Close());
    }

    bool Close() {
        if (mounted_) {
            if (umount(mount_point_.c_str()) != 0) {
                ADD_FAILURE() << "private loop unmount: " << strerror(errno);
                return false;
            }
            mounted_ = false;
        }
        if (bound_) {
            if (ioctl(loop_.value, dunitest::kLoopClrFd, 0) != 0) {
                ADD_FAILURE() << "private loop clear: " << strerror(errno);
                return false;
            }
            bound_ = false;
        }
        if (loop_.value >= 0) { close(loop_.value); loop_.value = -1; }
        if (backing_.value >= 0) { close(backing_.value); backing_.value = -1; }
        return true;
    }
private:
    Fd loop_;
    Fd backing_;
    bool bound_ = false;
    bool mounted_ = false;
    std::string image_;
    std::string mount_point_;
    std::string device_;
};

class FatLoopDurability : public testing::Test {
protected:
    void SetUp() override {
        Stage stage("fixture-setup");
        const char* supplied = getenv("DUNITEST_FAT_BACKING_DIR");
        std::string fat_directory;
        if (supplied && *supplied) {
            fat_directory = supplied;
        } else {
            // Fixed existing fixture, not a larger-cluster performance escape.
            // The 64MiB/512B guest scratch remains the primary DKC-088 run.
            outer_ = MakeDirectory("/tmp", "fat-loop-fixture-");
            ASSERT_FALSE(outer_.empty()) << strerror(errno);
            fat_image_ = outer_ + "/fat.img";
            fat_directory = outer_ + "/fat";
            ASSERT_EQ(0, mkdir(fat_directory.c_str(), 0700)) << strerror(errno);
            Fd image {open(fat_image_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
            ASSERT_GE(image.value, 0) << strerror(errno);
            ASSERT_NO_FATAL_FAILURE(dunitest::CopySparseFile(
                dunitest::FixturePath("fat32_stat_blocks.img"), image.value));
            ASSERT_EQ(0, fsync(image.value)) << strerror(errno);
            ASSERT_NO_FATAL_FAILURE(image.Close());
            ASSERT_NO_FATAL_FAILURE(fat_loop_.Bind(fat_image_, fat_directory));
            ASSERT_NO_FATAL_FAILURE(fat_loop_.Mount("vfat"));
            printf("[ FAT-LOOP ] fallback fixture=fat32_stat_blocks.img (fixed geometry)\n");
        }
        struct statfs fs {};
        ASSERT_EQ(0, statfs(fat_directory.c_str(), &fs)) << strerror(errno);
        ASSERT_EQ(kFatMagic, fs.f_type) << "DUNITEST_FAT_BACKING_DIR must name a FAT mount";
        root_ = MakeDirectory(fat_directory, "fat-durability-");
        ASSERT_FALSE(root_.empty()) << strerror(errno);
        scratch_ = MakeDirectory("/tmp", "fat-loop-mount-");
        ASSERT_FALSE(scratch_.empty()) << strerror(errno);
        printf("[ FAT-LOOP ] FAT=%s block_size=%lld owned_directory=%s\n",
               fat_directory.c_str(), static_cast<long long>(fs.f_bsize), root_.c_str());
        fflush(stdout);
    }

    void TearDown() override {
        // Never unlink a backing image underneath a live filesystem. Only
        // paths in this test's mkdtemp directories are cleanup targets.
        if (!ext4_loop_.Close()) return;
        if (!root_.empty()) {
            for (const char* path : {"growing", "growth-boundaries", "ext4.img",
                                    "from/growing", "to/renamed", "positioned-io", "truncate-growth",
                                    "errseq-file"}) {
                if (unlink((root_ + "/" + path).c_str()) != 0 && errno != ENOENT) {
                    ADD_FAILURE() << path << ": " << strerror(errno);
                }
            }
            for (const char* dir : {"from", "to"}) {
                if (rmdir((root_ + "/" + dir).c_str()) != 0 && errno != ENOENT) {
                    ADD_FAILURE() << dir << ": " << strerror(errno);
                }
            }
            EXPECT_EQ(0, rmdir(root_.c_str())) << strerror(errno);
            root_.clear();
        }
        if (!scratch_.empty()) { EXPECT_EQ(0, rmdir(scratch_.c_str())); scratch_.clear(); }
        if (!fat_loop_.Close()) return;
        if (!fat_image_.empty()) unlink(fat_image_.c_str());
        if (!outer_.empty()) {
            rmdir((outer_ + "/fat").c_str());
            rmdir(outer_.c_str());
        }
    }

    std::string root_;
    std::string scratch_;
    PrivateLoop ext4_loop_;
private:
    std::string outer_;
    std::string fat_image_;
    PrivateLoop fat_loop_;
};

TEST_F(FatLoopDurability, GrowingBackingFsyncReopenRenameDirectorySync) {
    ASSERT_EQ(0, mkdir((root_ + "/from").c_str(), 0700));
    ASSERT_EQ(0, mkdir((root_ + "/to").c_str(), 0700));
    const std::string original = root_ + "/from/growing";
    const std::string renamed = root_ + "/to/renamed";
    Fd file {open(original.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
    ASSERT_GE(file.value, 0) << strerror(errno);
    {
        Stage stage("FAT-2MiB-growth-write");
        ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, 0, 2 * kMiB));
    }
    {
        Stage stage("FAT-growth-fsync");
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
    }
    ASSERT_NO_FATAL_FAILURE(file.Close());
    {
        Stage stage("FAT-2MiB-reopen-verify");
        ASSERT_NO_FATAL_FAILURE(VerifyPattern(original, 2 * kMiB));
    }
    file.value = open(original.c_str(), O_RDWR);
    ASSERT_GE(file.value, 0);
    ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, 2 * kMiB, 128 * 1024));
    {
        Stage stage("FAT-append-fdatasync");
        ASSERT_EQ(0, fdatasync(file.value)) << strerror(errno);
    }
    ASSERT_NO_FATAL_FAILURE(file.Close());
    {
        Stage stage("FAT-rename-and-directory-fsync");
        ASSERT_EQ(0, rename(original.c_str(), renamed.c_str())) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(SyncDirectory(root_ + "/from"));
        ASSERT_NO_FATAL_FAILURE(SyncDirectory(root_ + "/to"));
        ASSERT_NO_FATAL_FAILURE(SyncDirectory(root_));
    }
    ASSERT_NO_FATAL_FAILURE(VerifyPattern(renamed, 2 * kMiB + 128 * 1024));
    errno = 0;
    EXPECT_EQ(-1, access(original.c_str(), F_OK));
    EXPECT_EQ(ENOENT, errno);
}

TEST_F(FatLoopDurability, Ext4DefaultBarrierMountAndRemount) {
    const std::string image = root_ + "/ext4.img";
    {
        Stage stage("FAT-8MiB-ext4-image-copy-and-sync");
        Fd backing {open(image.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(backing.value, 0);
        ASSERT_NO_FATAL_FAILURE(dunitest::CopySparseFile(dunitest::FixturePath(), backing.value));
        struct stat st {};
        ASSERT_EQ(0, fstat(backing.value, &st));
        ASSERT_EQ(static_cast<off_t>(8 * kMiB), st.st_size) << "do not shrink the ext4 fixture";
        ASSERT_EQ(0, fsync(backing.value)) << strerror(errno);
    }
    ASSERT_NO_FATAL_FAILURE(SyncDirectory(root_));
    ASSERT_NO_FATAL_FAILURE(ext4_loop_.Bind(image, scratch_));
    {
        Stage stage("ext4-default-barrier-mount");
        ASSERT_NO_FATAL_FAILURE(ext4_loop_.Mount("ext4"));
    }
    const std::string payload = scratch_ + "/durable-payload";
    {
        Stage stage("ext4-write-file-and-directory-fsync");
        Fd file {open(payload.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(file.value, 0);
        ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, 0, 256 * 1024));
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(file.Close());
        ASSERT_NO_FATAL_FAILURE(SyncDirectory(scratch_));
    }
    {
        Stage stage("ext4-normal-unmount-and-loop-unbind");
        ASSERT_NO_FATAL_FAILURE(ext4_loop_.Unmount());
        ASSERT_NO_FATAL_FAILURE(ext4_loop_.Unbind());
    }
    // Re-open the FAT backing OFD and bind again, rather than validating the
    // still-mounted ext4/page-cache view. This is not a power-cut simulation.
    ASSERT_NO_FATAL_FAILURE(ext4_loop_.Bind(image, scratch_));
    {
        Stage stage("ext4-default-barrier-remount-and-verify");
        ASSERT_NO_FATAL_FAILURE(ext4_loop_.Mount("ext4"));
        ASSERT_NO_FATAL_FAILURE(VerifyPattern(payload, 256 * 1024));
    }
    {
        Stage stage("ext4-final-normal-unmount");
        ASSERT_NO_FATAL_FAILURE(ext4_loop_.Unmount());
        ASSERT_NO_FATAL_FAILURE(ext4_loop_.Unbind());
    }
}

TEST_F(FatLoopDurability, RepeatedBoundaryGrowthAndOverwriteSync) {
    const std::string path = root_ + "/growth-boundaries";
    Fd file {open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
    ASSERT_GE(file.value, 0);
    uint64_t previous = 0;
    for (uint64_t size : {uint64_t(513), uint64_t(32 * 1024 + 17), 2 * kMiB + 1025}) {
        Stage stage("FAT-boundary-growth-and-sync");
        ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, previous, size - previous));
        ASSERT_EQ(0, fdatasync(file.value)) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(file.Close());
        ASSERT_NO_FATAL_FAILURE(VerifyPattern(path, size));
        file.value = open(path.c_str(), O_RDWR);
        ASSERT_GE(file.value, 0);
        previous = size;
    }
    {
        Stage stage("FAT-cross-boundary-overwrite-fsync");
        // Cover both 512B and 4KiB boundaries without changing the volume.
        ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, 509, 8197));
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(file.Close());
        ASSERT_NO_FATAL_FAILURE(SyncDirectory(root_));
        ASSERT_NO_FATAL_FAILURE(VerifyPattern(path, previous));
    }
}

TEST_F(FatLoopDurability, RandomWritesAndReverseReadsPreserveClusterLookup) {
    constexpr size_t kPage = 4096;
    constexpr size_t kPages = 2 * kMiB / kPage;
    constexpr size_t kPatchStart = 509;
    constexpr size_t kPatchLength = 819;
    const std::string path = root_ + "/positioned-io";
    Fd file {open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
    ASSERT_GE(file.value, 0);
    {
        Stage stage("FAT-2MiB-positioned-io-prepare");
        ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, 0, 2 * kMiB));
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
    }
    std::array<bool, kPages> patched {};
    std::array<uint8_t, kPage> block {};
    {
        Stage stage("FAT-random-pread-pwrite");
        // A deterministic permutation, not monotonically increasing offsets:
        // exercise cached lookup before and behind the last visited cluster.
        for (size_t index = 0; index < 64; ++index) {
            const size_t page = (index * 73 + 211) % kPages;
            const off_t base = static_cast<off_t>(page * kPage);
            ASSERT_EQ(static_cast<ssize_t>(block.size()),
                      pread(file.value, block.data(), block.size(), base)) << strerror(errno);
            for (size_t byte = 0; byte < block.size(); ++byte) {
                ASSERT_EQ(Pattern(base + byte), block[byte]) << "page=" << page << " byte=" << byte;
            }
            for (size_t byte = 0; byte < kPatchLength; ++byte) {
                block[byte] = Pattern(base + kPatchStart + byte) ^ 0xa5;
            }
            ASSERT_EQ(static_cast<ssize_t>(kPatchLength),
                      pwrite(file.value, block.data(), kPatchLength, base + kPatchStart))
                << strerror(errno);
            patched[page] = true;
        }
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(file.Close());
    }
    file.value = open(path.c_str(), O_RDONLY);
    ASSERT_GE(file.value, 0);
    {
        Stage stage("FAT-2MiB-reverse-pread-verify");
        for (size_t remaining = kPages; remaining != 0; --remaining) {
            const size_t page = remaining - 1;
            const off_t base = static_cast<off_t>(page * kPage);
            ASSERT_EQ(static_cast<ssize_t>(block.size()),
                      pread(file.value, block.data(), block.size(), base)) << strerror(errno);
            for (size_t byte = 0; byte < block.size(); ++byte) {
                uint8_t expected = Pattern(base + byte);
                if (patched[page] && byte >= kPatchStart && byte < kPatchStart + kPatchLength) {
                    expected ^= 0xa5;
                }
                ASSERT_EQ(expected, block[byte]) << "page=" << page << " byte=" << byte;
            }
        }
    }
}

TEST_F(FatLoopDurability, TruncateShrinkRegrowthZeroesDiscardedClustersAndTail) {
    constexpr uint64_t kRetained = 512 + 17;
    constexpr uint64_t kSize = 2 * kMiB;
    const std::string path = root_ + "/truncate-growth";
    Fd file {open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
    ASSERT_GE(file.value, 0);
    {
        Stage stage("FAT-2MiB-truncate-prepare");
        ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, 0, kSize));
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
    }
    {
        Stage stage("FAT-cross-cluster-shrink-regrow-sync");
        ASSERT_EQ(0, ftruncate(file.value, kRetained)) << strerror(errno);
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
        ASSERT_EQ(0, ftruncate(file.value, kSize)) << strerror(errno);
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(file.Close());
    }
    file.value = open(path.c_str(), O_RDWR);
    ASSERT_GE(file.value, 0);
    struct stat st {};
    ASSERT_EQ(0, fstat(file.value, &st));
    ASSERT_EQ(static_cast<off_t>(kSize), st.st_size);
    std::array<uint8_t, 64 * 1024> data {};
    {
        Stage stage("FAT-2MiB-zero-tail-reopen-verify");
        for (uint64_t offset = 0; offset < kSize; offset += data.size()) {
            ASSERT_EQ(static_cast<ssize_t>(data.size()),
                      pread(file.value, data.data(), data.size(), offset)) << strerror(errno);
            for (size_t byte = 0; byte < data.size(); ++byte) {
                const uint8_t expected = offset + byte < kRetained ? Pattern(offset + byte) : 0;
                ASSERT_EQ(expected, data[byte]) << "offset=" << offset + byte;
            }
        }
        // Restore data across the last cluster boundary after the regrowth;
        // this catches stale cached EOF/successor entries, not only zeroing.
        ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, kSize - 4097, 4097));
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
        ASSERT_NO_FATAL_FAILURE(file.Close());
    }
    file.value = open(path.c_str(), O_RDONLY);
    ASSERT_GE(file.value, 0);
    {
        Stage stage("FAT-regrown-final-tail-reopen-verify");
        std::array<uint8_t, 4097> tail {};
        ASSERT_EQ(static_cast<ssize_t>(tail.size()),
                  pread(file.value, tail.data(), tail.size(), kSize - tail.size())) << strerror(errno);
        for (size_t byte = 0; byte < tail.size(); ++byte) {
            ASSERT_EQ(Pattern(kSize - tail.size() + byte), tail[byte]);
        }
        ASSERT_NO_FATAL_FAILURE(SyncDirectory(root_));
    }
}

TEST_F(FatLoopDurability, FsyncPropagatesMappingErrorPerOpenDescription) {
    Fd injector {open("/sys/kernel/debug/errseq/inject", O_WRONLY)};
    if (injector.value < 0) {
        GTEST_SKIP() << "errseq debug injector is unavailable: " << strerror(errno);
    }
    const std::string path = root_ + "/errseq-file";
    Fd file {open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
    ASSERT_GE(file.value, 0);
    ASSERT_NO_FATAL_FAILURE(WritePattern(file.value, 0, 4096));
    ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
    Fd second {open(path.c_str(), O_RDWR)};
    ASSERT_GE(second.value, 0);
    {
        Stage stage("FAT-mapping-EIO-injection-and-fsync-reporting");
        char command[128] {};
        const int length = snprintf(command, sizeof(command), "mapping %d EIO\n", file.value);
        ASSERT_GT(length, 0);
        ASSERT_LT(static_cast<size_t>(length), sizeof(command));
        ASSERT_EQ(length, write(injector.value, command, length)) << strerror(errno);
        errno = 0;
        EXPECT_EQ(-1, fsync(file.value));
        EXPECT_EQ(EIO, errno);
        errno = 0;
        EXPECT_EQ(0, fsync(file.value)) << strerror(errno);
        errno = 0;
        EXPECT_EQ(-1, fsync(second.value));
        EXPECT_EQ(EIO, errno);
        errno = 0;
        EXPECT_EQ(0, fsync(second.value)) << strerror(errno);
        Fd late {open(path.c_str(), O_RDWR)};
        ASSERT_GE(late.value, 0);
        errno = 0;
        EXPECT_EQ(0, fsync(late.value)) << strerror(errno);
    }
    ASSERT_NO_FATAL_FAILURE(VerifyPattern(path, 4096));
}

class RamfsBacking {
public:
    ~RamfsBacking() {
        // The caller's private loops are destroyed before this backing mount.
        if (mounted_ && umount(memory_.c_str()) != 0) {
            ADD_FAILURE() << "private ramfs unmount: " << strerror(errno);
            return;
        }
        if (!root_.empty()) {
            for (const char* directory : {"ramfs", "fat", "ext4"}) {
                if (rmdir((root_ + "/" + directory).c_str()) != 0) {
                    ADD_FAILURE() << "private capability directory: " << strerror(errno);
                }
            }
            EXPECT_EQ(0, rmdir(root_.c_str())) << strerror(errno);
        }
    }
    void SetUp() {
        root_ = MakeDirectory("/tmp", "fat-capability-");
        ASSERT_FALSE(root_.empty()) << strerror(errno);
        for (const char* directory : {"ramfs", "fat", "ext4"}) {
            ASSERT_EQ(0, mkdir((root_ + "/" + directory).c_str(), 0700));
        }
        memory_ = root_ + "/ramfs";
        ASSERT_EQ(0, mount("ramfs", memory_.c_str(), "ramfs", 0, nullptr)) << strerror(errno);
        mounted_ = true;
    }
    std::string MemoryPath() const { return memory_; }
    std::string FatPath() const { return root_ + "/fat"; }
    std::string Ext4Path() const { return root_ + "/ext4"; }
private:
    bool mounted_ = false;
    std::string root_;
    std::string memory_;
};

TEST(FatLoopCapabilities, UnreliableRamfsBackingDoesNotBecomeReliableThroughFat) {
    // DragonOS ramfs keeps FileSystem's default supports_reliable_flush=false.
    // tmpfs deliberately reports true, so it would not be a valid negative
    // capability fixture. This tests delegation, not a physical flush failure.
    RamfsBacking backing;
    ASSERT_NO_FATAL_FAILURE(backing.SetUp());
    PrivateLoop fat_loop;
    PrivateLoop ext4_loop;
    const std::string fat_image = backing.MemoryPath() + "/fat.img";
    {
        Stage stage("unreliable-ramfs-FAT-fixture-copy");
        Fd image {open(fat_image.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(image.value, 0);
        ASSERT_NO_FATAL_FAILURE(dunitest::CopySparseFile(
            dunitest::FixturePath("fat32_stat_blocks.img"), image.value));
    }
    ASSERT_NO_FATAL_FAILURE(fat_loop.Bind(fat_image, backing.FatPath()));
    ASSERT_NO_FATAL_FAILURE(fat_loop.Mount("vfat"));
    const std::string ext4_image = backing.FatPath() + "/ext4.img";
    {
        Stage stage("unreliable-FAT-8MiB-ext4-fixture-copy");
        Fd image {open(ext4_image.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(image.value, 0);
        ASSERT_NO_FATAL_FAILURE(dunitest::CopySparseFile(dunitest::FixturePath(), image.value));
        struct stat st {};
        ASSERT_EQ(0, fstat(image.value, &st));
        ASSERT_EQ(static_cast<off_t>(8 * kMiB), st.st_size);
    }
    ASSERT_NO_FATAL_FAILURE(ext4_loop.Bind(ext4_image, backing.Ext4Path()));
    {
        Stage stage("unreliable-FAT-default-barrier-rejection");
        errno = 0;
        EXPECT_EQ(-1, ext4_loop.TryMount("ext4"));
        EXPECT_EQ(EOPNOTSUPP, errno);
    }
    // Do not demand a successful durability barrier from the explicitly
    // untrusted backing. Init bytes are only used to test mount admission.
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
