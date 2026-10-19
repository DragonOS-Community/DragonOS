#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"

#include <array>
#include <atomic>
#include <thread>
#include <vector>
#include <cstdlib>
#include <fcntl.h>
#include <linux/falloc.h>
#include <sys/mount.h>
#include <sys/syscall.h>
#include <linux/loop.h>
#include <string>
#include <sys/ioctl.h>
#include <unistd.h>

namespace {
struct ScopedFd {
    int value = -1;
    ~ScopedFd() { if (value >= 0) close(value); }
    int Close() { const int fd = value; value = -1; return close(fd); }
};

class LoopBackingIo : public testing::Test {
protected:
    void SetUp() override {
        // A real ext4 owner is necessary: tmpfs deliberately ignores O_DIRECT.
        ASSERT_NO_FATAL_FAILURE(fs_.SetUp("ext4_inode_identity.img", "/tmp", "loop_backing_io"));
        ASSERT_NO_FATAL_FAILURE(fs_.Mount());
        mounted_ = true;
        backing_path_ = fs_.mount_point() + "/backing";
        ScopedFd seed {open(backing_path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(seed.value, 0) << strerror(errno);
        std::array<unsigned char, 8192> bytes {};
        for (size_t i = 0; i != bytes.size(); ++i) bytes[i] = Pattern(i);
        ASSERT_EQ(static_cast<ssize_t>(bytes.size()), write(seed.value, bytes.data(), bytes.size()));
        ASSERT_EQ(0, fsync(seed.value));
        ScopedFd control {open("/dev/loop-control", O_RDWR)};
        ASSERT_GE(control.value, 0);
        const int minor = ioctl(control.value, LOOP_CTL_GET_FREE, 0);
        ASSERT_GE(minor, 0);
        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_.value = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(loop_.value, 0);
    }

    void TearDown() override {
        if (bound_) {
            EXPECT_EQ(0, ioctl(loop_.value, LOOP_CLR_FD, 0)) << strerror(errno);
            bound_ = false;
        }
        if (loop_.value >= 0) { EXPECT_EQ(0, loop_.Close()); }
        if (backing_.value >= 0) { EXPECT_EQ(0, backing_.Close()); }
        if (mounted_) {
            // Linux releases a loop worker's last backing reference through
            // deferred fput. Fixture cleanup owns no immediate-umount contract.
            // Reuse the bounded lazy-detach cleanup of the existing loop helper.
            ASSERT_NO_FATAL_FAILURE(fs_.Detach());
            ASSERT_NO_FATAL_FAILURE(fs_.FinishDetached());
            mounted_ = false;
        }
    }

    void Bind(bool direct) {
        backing_.value = open(backing_path_.c_str(), O_RDWR | (direct ? O_DIRECT : 0));
        ASSERT_GE(backing_.value, 0) << strerror(errno);
        ASSERT_EQ(137, lseek(backing_.value, 137, SEEK_SET));
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_FD, backing_.value)) << strerror(errno);
        bound_ = true;
    }

