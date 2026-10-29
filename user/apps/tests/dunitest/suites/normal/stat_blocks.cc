#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"
#include <fcntl.h>
#include <sys/mount.h>
#include <sys/stat.h>
#include <unistd.h>
#include <cerrno>
#include <cstring>
#include <string>
#include <vector>

namespace {
class Fd {
public:
    explicit Fd(int fd) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    int get() const { return fd_; }
private:
    int fd_;
};

void expect_blocks(const std::string& path, int fd, blkcnt_t sectors) {
    struct stat by_fd {}, by_path {};
    ASSERT_EQ(0, fstat(fd, &by_fd)) << strerror(errno);
    ASSERT_EQ(0, stat(path.c_str(), &by_path)) << strerror(errno);
    struct statx extended {};
    ASSERT_EQ(0, statx(fd, "", AT_EMPTY_PATH, STATX_SIZE | STATX_BLOCKS, &extended))
        << strerror(errno);
    EXPECT_NE(0u, extended.stx_mask & STATX_BLOCKS);
    EXPECT_EQ(sectors, by_fd.st_blocks);
    EXPECT_EQ(sectors, by_path.st_blocks);
    EXPECT_EQ(static_cast<uint64_t>(sectors), extended.stx_blocks);
    EXPECT_EQ(static_cast<uint64_t>(by_fd.st_size), extended.stx_size);
}

class MemoryBlocks : public ::testing::Test {
protected:
    void mount_fs(const char* type) {
        char directory[] = "/tmp/stat_blocks_XXXXXX";
        char* result = mkdtemp(directory);
        ASSERT_NE(nullptr, result) << strerror(errno);
        root_ = result;
        ASSERT_EQ(0, mount("none", root_.c_str(), type, 0, nullptr)) << strerror(errno);
        mounted_ = true;
        page_ = sysconf(_SC_PAGESIZE);
        ASSERT_GT(page_, 0u);
    }
    void TearDown() override {
        if (mounted_) { EXPECT_EQ(0, umount(root_.c_str())) << strerror(errno); }
        if (!root_.empty()) { EXPECT_EQ(0, rmdir(root_.c_str())) << strerror(errno); }
    }
    std::string root_;
    size_t page_ = 0;
    bool mounted_ = false;
};

TEST_F(MemoryBlocks, TmpfsSparseEofCountsOnlyAllocatedPages) {
    ASSERT_NO_FATAL_FAILURE(mount_fs("tmpfs"));
    const std::string path = root_ + "/sparse";
    Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 0));
    ASSERT_EQ(0, ftruncate(fd.get(), page_ * 128 + 1));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 0));
    ASSERT_EQ(1, pwrite(fd.get(), "x", 1, page_ * 17 + 5));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), page_ / 512));
}

TEST_F(MemoryBlocks, TmpfsKeepAndPunchChangeAllocationNotIoHint) {
    ASSERT_NO_FATAL_FAILURE(mount_fs("tmpfs"));
    const std::string path = root_ + "/reserved";
    Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_KEEP_SIZE, page_ * 2, page_ * 3));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), page_ * 3 / 512));
    struct stat st {}; ASSERT_EQ(0, fstat(fd.get(), &st)); EXPECT_EQ(0, st.st_size);
    ASSERT_EQ(0, ftruncate(fd.get(), page_ * 8));
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE, page_ * 3, page_));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), page_ * 2 / 512));
}

TEST_F(MemoryBlocks, TmpfsInlineAndPageSymlinksUse512ByteUnits) {
    ASSERT_NO_FATAL_FAILURE(mount_fs("tmpfs"));
    const std::string short_path = root_ + "/short", long_path = root_ + "/long";
    ASSERT_EQ(0, symlink("target", short_path.c_str()));
    const std::string target(256, 'a');
    ASSERT_EQ(0, symlink(target.c_str(), long_path.c_str()));
    struct stat short_stat {}, long_stat {};
    ASSERT_EQ(0, lstat(short_path.c_str(), &short_stat));
    ASSERT_EQ(0, lstat(long_path.c_str(), &long_stat));
    EXPECT_EQ(0, short_stat.st_blocks);
    EXPECT_EQ(static_cast<blkcnt_t>(page_ / 512), long_stat.st_blocks);
}

