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
#include <vector>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

constexpr off_t kBytes = 1024 * 1024 + 333;

class BlockDeviceIoctl : public testing::Test {
protected:
    void SetUp() override {
        const char* directory = getenv("DUNITEST_BLOCK_BACKING_DIR");
        std::string pattern = std::string(directory ? directory : "/tmp") +
                              "/block_ioctl_XXXXXX";
        std::vector<char> path(pattern.begin(), pattern.end());
        path.push_back('\0');
        backing_ = mkstemp(path.data());
        ASSERT_GE(backing_, 0) << strerror(errno);
        image_ = path.data();
        ASSERT_EQ(0, ftruncate(backing_, kBytes)) << strerror(errno);
        int control = open("/dev/loop-control", O_RDWR);
        ASSERT_GE(control, 0) << strerror(errno);
        int minor = ioctl(control, LOOP_CTL_GET_FREE, 0);
        int saved_errno = errno;
        close(control);
        ASSERT_GE(minor, 0) << strerror(saved_errno);
        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_ = open(loop_path_.c_str(), O_RDWR);
        ASSERT_GE(loop_, 0) << strerror(errno);
    }

    void TearDown() override {
        if (loop_ >= 0) {
            if (bound_) {
                EXPECT_EQ(0, ioctl(loop_, LOOP_CLR_FD, 0)) << strerror(errno);
            }
            close(loop_);
        }
        if (backing_ >= 0) close(backing_);
        if (!image_.empty()) unlink(image_.c_str());
    }

    void Bind() {
        ASSERT_EQ(0, ioctl(loop_, LOOP_SET_FD, backing_)) << strerror(errno);
        bound_ = true;
    }

    void ExpectCapacity(uint64_t bytes) {
        uint64_t result = UINT64_MAX;
        ASSERT_EQ(0, ioctl(loop_, BLKGETSIZE64, &result)) << strerror(errno);
        EXPECT_EQ(bytes, result);
        unsigned long sectors = ~0UL;
        ASSERT_EQ(0, ioctl(loop_, BLKGETSIZE, &sectors)) << strerror(errno);
        EXPECT_EQ(bytes / 512, sectors);
    }

    void ExpectLoopGeometry() {
        for (unsigned long cmd : {BLKSSZGET, BLKPBSZGET, BLKIOMIN}) {
            uint32_t size = 0;
            ASSERT_EQ(0, ioctl(loop_, cmd, &size)) << std::hex << cmd << ": " << strerror(errno);
            EXPECT_EQ(512U, size);
        }
        for (unsigned long cmd : {BLKIOOPT, BLKALIGNOFF}) {
            uint32_t result = UINT32_MAX;
            ASSERT_EQ(0, ioctl(loop_, cmd, &result)) << std::hex << cmd << ": " << strerror(errno);
            EXPECT_EQ(0U, result);
        }
    }

    int backing_ = -1;
    int loop_ = -1;
    bool bound_ = false;
    std::string image_;
    std::string loop_path_;
};

TEST_F(BlockDeviceIoctl, UnboundAndClearedDevicesHaveZeroCapacity) {
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(0));
    ASSERT_NO_FATAL_FAILURE(ExpectLoopGeometry());
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(kBytes / 512 * 512));
    ASSERT_EQ(0, ioctl(loop_, LOOP_CLR_FD, 0)) << strerror(errno);
    bound_ = false;
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(0));
    ASSERT_NO_FATAL_FAILURE(ExpectLoopGeometry());
}

TEST_F(BlockDeviceIoctl, BoundCapacityAndGeometryUseSectorUnits) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    ASSERT_EQ(137, lseek(loop_, 137, SEEK_SET));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(kBytes / 512 * 512));
    ASSERT_NO_FATAL_FAILURE(ExpectLoopGeometry());
    EXPECT_EQ(137, lseek(loop_, 0, SEEK_CUR));
}

TEST_F(BlockDeviceIoctl, OffsetLimitAndExplicitRefreshPublishLiveCapacity) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    loop_info64 status = {};
    status.lo_offset = 513;
    status.lo_sizelimit = 1701;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(1536));
    status.lo_sizelimit = 0;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status)) << strerror(errno);
    const uint64_t previous = (kBytes - 513) / 512 * 512;
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(previous));
    ASSERT_EQ(0, ftruncate(backing_, kBytes + 8192)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(previous));
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_CAPACITY, 0)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity((kBytes + 8192 - 513) / 512 * 512));
    ASSERT_EQ(0, ftruncate(backing_, 513 + 1025)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity((kBytes + 8192 - 513) / 512 * 512));
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_CAPACITY, 0)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(1024));
}

