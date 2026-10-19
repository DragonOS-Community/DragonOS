#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"
#include <array>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <string>
#include <sys/mount.h>
#include <unistd.h>

namespace {
struct Fd {
    int value = -1;
    ~Fd() { if (value >= 0) close(value); }
};
class BlockExt4Cache : public testing::Test {
protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(fs_.SetUp("ext4_inode_identity.img", "/tmp", "block_ext4_cache"));
        raw_.value = open(fs_.loop_path().c_str(), O_RDWR);
        ASSERT_GE(raw_.value, 0) << strerror(errno);
    }
    dunitest::LoopExt4 fs_;
    Fd raw_;
};

TEST_F(BlockExt4Cache, MountReadsPriorBufferedRawSuperblockWrite) {
    constexpr off_t magic_offset = 1024 + 56;
    std::array<unsigned char, 2> magic {};
    ASSERT_EQ(2, pread(raw_.value, magic.data(), magic.size(), magic_offset));
    ASSERT_EQ((std::array<unsigned char, 2> {0x53, 0xef}), magic);
    const std::array<unsigned char, 2> invalid {};
    ASSERT_EQ(2, pwrite(raw_.value, invalid.data(), invalid.size(), magic_offset));
    // No fsync: a completed buffered write already belongs to the node's
    // visible contents, so the filesystem adapter cannot read the old magic.
    errno = 0;
    const int result = mount(fs_.loop_path().c_str(), fs_.mount_point().c_str(),
                             "ext4", MS_RDONLY, nullptr);
    const int error = errno;
    if (result == 0) EXPECT_EQ(0, umount(fs_.mount_point().c_str()));
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EINVAL, error);
    // Restore the private fixture even when an assertion above detects a bug.
    ASSERT_EQ(2, pwrite(raw_.value, magic.data(), magic.size(), magic_offset));
    ASSERT_EQ(0, fsync(raw_.value));
}

TEST_F(BlockExt4Cache, FilesystemMetadataWriteInvalidatesPriorRawRead) {
    // The 4 KiB fixture's first block-group descriptor contains the free inode
    // count and inode bitmap location. Allocating a file must change this block.
    constexpr off_t descriptor_offset = 4096;
    std::array<unsigned char, 4096> before {}, after {}, backing {};
    ASSERT_EQ(4096, pread(raw_.value, before.data(), before.size(), descriptor_offset));
    ASSERT_NO_FATAL_FAILURE(fs_.Mount());
    {
        Fd file {open((fs_.mount_point() + "/raw_cache_new_inode").c_str(),
                      O_CREAT | O_EXCL | O_RDWR, 0600)};
        ASSERT_GE(file.value, 0) << strerror(errno);
        ASSERT_EQ(0, fsync(file.value)) << strerror(errno);
    }
    ASSERT_NO_FATAL_FAILURE(fs_.Unmount());
    Fd image {open(fs_.backing_path().c_str(), O_RDONLY)};
    ASSERT_GE(image.value, 0);
    ASSERT_EQ(4096, pread(image.value, backing.data(), backing.size(), descriptor_offset));
    ASSERT_NE(before, backing) << "fixture must perform an actual metadata write";
    ASSERT_EQ(4096, pread(raw_.value, after.data(), after.size(), descriptor_offset));
    EXPECT_EQ(backing, after) << "raw cache retained filesystem metadata from before allocation";
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
