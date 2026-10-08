#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"
#include <errno.h>
#include <fcntl.h>
#include <numeric>
#include <string.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/statfs.h>
#include <unistd.h>
#include <algorithm>
#include <string>
#include <vector>

namespace {
class Fd {
public:
    explicit Fd(int fd = -1) : fd_(fd) {}
    ~Fd() { if (fd_ >= 0) close(fd_); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
    int get() const { return fd_; }
    void close_now() { if (fd_ >= 0) close(fd_); fd_ = -1; }
private:
    int fd_;
};
class Mapping {
public:
    Mapping(int fd, size_t size, int flags) : size_(size) {
        data_ = mmap(nullptr, size, PROT_READ | PROT_WRITE, flags, fd, 0);
    }
    ~Mapping() { if (data_ != MAP_FAILED) munmap(data_, size_); }
    char* get() const { return static_cast<char*>(data_); }
private:
    void* data_;
    size_t size_;
};

class Ext4FallocateTest : public ::testing::Test {
protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(loop_.SetUp());
        ASSERT_NO_FATAL_FAILURE(loop_.Mount());
        struct statfs fs {}; ASSERT_EQ(0, statfs(loop_.mount_point().c_str(), &fs));
        ASSERT_GT(fs.f_bsize, 0); block_ = fs.f_bsize;
        const long page = sysconf(_SC_PAGESIZE); ASSERT_GT(page, 0); page_ = page;
        unit_ = std::lcm(block_, page_);
        path_ = loop_.mount_point() + "/allocation";
    }
    struct stat status(int fd) {
        struct stat st {};
        EXPECT_EQ(0, fstat(fd, &st)) << strerror(errno);
        return st;
    }
    void contents(int fd, const std::vector<char>& expected) {
        std::vector<char> actual(expected.size());
        ASSERT_EQ(static_cast<ssize_t>(actual.size()), pread(fd, actual.data(), actual.size(), 0));
        const auto mismatch = std::mismatch(expected.begin(), expected.end(), actual.begin());
        ASSERT_TRUE(mismatch.first == expected.end())
            << "first mismatch at byte " << std::distance(expected.begin(), mismatch.first)
            << ", expected=" << (mismatch.first == expected.end() ? -1 : static_cast<unsigned char>(*mismatch.first))
            << ", actual=" << (mismatch.second == actual.end() ? -1 : static_cast<unsigned char>(*mismatch.second));
    }
    void initialized(int fd, const std::vector<char>& bytes) {
        ASSERT_EQ(static_cast<ssize_t>(bytes.size()), pwrite(fd, bytes.data(), bytes.size(), 0));
        ASSERT_EQ(0, fsync(fd));
    }
    static constexpr int kPunch = FALLOC_FL_PUNCH_HOLE | FALLOC_FL_KEEP_SIZE;
    dunitest::LoopExt4 loop_;
    std::string path_;
    size_t block_ = 0, page_ = 0, unit_ = 0;
};

TEST_F(Ext4FallocateTest, KeepSizeReallyReservesBlocksAndExposesZeroesWithoutGrowth) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    const auto before = status(fd.get());
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_KEEP_SIZE, block_ * 2, block_ * 4));
    ASSERT_EQ(0, fsync(fd.get()));
    const auto reserved = status(fd.get());
    EXPECT_EQ(0, reserved.st_size);
    EXPECT_GE(reserved.st_blocks - before.st_blocks, static_cast<blkcnt_t>(block_ * 4 / 512));
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_KEEP_SIZE, block_ * 2, block_ * 4));
    ASSERT_EQ(0, fsync(fd.get())); EXPECT_EQ(reserved.st_blocks, status(fd.get()).st_blocks);
    // Growth merely exposes the preallocation; it must not discard it or
    // return stale disk contents from its unwritten extents.
    ASSERT_EQ(0, ftruncate(fd.get(), block_ * 6)); ASSERT_EQ(0, fsync(fd.get()));
    EXPECT_EQ(static_cast<off_t>(block_ * 6), status(fd.get()).st_size);
    EXPECT_EQ(reserved.st_blocks, status(fd.get()).st_blocks);
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), std::vector<char>(block_ * 6, 0)));
}