TEST_F(BlockDeviceIoctl, ByteOffsetMappingHonorsPublishedSectorCapacity) {
    std::array<unsigned char, 4096> backing;
    for (size_t i = 0; i < backing.size(); ++i) backing[i] = (i * 31) % 251;
    ASSERT_EQ(static_cast<ssize_t>(backing.size()),
              pwrite(backing_, backing.data(), backing.size(), 0));
    ASSERT_NO_FATAL_FAILURE(Bind());
    loop_info64 status {};
    status.lo_offset = 513;
    status.lo_sizelimit = 1701;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(1536));
    loop_info64 observed {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &observed));
    EXPECT_EQ(513U, observed.lo_offset);
    EXPECT_EQ(1701U, observed.lo_sizelimit);

    std::array<unsigned char, 2048> bytes;
    bytes.fill(0xA5);
    ASSERT_EQ(1536, pread(loop_, bytes.data(), bytes.size(), 0)) << strerror(errno);
    EXPECT_EQ(0, memcmp(backing.data() + 513, bytes.data(), 1536));
    for (size_t i = 1536; i < bytes.size(); ++i) EXPECT_EQ(0xA5, bytes[i]);
    ASSERT_EQ(1, pread(loop_, bytes.data(), 2, 1535)) << strerror(errno);
    EXPECT_EQ(backing[513 + 1535], bytes[0]);
    EXPECT_EQ(0, pread(loop_, bytes.data(), 1, 1536));

    const char tail[] = "XY";
    ASSERT_EQ(1, pwrite(loop_, tail, 2, 1535)) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, pwrite(loop_, tail, 1, 1536));
    EXPECT_EQ(ENOSPC, errno);
    ASSERT_EQ(0, fsync(loop_));
    std::array<unsigned char, 2> disk_tail {};
    ASSERT_EQ(2, pread(backing_, disk_tail.data(), disk_tail.size(), 513 + 1535));
    EXPECT_EQ('X', disk_tail[0]);
    EXPECT_EQ(backing[513 + 1536], disk_tail[1]);

    // O_DIRECT on the loop node keeps its own logical-sector alignment;
    // the normal buffered backing maps that sector to byte offset 513.
    int direct = open(loop_path_.c_str(), O_RDONLY | O_DIRECT);
    ASSERT_GE(direct, 0) << strerror(errno);
    void* aligned = nullptr;
    const int allocation = posix_memalign(&aligned, 512, 512);
    if (allocation != 0) { close(direct); FAIL() << strerror(allocation); }
    const ssize_t count = pread(direct, aligned, 512, 0);
    EXPECT_EQ(512, count) << strerror(errno);
    if (count == 512) EXPECT_EQ(0, memcmp(backing.data() + 513, aligned, 512));
    free(aligned);
    EXPECT_EQ(0, close(direct));
}

TEST_F(BlockDeviceIoctl, EmptyMappedRangesAndOverflowKeepConfigurationCoherent) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    loop_info64 status {};
    status.lo_offset = 513;
    status.lo_sizelimit = 511;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(0));
    char byte = 0;
    EXPECT_EQ(0, pread(loop_, &byte, 1, 0));
    errno = 0;
    EXPECT_EQ(-1, pwrite(loop_, &byte, 1, 0));
    EXPECT_EQ(ENOSPC, errno);
    status.lo_offset = kBytes + 1025;
    status.lo_sizelimit = 0;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(0));
    EXPECT_EQ(0, pread(loop_, &byte, 1, 0));
    loop_info64 invalid = status;
    invalid.lo_offset = UINT64_MAX;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_STATUS64, &invalid));
    EXPECT_EQ(EOVERFLOW, errno);
    invalid = status;
    invalid.lo_sizelimit = UINT64_MAX;
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, LOOP_SET_STATUS64, &invalid));
    EXPECT_EQ(EOVERFLOW, errno);
    loop_info64 observed {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &observed));
    EXPECT_EQ(status.lo_offset, observed.lo_offset);
    EXPECT_EQ(status.lo_sizelimit, observed.lo_sizelimit);
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(0));
}

TEST_F(BlockDeviceIoctl, OutputWidthsAndUnalignedPointersMatchTheAbi) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    struct Query { unsigned long command; size_t width; uint64_t expected; };
    for (const Query query : {
             Query{BLKGETSIZE64, sizeof(uint64_t), kBytes / 512 * 512},
             Query{BLKGETSIZE, sizeof(unsigned long), kBytes / 512},
             Query{BLKSSZGET, sizeof(int), 512},
             Query{BLKPBSZGET, sizeof(unsigned int), 512},
             Query{BLKIOMIN, sizeof(unsigned int), 512},
             Query{BLKIOOPT, sizeof(unsigned int), 0},
             Query{BLKALIGNOFF, sizeof(int), 0}}) {
        std::array<unsigned char, 24> output;
        output.fill(0xA5);
        ASSERT_EQ(0, ioctl(loop_, query.command, output.data() + 1)) << strerror(errno);
        uint64_t value = 0;
        memcpy(&value, output.data() + 1, query.width);
        EXPECT_EQ(query.expected, value) << std::hex << query.command;
        EXPECT_EQ(0xA5, output.front());
        for (size_t i = query.width + 1; i < output.size(); ++i) {
            EXPECT_EQ(0xA5, output[i]) << "overwrote byte " << i;
        }
    }
}

