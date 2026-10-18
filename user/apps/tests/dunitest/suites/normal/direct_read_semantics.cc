#include "loop_ext4_test_support.h"

#include <linux/fs.h>
#include <initializer_list>
#include <stdlib.h>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <vector>
#include <memory>
#include <atomic>
#include <thread>
#include <signal.h>
#include <sys/wait.h>

namespace {
constexpr size_t kPage = 4096;
constexpr size_t kSize = 3 * 65536;

class DirectRead : public testing::TestWithParam<bool> {
 protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(disk_.SetUp(GetParam() ? "fat_stat_blocks.img" : "ext4_inode_identity.img",
                                           "/tmp", "direct_read"));
        ASSERT_NO_FATAL_FAILURE(disk_.Mount(0, GetParam() ? "vfat" : "ext4"));
        path_ = disk_.mount_point() + "/file";
        buffered_ = open(path_.c_str(), O_CREAT | O_RDWR, 0600);
        ASSERT_GE(buffered_, 0) << strerror(errno);
        std::vector<unsigned char> data(kSize, 'r');
        ASSERT_EQ(static_cast<ssize_t>(data.size()), write(buffered_, data.data(), data.size()));
        ASSERT_EQ(0, fsync(buffered_));
        direct_ = open(path_.c_str(), O_RDONLY | O_DIRECT);
        ASSERT_GE(direct_, 0) << strerror(errno);
        ASSERT_EQ(0, posix_memalign(&output_, kPage, kSize + kPage));
        memset(output_, 'x', kSize + kPage);
    }
    void TearDown() override {
        if (direct_ >= 0) close(direct_);
        if (buffered_ >= 0) close(buffered_);
        free(output_);
    }
    unsigned char* bytes() { return static_cast<unsigned char*>(output_); }
    void ExpectError(ssize_t result, int error) {
        const int actual = errno;
        EXPECT_EQ(-1, result);
        EXPECT_EQ(error, actual);
        EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    }
    dunitest::LoopExt4 disk_;
    std::string path_;
    int buffered_ = -1, direct_ = -1;
    void* output_ = nullptr;
};