TEST_F(MemoryBlocks, RamfsCountsResidentPagesRatherThanSparseEof) {
    ASSERT_NO_FATAL_FAILURE(mount_fs("ramfs"));
    const std::string path = root_ + "/sparse";
    Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, ftruncate(fd.get(), page_ * 64));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 0));
    ASSERT_EQ(1, pwrite(fd.get(), "x", 1, page_ * 7 + 9));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), page_ / 512));
}

TEST(StatBlocks, Fat16FixedRootReportsClusterRoundedSectors) {
    dunitest::LoopExt4 loop;
    ASSERT_NO_FATAL_FAILURE(loop.SetUp("fat_stat_blocks.img"));
    ASSERT_NO_FATAL_FAILURE(loop.Mount(0, "vfat"));
    struct stat root {}; ASSERT_EQ(0, stat(loop.mount_point().c_str(), &root));
    // Its 16 fixed root dirents occupy 512 bytes, but Linux reports this
    // fixed-root exception rounded to the 2048-byte cluster size.
    EXPECT_EQ(4, root.st_blocks);
}

TEST(StatBlocks, Fat32CountsOnlyItsAllocatedClusterChain) {
    dunitest::LoopExt4 loop;
    ASSERT_NO_FATAL_FAILURE(loop.SetUp("fat32_stat_blocks.img"));
    ASSERT_NO_FATAL_FAILURE(loop.Mount(0, "vfat"));
    const std::string path = loop.mount_point() + "/blocks";
    Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
    ASSERT_GE(fd.get(), 0) << "create FAT file: errno=" << errno << " " << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 0));
    ASSERT_EQ(1, pwrite(fd.get(), "x", 1, 0)); ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 1)); // 512-byte cluster.
    std::vector<char> payload(4097, 'y');
    ASSERT_EQ(static_cast<ssize_t>(payload.size()), pwrite(fd.get(), payload.data(), payload.size(), 0));
    ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 9));
    ASSERT_EQ(0, ftruncate(fd.get(), 1)); ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 1));
    ASSERT_EQ(0, ftruncate(fd.get(), 0)); ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 0));
}

TEST(StatBlocks, Ext4PendingDataReservationsAreVisibleBeforeAndAfterWriteback) {
    dunitest::LoopExt4 loop;
    ASSERT_NO_FATAL_FAILURE(loop.SetUp());
    ASSERT_NO_FATAL_FAILURE(loop.Mount());
    const std::string path = loop.mount_point() + "/pending";
    Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 0));
    const size_t page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0u);
    std::vector<char> payload(page * 3, 'z');
    ASSERT_EQ(static_cast<ssize_t>(payload.size()), pwrite(fd.get(), payload.data(), payload.size(), 0));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), payload.size() / 512));
    ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), payload.size() / 512));
}

TEST(StatBlocks, Fat32RepeatedLargeChainStatsRemainCorrectAfterChainChanges) {
    dunitest::LoopExt4 loop;
    ASSERT_NO_FATAL_FAILURE(loop.SetUp("fat32_stat_blocks.img"));
    ASSERT_NO_FATAL_FAILURE(loop.Mount(0, "vfat"));
    const std::string path = loop.mount_point() + "/large";
    Fd fd(open(path.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    // This fixture has 512-byte clusters. Exceed the FAT entry LRU capacity,
    // without imposing a machine-dependent timing threshold on CI.
    std::vector<char> payload(4097 * 512, 'a');
    ASSERT_EQ(static_cast<ssize_t>(payload.size()), pwrite(fd.get(), payload.data(), payload.size(), 0));
    ASSERT_EQ(0, fsync(fd.get()));
    for (int i = 0; i < 4; ++i) {
        ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 4097));
    }
    Fd other(open((loop.mount_point() + "/other").c_str(), O_CREAT | O_EXCL | O_RDWR, 0600));
    ASSERT_GE(other.get(), 0);
    ASSERT_EQ(1, write(other.get(), "b", 1)); ASSERT_EQ(0, fsync(other.get()));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 4097));
    ASSERT_EQ(0, ftruncate(fd.get(), 1)); ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 1));
    ASSERT_EQ(0, ftruncate(fd.get(), 0)); ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(expect_blocks(path, fd.get(), 0));
}
} // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
