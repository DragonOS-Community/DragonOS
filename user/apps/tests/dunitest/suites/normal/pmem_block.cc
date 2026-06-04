#include <gtest/gtest.h>

#include <array>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/sysmacros.h>
#include <unistd.h>

namespace {

constexpr const char* kPmem0 = "/dev/pmem0";
constexpr size_t kSectorSize = 512;

void SkipIfNoPmem() {
    struct stat st = {};
    if (stat(kPmem0, &st) != 0) {
        if (errno == ENOENT) {
            GTEST_SKIP() << kPmem0 << " is not present in this boot configuration";
        }
        FAIL() << "stat(" << kPmem0 << ") failed: errno=" << errno << " ("
               << std::strerror(errno) << ")";
    }
}

}  // namespace

TEST(PmemBlock, ExposesWholeDiskBlockDevice) {
    SkipIfNoPmem();

    struct stat st = {};
    ASSERT_EQ(0, stat(kPmem0, &st)) << "stat(" << kPmem0 << ") failed: errno=" << errno
                                    << " (" << std::strerror(errno) << ")";

    EXPECT_TRUE(S_ISBLK(st.st_mode)) << kPmem0 << " is not a block device";
    EXPECT_EQ(259U, major(st.st_rdev)) << kPmem0 << " major mismatch";
    EXPECT_EQ(0U, minor(st.st_rdev)) << kPmem0 << " minor mismatch";
    EXPECT_GE(st.st_size, static_cast<off_t>(kSectorSize));
}

TEST(PmemBlock, SupportsAlignedAndPartialReads) {
    SkipIfNoPmem();

    int fd = open(kPmem0, O_RDONLY);
    ASSERT_GE(fd, 0) << "open(" << kPmem0 << ") failed: errno=" << errno << " ("
                     << std::strerror(errno) << ")";

    std::array<unsigned char, kSectorSize> sector {};
    sector.fill(0xaa);
    ASSERT_EQ(static_cast<ssize_t>(sector.size()), pread(fd, sector.data(), sector.size(), 0))
        << "pread first sector failed: errno=" << errno << " (" << std::strerror(errno) << ")";

    std::array<unsigned char, 17> partial {};
    partial.fill(0x55);
    ASSERT_EQ(static_cast<ssize_t>(partial.size()), pread(fd, partial.data(), partial.size(), 3))
        << "pread partial range failed: errno=" << errno << " (" << std::strerror(errno) << ")";

    EXPECT_EQ(0, close(fd));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