TEST_P(DirectRead, OriginalScalarAddressLengthAndOffsetAreValidated) {
    errno = 0;
    ExpectError(read(direct_, bytes() + 1, kPage), EINVAL);
    errno = 0;
    ExpectError(pread(direct_, bytes(), kPage - 1, 0), EINVAL);
    errno = 0;
    ExpectError(pread(direct_, bytes(), kPage, 1), EINVAL);
    std::vector<unsigned char> untouched(kPage + 1, 'x');
    EXPECT_EQ(0, memcmp(bytes(), untouched.data(), untouched.size()));
}
TEST_P(DirectRead, LogicalSectorReadsDoNotRequireFilesystemBlockAlignment) {
    EXPECT_EQ(512, pread(direct_, bytes(), 512, 512));
    EXPECT_EQ(1024, pread(direct_, bytes(), 1024, kPage - 512));
    std::vector<unsigned char> expected(1024, 'r');
    EXPECT_EQ(0, memcmp(bytes(), expected.data(), expected.size()));
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
}
TEST_P(DirectRead, WholeVectorAlignmentPrecedesFirstDestinationCopy) {
    iovec vectors[] = {{bytes(), kPage}, {bytes() + kPage + 1, kPage}};
    errno = 0;
    ExpectError(readv(direct_, vectors, 2), EINVAL);
    std::vector<unsigned char> untouched(2 * kPage + 1, 'x');
    EXPECT_EQ(0, memcmp(bytes(), untouched.data(), untouched.size()));
    vectors[1] = {bytes() + kPage, kPage - 1};
    errno = 0;
    ExpectError(preadv(direct_, vectors, 2, 0), EINVAL);
}
TEST_P(DirectRead, AlignedVectorsKeepExplicitAndSharedPositionsSeparate) {
    iovec vectors[] = {{bytes(), kPage}, {bytes() + 65536, kPage}};
    ASSERT_EQ(2 * static_cast<ssize_t>(kPage), preadv(direct_, vectors, 2, 0));
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    ASSERT_EQ(2 * static_cast<ssize_t>(kPage), readv(direct_, vectors, 2));
    EXPECT_EQ(2 * static_cast<off_t>(kPage), lseek(direct_, 0, SEEK_CUR));
    for (size_t i = 0; i < kPage; ++i) {
        ASSERT_EQ('r', bytes()[i]);
        ASSERT_EQ('r', bytes()[65536 + i]);
    }
}
TEST_P(DirectRead, Preadv2RetainsOriginalIteratorAndOffsetContract) {
    iovec bad[] = {{bytes(), kPage}, {bytes() + kPage + 1, kPage}};
    errno = 0;
    ExpectError(syscall(SYS_preadv2, direct_, bad, 2, 0UL, 0UL, 0U), EINVAL);
    errno = 0;
    ExpectError(syscall(SYS_preadv2, direct_, bad, 2, -1UL, -1UL, 0U), EINVAL);
    iovec good[] = {{bytes(), kPage}, {reinterpret_cast<void*>(1), 0}, {bytes() + kPage, kPage}};
    errno = 0;
    if (GetParam()) {
        EXPECT_EQ(2 * static_cast<ssize_t>(kPage), syscall(SYS_preadv2, direct_, good, 3, 0UL, 0UL, 0U));
    } else {
        ExpectError(syscall(SYS_preadv2, direct_, good, 3, 0UL, 0UL, 0U), EINVAL);
    }
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
}
TEST_P(DirectRead, ZeroAndEofPrecedeAlignmentAndMappedPageFault) {
    EXPECT_EQ(0, pread(direct_, bytes() + 1, 0, 1));
    EXPECT_EQ(0, pread(direct_, reinterpret_cast<void*>(1), kPage - 1, kSize));
    auto* inaccessible = static_cast<unsigned char*>(mmap(nullptr, kPage, PROT_NONE,
                                                         MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, inaccessible);
    EXPECT_EQ(0, pread(direct_, inaccessible, kPage, kSize));
    errno = 0;
    ExpectError(pread(direct_, inaccessible, kPage, 0), EFAULT);
    errno = 0;
    ExpectError(pread(direct_, inaccessible + 1, kPage - 1, 0), EINVAL);
    EXPECT_EQ(0, munmap(inaccessible, kPage));
}
TEST_P(DirectRead, MappedEofDoesNotTruncateAlignmentIterator) {
    ASSERT_EQ(0, ftruncate(buffered_, kPage + 37));
    ASSERT_EQ(0, fsync(buffered_));
    EXPECT_EQ(37, pread(direct_, bytes(), kPage, kPage));
    for (size_t i = 0; i < 37; ++i) EXPECT_EQ('r', bytes()[i]);
    errno = 0;
    ExpectError(pread(direct_, bytes(), kPage - 1, kPage), EINVAL);
    // Unlike a raw device's capacity clipping, regular-file EOF does not
    // erase a bad later vector from the original direct request.
    iovec vectors[] = {{bytes(), kPage}, {bytes() + kPage + 1, kPage}};
    errno = 0;
    ExpectError(preadv(direct_, vectors, 2, kPage), EINVAL);
}
TEST_P(DirectRead, EmptySegmentAddressRulesMatchTheRealBackend) {
    iovec leading[] = {{reinterpret_cast<void*>(1), 0}, {bytes(), kPage}};
    errno = 0;
    const auto result = preadv(direct_, leading, 2, 0);
    if (GetParam()) {
        EXPECT_EQ(static_cast<ssize_t>(kPage), result);
    } else {
        ExpectError(result, EINVAL);
    }
    iovec trailing[] = {{bytes(), kPage}, {reinterpret_cast<void*>(1), 0}};
    EXPECT_EQ(static_cast<ssize_t>(kPage), preadv(direct_, trailing, 2, 0));
}
TEST_P(DirectRead, FaultAfterFirstVectorKeepsBackendCompletionContract) {
    void* inaccessible = mmap(nullptr, kPage, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    iovec vectors[] = {{bytes(), kPage}, {inaccessible, kPage}};
    errno = 0;
    const auto result = readv(direct_, vectors, 2);
    if (GetParam()) {
        EXPECT_EQ(static_cast<ssize_t>(kPage), result);
        EXPECT_EQ(static_cast<off_t>(kPage), lseek(direct_, 0, SEEK_CUR));
    } else {
        ExpectError(result, EFAULT);
    }
    EXPECT_EQ(0, munmap(inaccessible, kPage));
}
TEST_P(DirectRead, ShortEofKeepsBackendDestinationPageAdmission) {
    ASSERT_EQ(0, ftruncate(buffered_, 37));
    ASSERT_EQ(0, fsync(buffered_));
    void* inaccessible = mmap(nullptr, kPage, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    iovec vectors[] = {{bytes(), 512}, {inaccessible, kPage - 512}};
    errno = 0;
    const auto result = readv(direct_, vectors, 2);
    if (GetParam()) {
        // Legacy FAT DIO retains its completed sector, then clips report to EOF.
        EXPECT_EQ(37, result);
        EXPECT_EQ(37, lseek(direct_, 0, SEEK_CUR));
    } else {
        // iomap admits the mapped transfer before clipping the completion to
        // i_size; an inaccessible later destination remains an operation error.
        ExpectError(result, EFAULT);
    }
    EXPECT_EQ(0, munmap(inaccessible, kPage));
    if (!GetParam()) {
        EXPECT_EQ(37, pread(direct_, bytes(), kPage, 0));
        std::vector<unsigned char> expected(kPage, 0);
        memset(expected.data(), 'r', 37);
        EXPECT_EQ(0, memcmp(bytes(), expected.data(), expected.size()));
    }
}
TEST_P(DirectRead, FlagUpdateUsesSameReadAdmission) {
    ASSERT_EQ(0, fcntl(buffered_, F_SETFL, fcntl(buffered_, F_GETFL) | O_DIRECT));
    errno = 0;
    EXPECT_EQ(-1, pread(buffered_, bytes() + 1, kPage, 0));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(0, fcntl(buffered_, F_SETFL, fcntl(buffered_, F_GETFL) & ~O_DIRECT));
    EXPECT_EQ(static_cast<ssize_t>(kPage), pread(buffered_, bytes() + 1, kPage, 0));
}
TEST_P(DirectRead, ConcurrentFlagUpdatesKeepEachReadDispatchConsistent) {
    const int original = fcntl(buffered_, F_GETFL);
    ASSERT_GE(original, 0);
    std::atomic<bool> stop{false};
    std::atomic<int> update_error{0};
    std::thread updater([&] {
        while (!stop.load(std::memory_order_acquire)) {
            if (fcntl(buffered_, F_SETFL, original | O_DIRECT) != 0
                || fcntl(buffered_, F_SETFL, original & ~O_DIRECT) != 0) {
                update_error.store(errno, std::memory_order_release);
                break;
            }
        }
    });
    // Either flag state is valid, but no state permits EIO or an OFD update
    // for this positional request. An unaligned destination distinguishes the
    // legitimate direct-admission rejection from the buffered success.
    for (int attempt = 0; attempt < 200; ++attempt) {
        errno = 0;
        const ssize_t count = pread(buffered_, bytes() + 1, kPage, 0);
        const int error = errno;
        if (count == static_cast<ssize_t>(kPage)) {
            std::vector<unsigned char> expected(kPage, 'r');
            EXPECT_EQ(0, memcmp(bytes() + 1, expected.data(), expected.size()));
        } else {
            EXPECT_EQ(-1, count);
            EXPECT_EQ(EINVAL, error);
        }
    }
    stop.store(true, std::memory_order_release);
    updater.join();
    EXPECT_EQ(0, update_error.load(std::memory_order_acquire));
    EXPECT_EQ(static_cast<off_t>(kSize), lseek(buffered_, 0, SEEK_CUR));
    EXPECT_EQ(0, fcntl(buffered_, F_SETFL, original));
}
INSTANTIATE_TEST_SUITE_P(Storage, DirectRead, testing::Values(false, true),
                        [](const testing::TestParamInfo<bool>& info) { return info.param ? "Fat" : "Ext4"; });

class Ext4DirectRead : public testing::Test {
 protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(disk_.SetUp("ext4_inode_identity.img", "/tmp", "direct_sparse_read"));
        ASSERT_NO_FATAL_FAILURE(disk_.Mount());
        path_ = disk_.mount_point() + "/sparse";
        writer_ = open(path_.c_str(), O_CREAT | O_RDWR, 0600);
        ASSERT_GE(writer_, 0);
        ASSERT_EQ(0, ftruncate(writer_, 4 * kPage));
        direct_ = open(path_.c_str(), O_RDONLY | O_DIRECT);
        ASSERT_GE(direct_, 0);
        ASSERT_EQ(0, posix_memalign(&output_, kPage, 4 * kPage));
        memset(output_, 'x', 4 * kPage);
    }
    void TearDown() override {
        if (direct_ >= 0) close(direct_);
        if (writer_ >= 0) close(writer_);
        free(output_);
    }
    unsigned char* bytes() { return static_cast<unsigned char*>(output_); }
    dunitest::LoopExt4 disk_;
    std::string path_;
    int writer_ = -1, direct_ = -1;
    void* output_ = nullptr;
};
TEST_F(Ext4DirectRead, HoleAllowsUnalignedDestinationLengthAndOffset) {
    ASSERT_EQ(0, fsync(writer_));
    ASSERT_EQ(37, pread(direct_, bytes() + 1, 37, 1));
    for (size_t i = 1; i < 38; ++i) EXPECT_EQ(0, bytes()[i]);
    EXPECT_EQ('x', bytes()[0]);
    EXPECT_EQ('x', bytes()[38]);
}
TEST_F(Ext4DirectRead, UnwrittenExtentUsesZeroReadSemantics) {
    ASSERT_EQ(0, fallocate(writer_, 0, 0, 4 * kPage)) << strerror(errno);
    ASSERT_EQ(0, fsync(writer_));
    ASSERT_EQ(37, pread(direct_, bytes() + 1, 37, 1));
    for (size_t i = 1; i < 38; ++i) EXPECT_EQ(0, bytes()[i]);
}
TEST_F(Ext4DirectRead, HoleBeforeBadMappedRangeDoesNotCommitPosition) {
    std::vector<unsigned char> data(kPage, 'm');
    ASSERT_EQ(static_cast<ssize_t>(kPage), pwrite(writer_, data.data(), data.size(), kPage));
    ASSERT_EQ(0, fsync(writer_));
    iovec vectors[] = {{bytes(), kPage}, {bytes() + kPage + 1, kPage}};
    errno = 0;
    EXPECT_EQ(-1, readv(direct_, vectors, 2));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    // iomap may zero the leading hole before discovering the mapped error.
    std::vector<unsigned char> zero(kPage, 0), untouched(kPage + 1, 'x');
    EXPECT_EQ(0, memcmp(bytes(), zero.data(), zero.size()));
    EXPECT_EQ(0, memcmp(bytes() + kPage, untouched.data(), untouched.size()));
    // Zeroing stops immediately after the first positive segment: its empty
    // successor remains in the iterator inspected by mapped admission.
    iovec boundary_empty[] = {{bytes(), kPage}, {reinterpret_cast<void*>(1), 0}, {bytes() + kPage, kPage}};
    errno = 0;
    EXPECT_EQ(-1, readv(direct_, boundary_empty, 3));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    // An empty segment inside the hole is traversed while zeroing continues,
    // so it no longer participates when the mapped suffix is reached.
    iovec interior_empty[] = {{bytes(), kPage / 2}, {reinterpret_cast<void*>(1), 0},
                             {bytes() + kPage / 2, kPage + kPage / 2}};
    EXPECT_EQ(2 * static_cast<ssize_t>(kPage), readv(direct_, interior_empty, 3));
    EXPECT_EQ(2 * static_cast<off_t>(kPage), lseek(direct_, 0, SEEK_CUR));
}
TEST_F(Ext4DirectRead, FaultBeyondScratchWindowRemainsAnOperationError) {
    constexpr size_t window = 65536;
    std::vector<unsigned char> data(2 * window, 'm');
    ASSERT_EQ(static_cast<ssize_t>(data.size()), pwrite(writer_, data.data(), data.size(), 0));
    ASSERT_EQ(0, fsync(writer_));
    auto* target = static_cast<unsigned char*>(mmap(nullptr, 2 * window, PROT_READ | PROT_WRITE,
                                                   MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, target);
    ASSERT_EQ(0, mprotect(target + window, window, PROT_NONE));
    errno = 0;
    EXPECT_EQ(-1, read(direct_, target, 2 * window));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    EXPECT_EQ(0, munmap(target, 2 * window));
}

TEST_F(Ext4DirectRead, ShortEofHoleAndUnwrittenStillTransferTheirIomapRange) {
    void* inaccessible = mmap(nullptr, kPage, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    for (bool unwritten : {false, true}) {
        ASSERT_EQ(0, ftruncate(writer_, 0));
        ASSERT_EQ(0, ftruncate(writer_, 37));
        if (unwritten) {
            ASSERT_EQ(0, fallocate(writer_, FALLOC_FL_KEEP_SIZE, 0, 2 * kPage));
        }
        ASSERT_EQ(0, fsync(writer_));
        ASSERT_EQ(0, lseek(direct_, 0, SEEK_SET));
        iovec tail_in_block[] = {{bytes(), 512}, {inaccessible, kPage - 512}};
        errno = 0;
        EXPECT_EQ(-1, readv(direct_, tail_in_block, 2)) << unwritten;
        EXPECT_EQ(EFAULT, errno) << unwritten;
        EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
        iovec tail_after_block[] = {{bytes(), kPage}, {inaccessible, kPage}};
        errno = 0;
        EXPECT_EQ(-1, readv(direct_, tail_after_block, 2)) << unwritten;
        EXPECT_EQ(EFAULT, errno) << unwritten;
        EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    }
    EXPECT_EQ(0, munmap(inaccessible, kPage));
    // A scratch-window boundary does not terminate a hole actor at EOF.
    ASSERT_EQ(0, ftruncate(writer_, 0));
    ASSERT_EQ(0, ftruncate(writer_, 37));
    ASSERT_EQ(0, fsync(writer_));
    constexpr size_t window = 65536;
    auto* large = static_cast<unsigned char*>(mmap(nullptr, window + kPage, PROT_READ | PROT_WRITE,
                                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, large);
    ASSERT_EQ(0, mprotect(large + window, kPage, PROT_NONE));
    ASSERT_EQ(0, lseek(direct_, 0, SEEK_SET));
    errno = 0;
    EXPECT_EQ(-1, read(direct_, large, window + kPage));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    ASSERT_EQ(0, mprotect(large + window, kPage, PROT_READ | PROT_WRITE));
    memset(large, 'x', window + kPage);
    EXPECT_EQ(37, pread(direct_, large, window + kPage, 0));
    std::vector<unsigned char> expected(window + kPage, 0);
    EXPECT_EQ(0, memcmp(large, expected.data(), expected.size()));
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    EXPECT_EQ(0, munmap(large, window + kPage));
}

TEST_F(Ext4DirectRead, DirectCompletionUpdatesAtimeExceptZeroAndNoatime) {
    std::vector<unsigned char> data(kPage, 'm');
    ASSERT_EQ(static_cast<ssize_t>(data.size()), pwrite(writer_, data.data(), data.size(), 0));
    ASSERT_EQ(0, fsync(writer_));
    timespec old[] = {{10, 0}, {10, 0}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, path_.c_str(), old, 0));
    EXPECT_EQ(0, pread(direct_, bytes() + 1, 0, 1));
    struct stat observed {};
    ASSERT_EQ(0, fstat(writer_, &observed));
    EXPECT_EQ(10, observed.st_atim.tv_sec);
    errno = 0;
    EXPECT_EQ(-1, pread(direct_, bytes() + 1, kPage, 0));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(0, fstat(writer_, &observed));
    EXPECT_GT(observed.st_atim.tv_sec, 10);
    ASSERT_EQ(0, utimensat(AT_FDCWD, path_.c_str(), old, 0));
    EXPECT_EQ(0, pread(direct_, bytes(), kPage, 4 * kPage));
    ASSERT_EQ(0, fstat(writer_, &observed));
    EXPECT_GT(observed.st_atim.tv_sec, 10);
    ASSERT_EQ(0, utimensat(AT_FDCWD, path_.c_str(), old, 0));
    int noatime = open(path_.c_str(), O_RDONLY | O_DIRECT | O_NOATIME);
    ASSERT_GE(noatime, 0);
    errno = 0;
    EXPECT_EQ(-1, pread(noatime, bytes() + 1, kPage, 0));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(0, fstat(writer_, &observed));
    EXPECT_EQ(10, observed.st_atim.tv_sec);
    EXPECT_EQ(0, close(noatime));
}
TEST(DirectReadReentry, SameInodeColdMappingCanFaultOutsideSourceGuards) {
    auto disk = std::make_unique<dunitest::LoopExt4>();
    ASSERT_NO_FATAL_FAILURE(disk->SetUp("ext4_inode_identity.img", "/tmp", "direct_read_reentry"));
    ASSERT_NO_FATAL_FAILURE(disk->Mount());
    const auto path = disk->mount_point() + "/file";
    int writer = open(path.c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(writer, 0);
    std::vector<unsigned char> data(2 * kPage, 'c');
    ASSERT_EQ(static_cast<ssize_t>(data.size()), write(writer, data.data(), data.size()));
    ASSERT_EQ(0, fsync(writer));
    ASSERT_EQ(0, close(writer));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(10);
        int source = open(path.c_str(), O_RDONLY | O_DIRECT);
        int mapped = open(path.c_str(), O_RDWR);
        if (source < 0 || mapped < 0) _exit(10);
        for (int flags : {MAP_PRIVATE, MAP_SHARED}) {
            void* target = mmap(nullptr, kPage, PROT_READ | PROT_WRITE, flags, mapped, 0);
            if (target == MAP_FAILED) _exit(11);
            // A fresh mapping has no destination PTE; protected copying must
            // be free to reenter this inode's cache/fault path.
            if (pread(source, target, kPage, kPage) != static_cast<ssize_t>(kPage)) _exit(12);
            if (memcmp(target, data.data(), kPage) != 0) _exit(13);
            if (munmap(target, kPage) != 0) _exit(14);
        }
        _exit(0);
    }
    for (int attempt = 0; attempt < 400; ++attempt) {
        int status = 0;
        pid_t result = waitpid(child, &status, WNOHANG);
        if (result == child) {
            ASSERT_TRUE(WIFEXITED(status));
            ASSERT_EQ(0, WEXITSTATUS(status));
            return;
        }
        ASSERT_EQ(0, result);
        usleep(25000);
    }
    kill(child, SIGKILL);
    int status = 0;
    (void)waitpid(child, &status, WNOHANG);
    (void)disk.release();
    FAIL() << "direct read destination fault blocked on source inode resources";
}

TEST(DirectReadBlock, CapacityClipsIteratorBeforeSectorAlignment) {
    dunitest::LoopExt4 disk;
    ASSERT_NO_FATAL_FAILURE(disk.SetUp("ext4_inode_identity.img", "/tmp", "direct_read_block"));
    int fd = open(disk.loop_path().c_str(), O_RDONLY | O_DIRECT);
    ASSERT_GE(fd, 0) << strerror(errno);
    struct stat fixture {};
    ASSERT_EQ(0, stat(dunitest::FixturePath().c_str(), &fixture));
    const auto capacity = fixture.st_size;
    ASSERT_GE(capacity, kPage);
    void* output = nullptr;
    ASSERT_EQ(0, posix_memalign(&output, kPage, 2 * kPage));
    auto* bytes = static_cast<unsigned char*>(output);
    errno = 0;
    EXPECT_EQ(-1, pread(fd, bytes + 1, kPage, 0));
    EXPECT_EQ(EINVAL, errno);
    iovec vectors[] = {{bytes, kPage}, {bytes + kPage + 1, kPage}};
    EXPECT_EQ(static_cast<ssize_t>(kPage), preadv(fd, vectors, 2, capacity - kPage));
    EXPECT_EQ(0, pread(fd, bytes + 1, kPage - 1, capacity));
    EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
    iovec empty_before[] = {{reinterpret_cast<void*>(1), 0}, {bytes, kPage}};
    errno = 0;
    EXPECT_EQ(-1, preadv(fd, empty_before, 2, 0));
    EXPECT_EQ(EINVAL, errno);
    iovec empty_after[] = {{bytes, kPage}, {reinterpret_cast<void*>(1), 0}};
    EXPECT_EQ(static_cast<ssize_t>(kPage), preadv(fd, empty_after, 2, 0));
    void* inaccessible = mmap(nullptr, kPage, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, inaccessible);
    iovec fault[] = {{bytes, kPage}, {inaccessible, kPage}};
    errno = 0;
    EXPECT_EQ(static_cast<ssize_t>(kPage), readv(fd, fault, 2));
    EXPECT_EQ(static_cast<off_t>(kPage), lseek(fd, 0, SEEK_CUR));
    EXPECT_EQ(0, munmap(inaccessible, kPage));
    free(output);
    EXPECT_EQ(0, close(fd));
}

TEST(DirectReadMemory, TmpfsIgnoresDirectReadAlignment) {
    char mountpoint[] = "/tmp/direct-read-memory-XXXXXX";
    ASSERT_NE(nullptr, mkdtemp(mountpoint));
    ASSERT_EQ(0, mount("none", mountpoint, "tmpfs", 0, nullptr));
    const auto path = std::string(mountpoint) + "/file";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_DIRECT, 0600);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(8, pwrite(fd, "abcdefgh", 8, 0));
    char output[16] = {};
    EXPECT_EQ(3, pread(fd, output + 1, 3, 1));
    EXPECT_EQ(0, memcmp(output + 1, "bcd", 3));
    iovec vectors[] = {{output + 1, 1}, {output + 5, 2}};
    EXPECT_EQ(3, preadv(fd, vectors, 2, 1));
    EXPECT_EQ('b', output[1]);
    EXPECT_EQ(0, memcmp(output + 5, "cd", 2));
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, unlink(path.c_str()));
    EXPECT_EQ(0, umount(mountpoint));
    EXPECT_EQ(0, rmdir(mountpoint));
}

class OverlayDirectReadMemory : public testing::Test {
 protected:
    void SetUp() override {
        char path[] = "/tmp/overlay-direct-read-memory-XXXXXX";
        ASSERT_NE(nullptr, mkdtemp(path));
        root_ = path;
        ASSERT_EQ(0, mount("none", root_.c_str(), "tmpfs", 0, nullptr));
        mounted_ = true;
        lower_ = root_ + "/lower";
        merged_ = root_ + "/merged";
        const auto upper = root_ + "/upper", work = root_ + "/work";
        for (const std::string* directory : std::initializer_list<const std::string*>{&lower_, &upper, &work, &merged_}) {
            ASSERT_EQ(0, mkdir(directory->c_str(), 0755));
        }
        int creator = open((lower_ + "/file").c_str(), O_CREAT | O_RDWR, 0600);
        ASSERT_GE(creator, 0);
        ASSERT_EQ(8, write(creator, "abcdefgh", 8));
        ASSERT_EQ(0, close(creator));
        std::string options = "lowerdir=" + lower_ + ",upperdir=" + upper + ",workdir=" + work;
        ASSERT_EQ(0, mount("overlay", merged_.c_str(), "overlay", 0, options.c_str())) << strerror(errno);
        overlaid_ = true;
        fd_ = open((merged_ + "/file").c_str(), O_RDONLY | O_DIRECT);
        ASSERT_GE(fd_, 0) << strerror(errno);
    }
    void TearDown() override {
        if (fd_ >= 0) EXPECT_EQ(0, close(fd_));
        if (overlaid_) EXPECT_EQ(0, umount(merged_.c_str()));
        if (mounted_) EXPECT_EQ(0, umount(root_.c_str()));
        if (!root_.empty()) EXPECT_EQ(0, rmdir(root_.c_str()));
    }
    std::string root_, lower_, merged_;
    bool mounted_ = false, overlaid_ = false;
    int fd_ = -1;
};
TEST_F(OverlayDirectReadMemory, BufferedBackingFallbackKeepsTheOriginalDestination) {
    char output[16] = {};
    EXPECT_EQ(3, pread(fd_, output + 1, 3, 1));
    EXPECT_EQ(0, memcmp(output + 1, "bcd", 3));
    EXPECT_EQ(0, lseek(fd_, 0, SEEK_CUR));
    iovec vectors[] = {{output + 1, 1}, {output + 5, 2}};
    EXPECT_EQ(3, readv(fd_, vectors, 2));
    EXPECT_EQ('a', output[1]);
    EXPECT_EQ(0, memcmp(output + 5, "bc", 2));
    EXPECT_EQ(3, lseek(fd_, 0, SEEK_CUR));
}

TEST_F(Ext4DirectRead, OverlayPreservesOriginalDestinationIterator) {
    std::string lower = disk_.mount_point() + "/lower";
    std::string upper = disk_.mount_point() + "/upper";
    std::string work = disk_.mount_point() + "/work";
    std::string merged = disk_.mount_point() + "/merged";
    for (const auto* path : {&lower, &upper, &work, &merged}) ASSERT_EQ(0, mkdir(path->c_str(), 0755));
    int creator = open((lower + "/file").c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(creator, 0);
    std::vector<unsigned char> data(2 * kPage, 'v');
    ASSERT_EQ(static_cast<ssize_t>(data.size()), write(creator, data.data(), data.size()));
    ASSERT_EQ(0, fsync(creator));
    ASSERT_EQ(0, close(creator));
    std::string options = "lowerdir=" + lower + ",upperdir=" + upper + ",workdir=" + work;
    ASSERT_EQ(0, mount("overlay", merged.c_str(), "overlay", 0, options.c_str())) << strerror(errno);
    int fd = open((merged + "/file").c_str(), O_RDONLY | O_DIRECT);
    EXPECT_GE(fd, 0) << strerror(errno);
    if (fd >= 0) {
        errno = 0;
        EXPECT_EQ(-1, pread(fd, bytes() + 1, kPage, 0));
        EXPECT_EQ(EINVAL, errno);
        iovec bad[] = {{bytes(), kPage}, {bytes() + kPage + 1, kPage}};
        errno = 0;
        EXPECT_EQ(-1, readv(fd, bad, 2));
        EXPECT_EQ(EINVAL, errno);
        EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
        EXPECT_EQ(static_cast<ssize_t>(kPage), pread(fd, bytes(), kPage, 0));
        EXPECT_EQ('v', bytes()[0]);
        close(fd);
    }
    EXPECT_EQ(0, umount(merged.c_str()));
}
}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