    void SetOffset(uint64_t offset) {
        loop_info64 status {};
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_GET_STATUS64, &status));
        status.lo_offset = offset;
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_STATUS64, &status)) << strerror(errno);
    }

    void ExpectDio(bool enabled) {
        loop_info64 status {};
        ASSERT_EQ(0, ioctl(loop_.value, LOOP_GET_STATUS64, &status));
        EXPECT_EQ(enabled, !!(status.lo_flags & LO_FLAGS_DIRECT_IO));
    }

    void ExpectRetainedFlags(bool direct) {
        int flags = fcntl(backing_.value, F_GETFL);
        ASSERT_GE(flags, 0);
        EXPECT_EQ(direct, !!(flags & O_DIRECT));
        EXPECT_EQ(137, lseek(backing_.value, 0, SEEK_CUR));
    }

    void ReadPattern(size_t backing_offset) {
        // Buffered loop-node I/O has no userspace alignment requirement.
        std::array<unsigned char, 512> bytes {};
        ASSERT_EQ(512, pread(loop_.value, bytes.data(), bytes.size(), 0)) << strerror(errno);
        for (size_t i = 0; i != bytes.size(); ++i) EXPECT_EQ(Pattern(backing_offset + i), bytes[i]);
    }

    void ByteRoundtrip(size_t base = 0) {
        const size_t capacity = (8192 - base) / 512 * 512;
        std::vector<unsigned char> expected(capacity);
        for (size_t i = 0; i < capacity; ++i) expected[i] = Pattern(base + i);
        unsigned char one = 0;
        ASSERT_EQ(1, pread(loop_.value, &one, 1, 1));
        EXPECT_EQ(expected[1], one);
        for (const auto& range : {std::pair<size_t, size_t>{1, 1}, {511, 2},
                                 {17, 1543}, {4095, 1537}}) {
            SCOPED_TRACE(range.first);
            SCOPED_TRACE(range.second);
            std::vector<unsigned char> data(range.second, 0x39 + range.first % 23);
            ASSERT_EQ(static_cast<ssize_t>(data.size()),
                      pwrite(loop_.value, data.data(), data.size(), range.first)) << strerror(errno);
            std::copy(data.begin(), data.end(), expected.begin() + range.first);
            std::vector<unsigned char> actual(capacity);
            ASSERT_EQ(static_cast<ssize_t>(capacity),
                      pread(loop_.value, actual.data(), actual.size(), 0)) << strerror(errno);
            EXPECT_EQ(expected, actual);
        }
        ASSERT_EQ(0, fsync(loop_.value));
        ScopedFd plain {open(backing_path_.c_str(), O_RDONLY)};
        ASSERT_GE(plain.value, 0);
        std::array<unsigned char, 8192> actual {};
        ASSERT_EQ(static_cast<ssize_t>(actual.size()),
                  pread(plain.value, actual.data(), actual.size(), 0));
        for (size_t i = 0; i < actual.size(); ++i) {
            EXPECT_EQ(i >= base && i < base + capacity ? expected[i - base] : Pattern(i), actual[i]) << i;
        }
    }

    static unsigned char Pattern(size_t i) { return (i * 31 + 17) % 251; }

    dunitest::LoopExt4 fs_;
    ScopedFd backing_;
    ScopedFd loop_;
    std::string backing_path_;
    std::string loop_path_;
    bool bound_ = false;
    bool mounted_ = false;
};

TEST_F(LoopBackingIo, BufferedBackingSupportsByteOffsetAndPreservesDescription) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ASSERT_NO_FATAL_FAILURE(SetOffset(513));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    ASSERT_NO_FATAL_FAILURE(ReadPattern(513));
    std::array<unsigned char, 512> replacement {};
    replacement.fill(0x5a);
    ASSERT_EQ(512, pwrite(loop_.value, replacement.data(), replacement.size(), 0)) << strerror(errno);
    ASSERT_EQ(0, fsync(loop_.value));
    ScopedFd plain {open(backing_path_.c_str(), O_RDONLY)};
    ASSERT_GE(plain.value, 0);
    std::array<unsigned char, 514> observed {};
    ASSERT_EQ(514, pread(plain.value, observed.data(), observed.size(), 512));
    EXPECT_EQ(Pattern(512), observed.front());
    for (size_t i = 1; i != 513; ++i) EXPECT_EQ(0x5a, observed[i]);
    EXPECT_EQ(Pattern(1025), observed.back());
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
}

TEST_F(LoopBackingIo, DirectBackingAlignedUsesDioAndRetainsOriginalDescription) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_NO_FATAL_FAILURE(ReadPattern(0));
    std::array<unsigned char, 512> replacement {};
    replacement.fill(0x5a);
    ASSERT_EQ(512, pwrite(loop_.value, replacement.data(), replacement.size(), 0)) << strerror(errno);
    ASSERT_EQ(0, fsync(loop_.value));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(true));
    ScopedFd plain {open(backing_path_.c_str(), O_RDONLY)};
    ASSERT_GE(plain.value, 0);
    std::array<unsigned char, 512> observed {};
    ASSERT_EQ(512, pread(plain.value, observed.data(), observed.size(), 0));
    EXPECT_EQ(replacement, observed);
}

