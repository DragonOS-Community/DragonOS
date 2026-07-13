#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/statfs.h>
#include <unistd.h>

#include <string>

namespace {

constexpr long kMsdosSuperMagic = 0x4D44;

std::string RootFilesystemType() {
    FILE* mounts = fopen("/proc/self/mounts", "r");
    if (mounts == nullptr) {
        return {};
    }

    char line[1024];
    char source[256];
    char mount_point[256];
    char filesystem_type[64];
    while (fgets(line, sizeof(line), mounts) != nullptr) {
        if (sscanf(line,
                   "%255s %255s %63s",
                   source,
                   mount_point,
                   filesystem_type) == 3 &&
            strcmp(mount_point, "/") == 0) {
            fclose(mounts);
            return filesystem_type;
        }
    }

    fclose(mounts);
    return {};
}

TEST(FatStatfs, ReportsLinuxAbiByPathAndFd) {
    const std::string root_filesystem_type = RootFilesystemType();
    ASSERT_FALSE(root_filesystem_type.empty()) << "cannot identify the root filesystem";
    if (root_filesystem_type != "fat" && root_filesystem_type != "vfat" &&
        root_filesystem_type != "msdos") {
        GTEST_SKIP() << "root filesystem is " << root_filesystem_type << ", not FAT";
    }

    struct statfs by_path = {};
    ASSERT_EQ(0, statfs("/", &by_path)) << strerror(errno);

    int fd = open("/", O_RDONLY | O_DIRECTORY);
    ASSERT_GE(fd, 0) << strerror(errno);
    struct statfs by_fd = {};
    ASSERT_EQ(0, fstatfs(fd, &by_fd)) << strerror(errno);
    ASSERT_EQ(0, close(fd)) << strerror(errno);

    EXPECT_EQ(kMsdosSuperMagic, by_path.f_type);
    EXPECT_EQ(kMsdosSuperMagic, by_fd.f_type);
    EXPECT_EQ(255, by_path.f_namelen);
    EXPECT_EQ(255, by_fd.f_namelen);
    EXPECT_GT(by_path.f_bsize, 0);
    EXPECT_EQ(by_path.f_bsize, by_fd.f_bsize);
    EXPECT_EQ(by_path.f_frsize, by_fd.f_frsize);
    EXPECT_LE(by_path.f_bfree, by_path.f_blocks);
    EXPECT_LE(by_path.f_bavail, by_path.f_bfree);
    EXPECT_LE(by_fd.f_bfree, by_fd.f_blocks);
    EXPECT_LE(by_fd.f_bavail, by_fd.f_bfree);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