TEST_F(Ext4FallocateTest, AllocationGrowsAndRepeatedAllocationPreservesWrittenDataOnDisk) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    std::vector<char> expected(block_ * 8, 0);
    std::fill(expected.begin(), expected.begin() + block_ * 2, 's');
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), std::vector<char>(block_ * 2, 's')));
    const auto before = status(fd.get());
    ASSERT_EQ(0, fallocate(fd.get(), 0, block_ * 4, block_ * 4)); ASSERT_EQ(0, fsync(fd.get()));
    const auto allocated = status(fd.get());
    EXPECT_EQ(static_cast<off_t>(expected.size()), allocated.st_size);
    EXPECT_GE(allocated.st_blocks - before.st_blocks, static_cast<blkcnt_t>(block_ * 4 / 512));
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    ASSERT_EQ(1, pwrite(fd.get(), "x", 1, block_ * 5 + 19)); expected[block_ * 5 + 19] = 'x';
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_KEEP_SIZE, block_ * 4, block_ * 4));
    ASSERT_EQ(0, fsync(fd.get()));
    EXPECT_EQ(allocated.st_blocks, status(fd.get()).st_blocks);
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    fd.close_now();
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ASSERT_NO_FATAL_FAILURE(loop_.Mount());
    Fd reopened(open(path_.c_str(), O_RDONLY)); ASSERT_GE(reopened.get(), 0);
    ASSERT_NO_FATAL_FAILURE(contents(reopened.get(), expected));
    EXPECT_EQ(allocated.st_blocks, status(reopened.get()).st_blocks);
}

TEST_F(Ext4FallocateTest, AlignedHoleReleasesBlocksWithoutChangingSizeOrOutsideData) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    std::vector<char> expected(block_ * 6, 'a');
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), expected));
    const auto before = status(fd.get());
    ASSERT_EQ(0, fallocate(fd.get(), kPunch, block_ * 2, block_ * 2)); ASSERT_EQ(0, fsync(fd.get()));
    std::fill(expected.begin() + block_ * 2, expected.begin() + block_ * 4, 0);
    const auto punched = status(fd.get());
    EXPECT_EQ(before.st_size, punched.st_size);
    EXPECT_GE(before.st_blocks - punched.st_blocks, static_cast<blkcnt_t>(block_ * 2 / 512));
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    ASSERT_EQ(0, fallocate(fd.get(), kPunch, block_ * 2, block_ * 2));
    ASSERT_EQ(0, fallocate(fd.get(), kPunch, block_ * 20, block_ * 3));
    ASSERT_EQ(0, fsync(fd.get()));
    EXPECT_EQ(punched.st_blocks, status(fd.get()).st_blocks);
    EXPECT_EQ(before.st_size, status(fd.get()).st_size);
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
}

TEST_F(Ext4FallocateTest, TruncateGrowthPreservesPendingPartialAppend) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), std::vector<char>(37, 's')));
    const std::vector<char> append(63, 'd');
    ASSERT_EQ(static_cast<ssize_t>(append.size()), pwrite(fd.get(), append.data(), append.size(), 37));
    ASSERT_EQ(0, ftruncate(fd.get(), block_ * 2 + 17));
    std::vector<char> expected(block_ * 2 + 17, 0);
    std::fill(expected.begin(), expected.begin() + 37, 's');
    std::copy(append.begin(), append.end(), expected.begin() + 37);
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    ASSERT_EQ(0, fsync(fd.get()));
    fd.close_now();
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ASSERT_NO_FATAL_FAILURE(loop_.Mount());
    Fd reopened(open(path_.c_str(), O_RDONLY)); ASSERT_GE(reopened.get(), 0);
    ASSERT_NO_FATAL_FAILURE(contents(reopened.get(), expected));
}

TEST_F(Ext4FallocateTest, TruncateGrowthUnmapsSharedOldEofBeforeZeroingTail) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), std::vector<char>(37, 's')));
    Mapping shared(fd.get(), unit_, MAP_SHARED);
    ASSERT_NE(MAP_FAILED, shared.get());
    shared.get()[11] = 'd';
    ASSERT_EQ(0, ftruncate(fd.get(), unit_ * 2 + 17));
    std::vector<char> expected(unit_ * 2 + 17, 0);
    std::fill(expected.begin(), expected.begin() + 37, 's');
    expected[11] = 'd';
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    EXPECT_EQ('d', shared.get()[11]);
    ASSERT_EQ(0, fsync(fd.get()));
}

TEST_F(Ext4FallocateTest, TruncateToZeroReleasesWrittenAndUnwrittenBlocks) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), std::vector<char>(block_ * 3, 'w')));
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_KEEP_SIZE, block_ * 8, block_ * 4));
    ASSERT_EQ(0, fsync(fd.get()));
    EXPECT_GE(status(fd.get()).st_blocks, static_cast<blkcnt_t>(block_ * 7 / 512));
    ASSERT_EQ(0, ftruncate(fd.get(), 0)); ASSERT_EQ(0, fsync(fd.get()));
    EXPECT_EQ(0, status(fd.get()).st_size);
    EXPECT_EQ(0, status(fd.get()).st_blocks);
    ASSERT_EQ(0, ftruncate(fd.get(), block_ * 12));
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), std::vector<char>(block_ * 12, 0)));
    ASSERT_EQ(0, fsync(fd.get()));
    EXPECT_EQ(0, status(fd.get()).st_blocks);
    fd.close_now();
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ASSERT_NO_FATAL_FAILURE(loop_.Mount());
    Fd reopened(open(path_.c_str(), O_RDONLY)); ASSERT_GE(reopened.get(), 0);
    ASSERT_NO_FATAL_FAILURE(contents(reopened.get(), std::vector<char>(block_ * 12, 0)));
    EXPECT_EQ(0, status(reopened.get()).st_blocks);
}