TEST_F(LoopBackingIo, MisalignedMappingDoesNotClearBackingDirectFlag) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ASSERT_NO_FATAL_FAILURE(SetOffset(513));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    std::array<unsigned char, 512> bytes {};
    // Linux disables loop AIO, but simple vfs_iter_* still derives IOCB_DIRECT
    // from the retained backing description; ext4 rejects the backing offset.
    errno = 0;
    EXPECT_EQ(-1, pread(loop_.value, bytes.data(), bytes.size(), 0));
    EXPECT_EQ(EIO, errno);
    unsigned char one = 0;
    errno = 0;
    EXPECT_EQ(-1, pread(loop_.value, &one, 1, 1));
    EXPECT_EQ(EIO, errno);
    errno = 0;
    EXPECT_EQ(-1, pwrite(loop_.value, bytes.data(), bytes.size(), 0));
    EXPECT_EQ(EIO, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(true));
    ASSERT_NO_FATAL_FAILURE(SetOffset(0));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    ASSERT_NO_FATAL_FAILURE(ReadPattern(0));
}

TEST_F(LoopBackingIo, SharedBackingFlagChangeDoesNotChangePublishedDio) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ASSERT_NO_FATAL_FAILURE(SetOffset(513));
    int original = fcntl(backing_.value, F_GETFL);
    ASSERT_GE(original, 0);
    ASSERT_EQ(0, fcntl(backing_.value, F_SETFL, original | O_DIRECT));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    std::array<unsigned char, 512> bytes {};
    errno = 0;
    EXPECT_EQ(-1, pread(loop_.value, bytes.data(), bytes.size(), 0));
    EXPECT_EQ(EIO, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(true));
    ASSERT_EQ(0, fcntl(backing_.value, F_SETFL, original));
    ASSERT_NO_FATAL_FAILURE(ReadPattern(513));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
}

TEST_F(LoopBackingIo, ExplicitDioControlDoesNotModifyBackingFlags) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_DIRECT_IO, 1UL)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_NO_FATAL_FAILURE(ReadPattern(0));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_DIRECT_IO, 0UL)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    ASSERT_NO_FATAL_FAILURE(SetOffset(513));
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_.value, LOOP_SET_DIRECT_IO, 1UL));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    ASSERT_NO_FATAL_FAILURE(ReadPattern(513));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
}

TEST_F(LoopBackingIo, ExplicitDioPreservesByteAndMultiSectorReadModifyWrite) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_DIRECT_IO, 1UL));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_NO_FATAL_FAILURE(ByteRoundtrip());
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
}

TEST_F(LoopBackingIo, AutomaticDioSupportsUnalignedByteRequests) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_NO_FATAL_FAILURE(ByteRoundtrip());
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(true));
}

TEST_F(LoopBackingIo, AutomaticDioSurvivesClearingTheOriginalDescriptionFlag) {
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    const int flags = fcntl(backing_.value, F_GETFL);
    ASSERT_GE(flags, 0);
    ASSERT_EQ(0, fcntl(backing_.value, F_SETFL, flags & ~O_DIRECT));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_NO_FATAL_FAILURE(ByteRoundtrip());
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
}

