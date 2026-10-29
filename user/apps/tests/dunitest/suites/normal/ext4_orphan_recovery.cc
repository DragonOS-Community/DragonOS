#include <gtest/gtest.h>

#include "loop_ext4_test_support.h"

#include <array>
#include <cerrno>
#include <cstdio>
#include <cstring>
#include <fcntl.h>
#include <sys/stat.h>
#include <sys/statvfs.h>
#include <unistd.h>

namespace {

TEST(Ext4OrphanRecovery, MountReclaimsDurableOrphansAndPreservesLiveData) {
    dunitest::LoopExt4 fs;
    ASSERT_NO_FATAL_FAILURE(fs.SetUp("ext4_orphan_recovery.img"));
    ASSERT_NO_FATAL_FAILURE(fs.Mount());

    const std::string survivor = fs.mount_point() + "/recovery-baseline";
    std::array<char, 4096> payload{};
    int fd = open(survivor.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    const ssize_t read_count = read(fd, payload.data(), payload.size());
    const int read_error = errno;
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_EQ(static_cast<ssize_t>(payload.size()), read_count) << strerror(read_error);
    unsigned long long expected_blocks = 0;
    unsigned long long expected_inodes = 0;
    int header_size = 0;
    ASSERT_EQ(2, sscanf(payload.data(), "%llu %llu\n%n", &expected_blocks,
                        &expected_inodes, &header_size));
    ASSERT_GT(header_size, 0);
    ASSERT_LT(static_cast<size_t>(header_size), payload.size());
    for (size_t i = static_cast<size_t>(header_size); i < payload.size(); ++i) {
        ASSERT_EQ('Z', payload[i]) << "surviving data at byte " << i;
    }
    for (int index = 0; index < 2; ++index) {
        const std::string orphan = fs.mount_point() + "/recovery-orphan-"
            + std::to_string(index);
        struct stat st{};
        errno = 0;
        EXPECT_EQ(-1, stat(orphan.c_str(), &st));
        EXPECT_EQ(ENOENT, errno);
    }
    struct statvfs recovered{};
    ASSERT_EQ(0, statvfs(fs.mount_point().c_str(), &recovered)) << strerror(errno);
    EXPECT_EQ(expected_blocks, recovered.f_bfree);
    EXPECT_EQ(expected_inodes, recovered.f_ffree);

    const std::string replacement = fs.mount_point() + "/post-recovery";
    fd = open(replacement.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    const ssize_t written = write(fd, payload.data(), payload.size());
    const int synced = fsync(fd);
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_EQ(static_cast<ssize_t>(payload.size()), written);
    ASSERT_EQ(0, synced);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    fd = open(replacement.c_str(), O_RDONLY);
    ASSERT_GE(fd, 0) << strerror(errno);
    std::array<char, 4096> restored{};
    const ssize_t restored_size = read(fd, restored.data(), restored.size());
    ASSERT_EQ(0, close(fd)) << strerror(errno);
    ASSERT_EQ(static_cast<ssize_t>(restored.size()), restored_size);
    EXPECT_EQ(payload, restored);
    ASSERT_EQ(0, unlink(replacement.c_str())) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
    ASSERT_NO_FATAL_FAILURE(fs.Mount());
    ASSERT_EQ(0, statvfs(fs.mount_point().c_str(), &recovered)) << strerror(errno);
    EXPECT_EQ(expected_blocks, recovered.f_bfree);
    EXPECT_EQ(expected_inodes, recovered.f_ffree);
    ASSERT_NO_FATAL_FAILURE(fs.Unmount());
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