TEST_F(Ext4FallocateTest, ShrinkPreservesDirtyPrefixAcrossRegrowthAndRemount) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), std::vector<char>(block_ * 3, 's')));
    ASSERT_EQ(1, pwrite(fd.get(), "d", 1, 11));
    const size_t eof = 37;
    ASSERT_EQ(0, ftruncate(fd.get(), eof));
    std::vector<char> prefix(eof, 's'); prefix[11] = 'd';
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), prefix));
    ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_EQ(0, ftruncate(fd.get(), block_ * 3));
    std::vector<char> expected(block_ * 3, 0);
    std::copy(prefix.begin(), prefix.end(), expected.begin());
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    ASSERT_EQ(0, fsync(fd.get()));
    fd.close_now();
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ASSERT_NO_FATAL_FAILURE(loop_.Mount());
    Fd reopened(open(path_.c_str(), O_RDONLY)); ASSERT_GE(reopened.get(), 0);
    ASSERT_NO_FATAL_FAILURE(contents(reopened.get(), expected));
}

TEST_F(Ext4FallocateTest, TruncateOfAnonymousFilePreservesItsRelinkLifecycle) {
    Fd fd(open(loop_.mount_point().c_str(), O_TMPFILE | O_RDWR, 0600));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), std::vector<char>(block_ * 3, 'a')));
    ASSERT_EQ(0, ftruncate(fd.get(), 37));
    ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_EQ(0, linkat(fd.get(), "", AT_FDCWD, path_.c_str(), AT_EMPTY_PATH)) << strerror(errno);
    ASSERT_EQ(0, ftruncate(fd.get(), block_ * 3));
    std::vector<char> expected(block_ * 3, 0);
    std::fill(expected.begin(), expected.begin() + 37, 'a');
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    ASSERT_EQ(0, fsync(fd.get()));
    fd.close_now();
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ASSERT_NO_FATAL_FAILURE(loop_.Mount());
    Fd reopened(open(path_.c_str(), O_RDONLY)); ASSERT_GE(reopened.get(), 0);
    ASSERT_NO_FATAL_FAILURE(contents(reopened.get(), expected));
}

TEST_F(Ext4FallocateTest, GrowthPreservesDirtyOldEofPrefixAndZeroesTheExposedTail) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    // Start with a real mapped block, then shrink inside it. Growth must not
    // expose its old tail or replace the valid, currently dirty prefix with
    // an older disk image while preparing the zero tail.
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), std::vector<char>(block_ * 2, 's')));
    const size_t old_eof = 37;
    ASSERT_EQ(0, ftruncate(fd.get(), old_eof)); ASSERT_EQ(0, fsync(fd.get()));
    std::vector<char> expected(block_ * 3, 0);
    std::fill(expected.begin(), expected.begin() + old_eof, 's');
    ASSERT_EQ(1, pwrite(fd.get(), "d", 1, 11)); expected[11] = 'd';
    ASSERT_EQ(0, fallocate(fd.get(), 0, old_eof, expected.size() - old_eof));
    EXPECT_EQ(static_cast<off_t>(expected.size()), status(fd.get()).st_size);
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    ASSERT_EQ(0, fsync(fd.get()));
    fd.close_now();
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ASSERT_NO_FATAL_FAILURE(loop_.Mount());
    Fd reopened(open(path_.c_str(), O_RDONLY)); ASSERT_GE(reopened.get(), 0);
    ASSERT_NO_FATAL_FAILURE(contents(reopened.get(), expected));
}

TEST_F(Ext4FallocateTest, SameSizeTruncateReleasesPreallocationBeyondEof) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    const std::vector<char> expected(block_, 'p');
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), expected));
    const auto original = status(fd.get());
    ASSERT_EQ(0, fallocate(fd.get(), FALLOC_FL_KEEP_SIZE, block_, block_ * 6));
    ASSERT_EQ(0, fsync(fd.get()));
    const auto reserved = status(fd.get());
    EXPECT_EQ(original.st_size, reserved.st_size);
    EXPECT_GE(reserved.st_blocks - original.st_blocks, static_cast<blkcnt_t>(block_ * 6 / 512));
    // Linux still truncates KEEP_SIZE allocations when the requested size
    // equals the current size. An early size-equality return would leak them.
    ASSERT_EQ(0, ftruncate(fd.get(), expected.size())); ASSERT_EQ(0, fsync(fd.get()));
    const auto truncated = status(fd.get());
    EXPECT_EQ(original.st_size, truncated.st_size);
    EXPECT_EQ(original.st_blocks, truncated.st_blocks);
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    fd.close_now();
    ASSERT_NO_FATAL_FAILURE(loop_.Unmount());
    ASSERT_NO_FATAL_FAILURE(loop_.Mount());
    Fd reopened(open(path_.c_str(), O_RDONLY)); ASSERT_GE(reopened.get(), 0);
    ASSERT_NO_FATAL_FAILURE(contents(reopened.get(), expected));
    EXPECT_EQ(original.st_blocks, status(reopened.get()).st_blocks);
}