TEST_F(BlockDeviceIoctl, UserCopyFaultsLeaveQueriesUsable) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);
    void* unavailable = mmap(nullptr, page_size, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, unavailable);
    for (unsigned long cmd : std::array<unsigned long, 7>{BLKGETSIZE64, BLKGETSIZE, BLKSSZGET, BLKPBSZGET,
                             BLKIOMIN, BLKIOOPT, BLKALIGNOFF}) {
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_, cmd, nullptr));
        EXPECT_EQ(EFAULT, errno) << std::hex << cmd;
        errno = 0;
        EXPECT_EQ(-1, ioctl(loop_, cmd, unavailable));
        EXPECT_EQ(EFAULT, errno) << std::hex << cmd;
    }
    EXPECT_EQ(0, munmap(unavailable, page_size));
    ASSERT_NO_FATAL_FAILURE(ExpectCapacity(kBytes / 512 * 512));
}

TEST_F(BlockDeviceIoctl, ReadOnlyDescriptorAndUnprivilegedCallerMayQuery) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    int read_only = open(loop_path_.c_str(), O_RDONLY);
    ASSERT_GE(read_only, 0) << strerror(errno);
    pid_t child = fork();
    if (child < 0) {
        const int error = errno;
        close(read_only);
        FAIL() << strerror(error);
    }
    if (child == 0) {
        if (setgid(65534) || setuid(65534)) _exit(1);
        uint64_t bytes = 0;
        int logical = 0;
        _exit(ioctl(read_only, BLKGETSIZE64, &bytes) == 0 &&
              bytes == kBytes / 512 * 512 && ioctl(read_only, BLKSSZGET, &logical) == 0 &&
              logical == 512 ? 0 : 2);
    }
    close(read_only);
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(BlockDeviceIoctl, UnknownLoopCommandUsesEinvalBeforeTouchingItsArgument) {
    ASSERT_NO_FATAL_FAILURE(Bind());
    errno = 0;
    EXPECT_EQ(-1, ioctl(loop_, _IO(0x12, 255), reinterpret_cast<void*>(1)));
    EXPECT_EQ(EINVAL, errno);
}

TEST(BlockDeviceIoctlNodes, PartitionCapacityIsNotTheWholeDiskCapacity) {
    // The dunitest guest root image has a DOS partition table. The host
    // reference supplies a private MBR loop image through these two paths.
    const char* whole_path = getenv("DUNITEST_BLOCK_WHOLE");
    const char* part_path = getenv("DUNITEST_BLOCK_PARTITION");
    if (!whole_path) whole_path = access("/dev/vda", F_OK) == 0 ? "/dev/vda" : "/dev/sda";
    std::string default_partition = std::string(whole_path) + "1";
    if (!part_path) part_path = default_partition.c_str();
    int whole = open(whole_path, O_RDONLY);
    ASSERT_GE(whole, 0) << whole_path << ": " << strerror(errno);
    int partition = open(part_path, O_RDONLY);
    if (partition < 0) {
        close(whole);
        FAIL() << part_path << ": " << strerror(errno);
    }
    std::array<unsigned char, 512> mbr = {};
    const ssize_t read_count = pread(whole, mbr.data(), mbr.size(), 0);
    uint64_t disk_bytes = 0, partition_bytes = 0;
    const int whole_result = ioctl(whole, BLKGETSIZE64, &disk_bytes);
    const int part_result = ioctl(partition, BLKGETSIZE64, &partition_bytes);
    struct stat metadata = {};
    const int stat_result = fstat(whole, &metadata);
    errno = 0;
    const int unknown_result = ioctl(whole, _IO(0x12, 255), reinterpret_cast<void*>(1));
    const int unknown_errno = errno;
    close(partition);
    close(whole);
    ASSERT_EQ(static_cast<ssize_t>(mbr.size()), read_count);
    ASSERT_EQ(0x55, mbr[510]);
    ASSERT_EQ(0xAA, mbr[511]);
    ASSERT_NE(0xEE, mbr[450]) << "use a private DOS-partitioned image on the host";
    auto le32 = [&mbr](size_t position) {
        uint32_t value = 0;
        for (size_t i = 0; i < 4; ++i) value |= uint32_t(mbr[position + i]) << (8 * i);
        return value;
    };
    const uint64_t start = le32(454), sectors = le32(458);
    ASSERT_GT(start, 0U);
    ASSERT_GT(sectors, 0U);
    ASSERT_EQ(0, whole_result);
    ASSERT_EQ(0, part_result);
    EXPECT_EQ(sectors * 512, partition_bytes);
    EXPECT_GE(disk_bytes, (start + sectors) * 512);
    EXPECT_GT(disk_bytes, partition_bytes);
    ASSERT_EQ(0, stat_result);
    EXPECT_EQ(-1, unknown_result);
    EXPECT_EQ(major(metadata.st_rdev) == 7 ? EINVAL : ENOTTY, unknown_errno);
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
