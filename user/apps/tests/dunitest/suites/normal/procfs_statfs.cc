#include <gtest/gtest.h>

#include <cerrno>
#include <climits>
#include <cstring>
#include <fcntl.h>
#include <sys/statfs.h>
#include <unistd.h>

namespace {
constexpr long kProcfsMagic = 0x9fa0;

void ExpectProcfsMetadata(const struct statfs& info, long page_size) {
    EXPECT_EQ(kProcfsMagic, info.f_type);
    EXPECT_EQ(page_size, info.f_bsize);
    EXPECT_EQ(page_size, info.f_frsize);
    EXPECT_EQ(NAME_MAX, info.f_namelen);
}

TEST(ProcfsStatfs, ReportsMetadataByPathAndFd) {
    const long page_size = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page_size, 0);

    for (const char* path : {"/proc", "/proc/self", "/proc/uptime"}) {
        SCOPED_TRACE(path);
        {
            SCOPED_TRACE("statfs");
            struct statfs info {};
            ASSERT_EQ(0, statfs(path, &info)) << strerror(errno);
            ExpectProcfsMetadata(info, page_size);
        }
        {
            SCOPED_TRACE("fstatfs");
            const int fd = open(path, O_RDONLY | O_CLOEXEC);
            ASSERT_GE(fd, 0) << strerror(errno);
            struct statfs info {};
            const int result = fstatfs(fd, &info);
            const int error = errno;
            EXPECT_EQ(0, close(fd)) << strerror(errno);
            ASSERT_EQ(0, result) << strerror(error);
            ExpectProcfsMetadata(info, page_size);
        }
    }
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