TEST_F(LoopBackingIo, ConcurrentEdgesAndWholeSectorWritesShareOneTransaction) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_DIRECT_IO, 1UL));
    constexpr std::array<off_t, 4> positions {3, 197, 330, 509};
    std::atomic<unsigned> errors {0};
    std::atomic<bool> start {false};
    std::vector<std::thread> workers;
    for (size_t id = 0; id < positions.size(); ++id) {
        workers.emplace_back([&, id] {
            ScopedFd writer {open(loop_path_.c_str(), O_RDWR)};
            if (writer.value < 0) { ++errors; return; }
            const unsigned char value = 0xc0 + id;
            while (!start.load(std::memory_order_acquire)) std::this_thread::yield();
            for (unsigned iteration = 0; iteration < 64; ++iteration) {
                if (pwrite(writer.value, &value, 1, positions[id]) != 1) ++errors;
            }
        });
    }
    start.store(true, std::memory_order_release);
    for (auto& worker : workers) worker.join();
    EXPECT_EQ(0U, errors.load());
    std::array<unsigned char, 512> actual {};
    ASSERT_EQ(512, pread(loop_.value, actual.data(), actual.size(), 0));
    for (size_t index = 0; index < actual.size(); ++index) {
        unsigned char expected = Pattern(index);
        for (size_t id = 0; id < positions.size(); ++id) {
            if (index == static_cast<size_t>(positions[id])) expected = 0xc0 + id;
        }
        EXPECT_EQ(expected, actual[index]) << index;
    }

    // A stale edge RMW must never restore older bytes outside its own byte
    // after the complete writer's final sector has committed.
    start.store(false, std::memory_order_relaxed);
    std::thread whole([&] {
        ScopedFd writer {open(loop_path_.c_str(), O_RDWR)};
        if (writer.value < 0) { ++errors; return; }
        std::array<unsigned char, 512> sector;
        while (!start.load(std::memory_order_acquire)) std::this_thread::yield();
        for (unsigned iteration = 0; iteration < 128; ++iteration) {
            sector.fill(iteration);
            if (pwrite(writer.value, sector.data(), sector.size(), 0) != 512) ++errors;
        }
    });
    std::thread edge([&] {
        ScopedFd writer {open(loop_path_.c_str(), O_RDWR)};
        if (writer.value < 0) { ++errors; return; }
        const unsigned char value = 0xfd;
        while (!start.load(std::memory_order_acquire)) std::this_thread::yield();
        for (unsigned iteration = 0; iteration < 128; ++iteration) {
            if (pwrite(writer.value, &value, 1, 3) != 1) ++errors;
        }
    });
    start.store(true, std::memory_order_release);
    whole.join();
    edge.join();
    EXPECT_EQ(0U, errors.load());
    ASSERT_EQ(0, fsync(loop_.value));
    ASSERT_EQ(512, pread(loop_.value, actual.data(), actual.size(), 0));
    for (size_t index = 0; index < actual.size(); ++index) {
        if (index != 3) { EXPECT_EQ(127, actual[index]) << index; }
    }
    EXPECT_TRUE(actual[3] == 127 || actual[3] == 0xfd);
}

// The loop capacity is a published mapping property: truncating the backing
// file does not update it until LOOP_SET_CAPACITY. Use a fresh O_DIRECT node
// description so a raw block page-cache hit cannot hide the backing short read.
TEST_F(LoopBackingIo, SimpleBackingShortReadClearsTheWholeSector) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    ASSERT_EQ(0, ftruncate(backing_.value, 257));
    ASSERT_EQ(0, fsync(backing_.value));
    uint64_t capacity = 0;
    ASSERT_EQ(0, ioctl(loop_.value, BLKGETSIZE64, &capacity));
    ASSERT_EQ(8192U, capacity);
    ScopedFd node {open(loop_path_.c_str(), O_RDONLY | O_DIRECT)};
    ASSERT_GE(node.value, 0);
    alignas(4096) std::array<unsigned char, 512> bytes;
    bytes.fill(0xa5);
    ASSERT_EQ(512, pread(node.value, bytes.data(), bytes.size(), 0)) << strerror(errno);
    // Linux lo_read_simple zero_fill_bio clears even the 257-byte prefix that
    // was read successfully; preserving that prefix would be a regression.
    for (size_t index = 0; index < bytes.size(); ++index) EXPECT_EQ(0, bytes[index]) << index;
    ASSERT_NO_FATAL_FAILURE(ExpectDio(false));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
}