TEST_F(Ext4FallocateTest, PartialHoleZeroesEdgesAndPreservesDirtyOutsideBytes) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    std::vector<char> expected(block_ * 6, 'b');
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), expected));
    const auto before = status(fd.get());
    const size_t first = block_ + 17, end = block_ * 4 - 29;
    // Intentionally dirty both partial boundary blocks without fsync before
    // punch. Invalidating whole cache pages would lose these outside bytes.
    ASSERT_EQ(1, pwrite(fd.get(), "L", 1, first - 1)); expected[first - 1] = 'L';
    ASSERT_EQ(1, pwrite(fd.get(), "R", 1, end)); expected[end] = 'R';
    ASSERT_EQ(0, fallocate(fd.get(), kPunch, first, end - first));
    std::fill(expected.begin() + first, expected.begin() + end, 0);
    ASSERT_EQ(0, fsync(fd.get()));
    const auto punched = status(fd.get());
    EXPECT_EQ(before.st_size, punched.st_size);
    EXPECT_GE(before.st_blocks - punched.st_blocks, static_cast<blkcnt_t>(block_ / 512));
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    ASSERT_EQ(0, fallocate(fd.get(), kPunch, 11, 31)); ASSERT_EQ(0, fsync(fd.get()));
    std::fill(expected.begin() + 11, expected.begin() + 42, 0);
    EXPECT_EQ(punched.st_blocks, status(fd.get()).st_blocks);
    EXPECT_EQ(before.st_size, status(fd.get()).st_size);
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
}

TEST_F(Ext4FallocateTest, HoleInvalidatesSharedMappingButPreservesActualPrivateCow) {
    Fd fd(open(path_.c_str(), O_CREAT | O_EXCL | O_RDWR, 0600)); ASSERT_GE(fd.get(), 0);
    std::vector<char> expected(unit_ * 4, 'a');
    ASSERT_NO_FATAL_FAILURE(initialized(fd.get(), expected));
    const auto before = status(fd.get());
    Mapping shared(fd.get(), expected.size(), MAP_SHARED);
    Mapping private_cow(fd.get(), expected.size(), MAP_PRIVATE);
    ASSERT_NE(MAP_FAILED, shared.get()); ASSERT_NE(MAP_FAILED, private_cow.get());
    // Populate every shared PTE, then dirty both the soon-to-be-removed range
    // and adjacent pages. No msync before punch may hide stale-cache bugs.
    for (size_t offset = 0; offset < expected.size(); offset += page_)
        ASSERT_EQ('a', shared.get()[offset]);
    shared.get()[17] = 'L'; expected[17] = 'L';
    shared.get()[unit_ + 73] = 'd';
    shared.get()[unit_ * 2 + 11] = 'R'; expected[unit_ * 2 + 11] = 'R';
    std::vector<char> cow(unit_, 'a'); cow[73] = 'd';
    // Writes, not just reads, force genuine COW on each punched private page.
    for (size_t offset = 0; offset < unit_; offset += page_) {
        private_cow.get()[unit_ + offset] = 'C'; cow[offset] = 'C';
    }
    ASSERT_EQ(0, fallocate(fd.get(), kPunch, unit_, unit_));
    std::fill(expected.begin() + unit_, expected.begin() + unit_ * 2, 0);
    EXPECT_EQ(0, memcmp(shared.get(), expected.data(), expected.size()));
    EXPECT_EQ(0, memcmp(private_cow.get() + unit_, cow.data(), cow.size()));
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    ASSERT_EQ(0, msync(shared.get(), expected.size(), MS_SYNC)); ASSERT_EQ(0, fsync(fd.get()));
    ASSERT_NO_FATAL_FAILURE(contents(fd.get(), expected));
    const auto punched = status(fd.get());
    EXPECT_EQ(before.st_size, punched.st_size);
    EXPECT_GE(before.st_blocks - punched.st_blocks, static_cast<blkcnt_t>(unit_ / 512));
    EXPECT_EQ(0, memcmp(private_cow.get() + unit_, cow.data(), cow.size()));
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
