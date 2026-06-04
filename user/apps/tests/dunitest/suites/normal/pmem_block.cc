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

enum class PmemProbeResult {
    Present,
    Missing,
    Error,
};

PmemProbeResult ProbePmem(struct stat* st, int* err) {
    if (stat(kPmem0, st) == 0) {
        return PmemProbeResult::Present;
    }
    *err = errno;
    return errno == ENOENT ? PmemProbeResult::Missing : PmemProbeResult::Error;
}

}  // namespace

TEST(PmemBlock, ExposesWholeDiskBlockDevice) {
    struct stat st = {};
    int err = 0;
    switch (ProbePmem(&st, &err)) {
        case PmemProbeResult::Missing:
            GTEST_SKIP() << kPmem0 << " is not present in this boot configuration";
        case PmemProbeResult::Error:
            FAIL() << "stat(" << kPmem0 << ") failed: errno=" << err << " ("
                   << std::strerror(err) << ")";
        case PmemProbeResult::Present:
            break;
    }

    EXPECT_TRUE(S_ISBLK(st.st_mode)) << kPmem0 << " is not a block device";
    EXPECT_EQ(259U, major(st.st_rdev)) << kPmem0 << " major mismatch";
    EXPECT_EQ(0U, minor(st.st_rdev)) << kPmem0 << " minor mismatch";
    EXPECT_GE(st.st_size, static_cast<off_t>(kSectorSize));
}

TEST(PmemBlock, SupportsAlignedAndPartialReads) {
    struct stat st = {};
    int err = 0;
    switch (ProbePmem(&st, &err)) {
        case PmemProbeResult::Missing:
            GTEST_SKIP() << kPmem0 << " is not present in this boot configuration";
        case PmemProbeResult::Error:
            FAIL() << "stat(" << kPmem0 << ") failed: errno=" << err << " ("
                   << std::strerror(err) << ")";
        case PmemProbeResult::Present:
            break;
    }

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