TEST_F(LoopBackingIo, EffectiveDioShortReadFailsWithoutRefreshingCapacity) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ASSERT_EQ(0, ioctl(loop_.value, LOOP_SET_DIRECT_IO, 1UL));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_EQ(0, ftruncate(backing_.value, 257));
    ASSERT_EQ(0, fsync(backing_.value));
    uint64_t capacity = 0;
    ASSERT_EQ(0, ioctl(loop_.value, BLKGETSIZE64, &capacity));
    ASSERT_EQ(8192U, capacity);
    ScopedFd node {open(loop_path_.c_str(), O_RDONLY | O_DIRECT)};
    ASSERT_GE(node.value, 0);
    alignas(4096) std::array<unsigned char, 512> bytes;
    bytes.fill(0xa5);
    // Effective DIO retries a short backing read; reaching backing EOF while
    // the published request remains incomplete ends that request with EIO.
    errno = 0;
    EXPECT_EQ(-1, pread(node.value, bytes.data(), bytes.size(), 0));
    EXPECT_EQ(EIO, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
}

TEST_F(LoopBackingIo, BlockBackingRetainsDirectAdmissionAfterMisalignedMapping) {
    ASSERT_NO_FATAL_FAILURE(Bind(false));
    ScopedFd retained {open(loop_path_.c_str(), O_RDWR | O_DIRECT)};
    ASSERT_GE(retained.value, 0);
    ASSERT_EQ(137, lseek(retained.value, 137, SEEK_SET));
    ScopedFd control {open("/dev/loop-control", O_RDWR)};
    ASSERT_GE(control.value, 0);
    const int minor = ioctl(control.value, LOOP_CTL_GET_FREE, 0);
    ASSERT_GE(minor, 0);
    ScopedFd outer {open(("/dev/loop" + std::to_string(minor)).c_str(), O_RDWR)};
    ASSERT_GE(outer.value, 0);
    // CLR runs before closing the retained block OFD and before the fixture
    // clears its inner loop, including on a fatal assertion below.
    struct Binding {
        int fd;
        bool bound = false;
        ~Binding() { if (bound) EXPECT_EQ(0, ioctl(fd, LOOP_CLR_FD, 0)); }
    } binding {outer.value};
    ASSERT_EQ(0, ioctl(outer.value, LOOP_SET_FD, retained.value)) << strerror(errno);
    binding.bound = true;
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(outer.value, LOOP_GET_STATUS64, &status));
    ASSERT_NE(0U, status.lo_flags & LO_FLAGS_DIRECT_IO);
    std::array<unsigned char, 512> bytes {};
    ASSERT_EQ(512, pread(outer.value, bytes.data(), bytes.size(), 0)) << strerror(errno);
    for (size_t index = 0; index < bytes.size(); ++index) EXPECT_EQ(Pattern(index), bytes[index]);
    status.lo_offset = 513;
    ASSERT_EQ(0, ioctl(outer.value, LOOP_SET_STATUS64, &status)) << strerror(errno);
    ASSERT_EQ(0, ioctl(outer.value, LOOP_GET_STATUS64, &status));
    ASSERT_EQ(0U, status.lo_flags & LO_FLAGS_DIRECT_IO);
    errno = 0;
    EXPECT_EQ(-1, pread(outer.value, bytes.data(), bytes.size(), 0));
    EXPECT_EQ(EIO, errno);
    const int flags = fcntl(retained.value, F_GETFL);
    ASSERT_GE(flags, 0);
    EXPECT_NE(0, flags & O_DIRECT);
    EXPECT_EQ(137, lseek(retained.value, 0, SEEK_CUR));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(false));
}

TEST_F(LoopBackingIo, DirectHoleReadsPreserveZeroesAndEof) {
    ScopedFd sparse {open(backing_path_.c_str(), O_RDWR)};
    ASSERT_GE(sparse.value, 0);
    ASSERT_EQ(0, fallocate(sparse.value, FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, 4096, 4096));
    ASSERT_EQ(0, fsync(sparse.value));
    ASSERT_EQ(0, sparse.Close());
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    std::array<unsigned char, 4096> data;
    data.fill(0xa5);
    ASSERT_EQ(4096, pread(loop_.value, data.data(), data.size(), 4096));
    for (unsigned char value : data) EXPECT_EQ(0, value);
    unsigned char one = 0xa5;
    EXPECT_EQ(1, pread(loop_.value, &one, 1, 4097));
    EXPECT_EQ(0, one);
    EXPECT_EQ(0, syscall(SYS_pread64, loop_.value, nullptr, 1, off_t(8192)));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(true));
}

TEST_F(LoopBackingIo, MemoryBackingWithNoDeviceAllowsDioAtByteOffset) {
    struct Tmpfs {
        std::string path;
        bool mounted = false;
        ~Tmpfs() {
            if (mounted) { EXPECT_EQ(0, umount2(path.c_str(), MNT_DETACH)); }
            if (!path.empty()) { EXPECT_EQ(0, rmdir(path.c_str())); }
        }
    } memory;
    char directory[] = "/tmp/loop_dio_tmpfs_XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(directory));
    memory.path = directory;
    ASSERT_EQ(0, mount("tmpfs", memory.path.c_str(), "tmpfs", 0, nullptr));
    memory.mounted = true;
    backing_path_ = memory.path + "/backing";
    {
        ScopedFd seed {open(backing_path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(seed.value, 0);
        std::array<unsigned char, 8192> bytes;
        for (size_t i = 0; i < bytes.size(); ++i) bytes[i] = Pattern(i);
        ASSERT_EQ(8192, write(seed.value, bytes.data(), bytes.size()));
        ASSERT_EQ(0, fsync(seed.value));
    }
    ASSERT_NO_FATAL_FAILURE(Bind(true));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_NO_FATAL_FAILURE(SetOffset(513));
    ASSERT_NO_FATAL_FAILURE(ExpectDio(true));
    ASSERT_NO_FATAL_FAILURE(ByteRoundtrip(513));
    ASSERT_NO_FATAL_FAILURE(ExpectRetainedFlags(true));
}

TEST_F(LoopBackingIo, MountedModeChangesDoNotMakeIoOrPublicStateGoOffline) {
    ScopedFd outer {open(fs_.loop_path().c_str(), O_RDWR)};
    ScopedFd data {open((fs_.mount_point() + "/during-mode").c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)};
    ASSERT_GE(outer.value, 0);
    ASSERT_GE(data.value, 0);
    std::atomic<unsigned> errors {0};
    std::atomic<bool> start {false};
    std::thread io([&] {
        std::array<unsigned char, 512> bytes;
        while (!start.load(std::memory_order_acquire)) std::this_thread::yield();
        for (unsigned iteration = 0; iteration < 64; ++iteration) {
            bytes.fill(iteration);
            if (pwrite(data.value, bytes.data(), bytes.size(), 0) != 512 || fsync(data.value) != 0) ++errors;
            std::array<unsigned char, 512> observed {};
            if (pread(data.value, observed.data(), observed.size(), 0) != 512 || observed != bytes) ++errors;
            ScopedFd probe {open(fs_.loop_path().c_str(), O_RDONLY)};
            loop_info64 status {};
            if (probe.value < 0 || ioctl(probe.value, LOOP_GET_STATUS64, &status) != 0) ++errors;
        }
    });
    start.store(true, std::memory_order_release);
    for (unsigned iteration = 0; iteration < 64; ++iteration) {
        if (ioctl(outer.value, LOOP_SET_DIRECT_IO, iteration & 1U) != 0) ++errors;
    }
    io.join();
    EXPECT_EQ(0U, errors.load());
    EXPECT_EQ(0, ioctl(outer.value, LOOP_SET_DIRECT_IO, 0UL));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
