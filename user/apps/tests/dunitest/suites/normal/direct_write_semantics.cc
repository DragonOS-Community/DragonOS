#include "loop_ext4_test_support.h"

#include <stdlib.h>
#include <signal.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <sys/uio.h>
#include <vector>
#include <atomic>
#include <thread>

namespace {
class DirectMemoryFs : public testing::Test {
 protected:
    void Mount(const char* type) {
        char path[] = "/tmp/direct-memory-XXXXXX";
        ASSERT_NE(nullptr, mkdtemp(path));
        root_ = path;
        ASSERT_EQ(0, mount("none", root_.c_str(), type, 0, nullptr)) << strerror(errno);
        mounted_ = true;
    }
    void TearDown() override {
        if (!overlay_.empty()) { EXPECT_EQ(0, umount(overlay_.c_str())); }
        if (mounted_) { EXPECT_EQ(0, umount(root_.c_str())) << strerror(errno); }
        if (!root_.empty()) { EXPECT_EQ(0, rmdir(root_.c_str())); }
    }
    std::string root_, overlay_;
    bool mounted_ = false;
};
TEST_F(DirectMemoryFs, TmpfsAcceptsUnalignedDirectWrite) {
    ASSERT_NO_FATAL_FAILURE(Mount("tmpfs"));
    const auto path = root_ + "/file";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_DIRECT, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    char data[] = "xpayload";
    EXPECT_EQ(7, pwrite(fd, data + 1, 7, 1)) << strerror(errno);
    // tmpfs advertises O_DIRECT but intentionally uses its memory-page path.
    EXPECT_EQ(0, fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) & ~O_DIRECT));
    char actual[8] = {};
    EXPECT_EQ(8, pread(fd, actual, sizeof(actual), 0));
    EXPECT_EQ(0, actual[0]);
    EXPECT_EQ(0, memcmp(data + 1, actual + 1, 7));
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, unlink(path.c_str()));
}
TEST_F(DirectMemoryFs, RamfsRejectsDirectAtOpen) {
    ASSERT_NO_FATAL_FAILURE(Mount("ramfs"));
    const auto path = root_ + "/file";
    errno = 0;
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_DIRECT, 0600);
    const int error = errno;
    if (fd >= 0) close(fd);
    EXPECT_EQ(-1, fd);
    EXPECT_EQ(EINVAL, error);
    EXPECT_EQ(0, unlink(path.c_str()));
}
TEST_F(DirectMemoryFs, RamfsRejectsDirectFlagUpdate) {
    ASSERT_NO_FATAL_FAILURE(Mount("ramfs"));
    const auto path = root_ + "/file";
    int fd = open(path.c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(fd, 0);
    const int flags = fcntl(fd, F_GETFL);
    errno = 0;
    EXPECT_EQ(-1, fcntl(fd, F_SETFL, flags | O_DIRECT));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(flags, fcntl(fd, F_GETFL));
    EXPECT_EQ(0, close(fd));
    EXPECT_EQ(0, unlink(path.c_str()));
}

class DirectExt4 : public testing::Test {
 protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(disk_.SetUp("ext4_inode_identity.img", "/tmp", "direct_write"));
        ASSERT_NO_FATAL_FAILURE(disk_.Mount());
        path_ = disk_.mount_point() + "/file";
        buffered_ = open(path_.c_str(), O_CREAT | O_RDWR, 0600);
        ASSERT_GE(buffered_, 0) << strerror(errno);
        std::vector<unsigned char> original(3 * 65536, 'o');
        ASSERT_EQ(static_cast<ssize_t>(original.size()),
                  pwrite(buffered_, original.data(), original.size(), 0));
        // Keep buffered dirty pages present when opening the direct writer.
        direct_ = open(path_.c_str(), O_RDWR | O_DIRECT);
        ASSERT_GE(direct_, 0) << strerror(errno);
        ASSERT_EQ(0, posix_memalign(&aligned_, 4096, 3 * 65536));
        memset(aligned_, 'n', 3 * 65536);
    }
    void TearDown() override {
        if (direct_ >= 0) close(direct_);
        if (buffered_ >= 0) close(buffered_);
        free(aligned_);
    }
    void CheckNoMutation() {
        unsigned char actual[32] = {};
        ASSERT_EQ(32, pread(buffered_, actual, sizeof(actual), 0));
        for (unsigned char byte : actual) EXPECT_EQ('o', byte);
        EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    }
    dunitest::LoopExt4 disk_;
    std::string path_;
    int buffered_ = -1, direct_ = -1;
    void* aligned_ = nullptr;
};
TEST_F(DirectExt4, RejectsMisalignedUserAddress) {
    errno = 0;
    EXPECT_EQ(-1, write(direct_, static_cast<unsigned char*>(aligned_) + 1, 4096));
    EXPECT_EQ(EINVAL, errno);
    CheckNoMutation();
}
TEST_F(DirectExt4, RejectsMisalignedLengthAndOffset) {
    errno = 0;
    EXPECT_EQ(-1, pwrite(direct_, aligned_, 4095, 0));
    EXPECT_EQ(EINVAL, errno);
    errno = 0;
    EXPECT_EQ(-1, pwrite(direct_, aligned_, 4096, 1));
    EXPECT_EQ(EINVAL, errno);
    CheckNoMutation();
}
TEST_F(DirectExt4, ChecksEachVectorSegmentBeforeCopy) {
    // Aggregate size/address are aligned; the interior iovec boundary is not.
    iovec vectors[] = {{aligned_, 1}, {static_cast<unsigned char*>(aligned_) + 1, 4095}};
    errno = 0;
    EXPECT_EQ(-1, writev(direct_, vectors, 2));
    EXPECT_EQ(EINVAL, errno);
    CheckNoMutation();
}
TEST_F(DirectExt4, FlagUpdateUsesSameWriteAdmission) {
    ASSERT_EQ(0, fcntl(buffered_, F_SETFL, fcntl(buffered_, F_GETFL) | O_DIRECT));
    errno = 0;
    EXPECT_EQ(-1, pwrite(buffered_, static_cast<unsigned char*>(aligned_) + 1, 4096, 0));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(0, fcntl(buffered_, F_SETFL, fcntl(buffered_, F_GETFL) & ~O_DIRECT));
    CheckNoMutation();
}
TEST_F(DirectExt4, AlignedWriteKeepsBufferedAndSharedMappingCoherent) {
    constexpr size_t size = 2 * 65536;
    auto* mapping = static_cast<unsigned char*>(mmap(nullptr, size, PROT_READ,
                                                   MAP_SHARED, buffered_, 0));
    ASSERT_NE(MAP_FAILED, mapping) << strerror(errno);
    EXPECT_EQ('o', mapping[0]);
    EXPECT_EQ('o', mapping[size - 1]);
    EXPECT_EQ(static_cast<ssize_t>(size), pwrite(direct_, aligned_, size, 0)) << strerror(errno);
    std::vector<unsigned char> actual(size);
    EXPECT_EQ(static_cast<ssize_t>(size), pread(buffered_, actual.data(), actual.size(), 0));
    EXPECT_EQ(0, memcmp(aligned_, actual.data(), size));
    EXPECT_EQ('n', mapping[0]);
    EXPECT_EQ('n', mapping[size - 1]);
    EXPECT_EQ(0, munmap(mapping, size));
    // A later dirty flush must not resurrect bytes from the old cache image.
    EXPECT_EQ(0, fsync(buffered_));
    EXPECT_EQ(static_cast<ssize_t>(size), pread(direct_, aligned_, size, 0));
    for (size_t i = 0; i < size; ++i) ASSERT_EQ('n', static_cast<unsigned char*>(aligned_)[i]) << i;
}
TEST_F(DirectExt4, ZeroLengthDoesNotRequireAlignment) {
    EXPECT_EQ(0, pwrite(direct_, static_cast<unsigned char*>(aligned_) + 1, 0, 1));
    CheckNoMutation();
}
TEST_F(DirectExt4, AlignedVectorsAndPwritev2KeepPosition) {
    iovec vectors[] = {{aligned_, 4096}, {static_cast<unsigned char*>(aligned_) + 65536, 4096}};
    ASSERT_EQ(8192, pwritev(direct_, vectors, 2, 0)) << strerror(errno);
    ASSERT_EQ(8192, syscall(SYS_pwritev2, direct_, vectors, 2, 8192UL, 0UL, 0U)) << strerror(errno);
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    unsigned char output[16384];
    ASSERT_EQ(static_cast<ssize_t>(sizeof(output)), pread(buffered_, output, sizeof(output), 0));
    for (unsigned char byte : output) ASSERT_EQ('n', byte);
}
TEST_F(DirectExt4, FaultReturnsOnlyCompleteSectors) {
    const size_t page = sysconf(_SC_PAGESIZE);
    auto* input = static_cast<unsigned char*>(mmap(nullptr, page * 2, PROT_READ | PROT_WRITE,
                                                  MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
    ASSERT_NE(MAP_FAILED, input);
    memset(input, 'f', page);
    ASSERT_EQ(0, mprotect(input + page, page, PROT_NONE));
    errno = 0;
    EXPECT_EQ(-1, pwrite(direct_, input + page, page, 0));
    EXPECT_EQ(EFAULT, errno);
    EXPECT_EQ(static_cast<ssize_t>(page), pwrite(direct_, input, page * 2, 0));
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
    std::vector<unsigned char> actual(page * 2);
    ASSERT_EQ(static_cast<ssize_t>(actual.size()), pread(buffered_, actual.data(), actual.size(), 0));
    for (size_t i = 0; i < actual.size(); ++i) ASSERT_EQ(i < page ? 'f' : 'o', actual[i]) << i;
    EXPECT_EQ(0, munmap(input, page * 2));
}
TEST_F(DirectExt4, FileLimitUsesEffectiveIteratorAlignment) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        rlimit limit{4096, 4096};
        if (setrlimit(RLIMIT_FSIZE, &limit) != 0) _exit(10);
        iovec vectors[] = {{aligned_, 4096}, {static_cast<unsigned char*>(aligned_) + 1, 4096}};
        // The invalid tail is outside generic_write_checks' effective prefix.
        _exit(pwritev(direct_, vectors, 2, 0) == 4096 ? 0 : 11);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}
TEST_F(DirectExt4, AppendUsesActualAlignedEof) {
    ASSERT_EQ(0, fcntl(direct_, F_SETFL, fcntl(direct_, F_GETFL) | O_APPEND));
    ASSERT_EQ(4096, pwrite(direct_, aligned_, 4096, 0)) << strerror(errno);
    struct stat statbuf{};
    ASSERT_EQ(0, fstat(direct_, &statbuf));
    EXPECT_EQ(3 * 65536 + 4096, statbuf.st_size);
    EXPECT_EQ(0, lseek(direct_, 0, SEEK_CUR));
}
TEST_F(DirectExt4, PrimaryTimesIncludeAlignmentFailure) {
    timespec times[] = {{10, 0}, {10, 0}};
    ASSERT_EQ(0, utimensat(AT_FDCWD, path_.c_str(), times, 0));
    EXPECT_EQ(-1, pwrite(direct_, static_cast<unsigned char*>(aligned_) + 1, 4096, 0));
    EXPECT_EQ(EINVAL, errno);
    struct stat statbuf{};
    ASSERT_EQ(0, fstat(buffered_, &statbuf));
    EXPECT_GT(statbuf.st_mtim.tv_sec, 10);
    ASSERT_EQ(4096, pwrite(direct_, aligned_, 4096, 0));
    ASSERT_EQ(0, fsync(direct_));
    const auto observed = statbuf.st_mtim.tv_sec;
    EXPECT_EQ(0, close(direct_)); direct_ = -1;
    EXPECT_EQ(0, close(buffered_)); buffered_ = -1;
    ASSERT_NO_FATAL_FAILURE(disk_.Unmount());
    ASSERT_NO_FATAL_FAILURE(disk_.Mount());
    ASSERT_EQ(0, stat(path_.c_str(), &statbuf));
    EXPECT_GE(statbuf.st_mtim.tv_sec, observed);
}
TEST_F(DirectExt4, SameInodeMappingSourceFaultsOutsideInvalidation) {
    for (int flags : {MAP_PRIVATE, MAP_SHARED}) {
        SCOPED_TRACE(flags);
        auto* input = static_cast<unsigned char*>(mmap(nullptr, 2 * 65536, PROT_READ,
                                                       flags, buffered_, 0));
        ASSERT_NE(MAP_FAILED, input);
        ASSERT_EQ(0, madvise(input, 2 * 65536, MADV_DONTNEED));
        pid_t child = fork();
        ASSERT_GE(child, 0);
        if (child == 0) {
            alarm(10);
            _exit(pwrite(direct_, input, 2 * 65536, 0) == 2 * 65536 ? 0 : 12);
        }
        int status = 0;
        ASSERT_EQ(child, waitpid(child, &status, 0));
        ASSERT_TRUE(WIFEXITED(status)) << status;
        EXPECT_EQ(0, WEXITSTATUS(status));
        ASSERT_EQ(0, munmap(input, 2 * 65536));
    }
}
TEST_F(DirectExt4, DirectDirectoryOpenIsRejected) {
    errno = 0;
    int fd = open(disk_.mount_point().c_str(), O_RDONLY | O_DIRECTORY | O_DIRECT);
    const int error = errno;
    if (fd >= 0) close(fd);
    EXPECT_EQ(-1, fd);
    EXPECT_EQ(EINVAL, error);
}
TEST_F(DirectMemoryFs, TmpfsDirectReadUsesMemoryOwner) {
    ASSERT_NO_FATAL_FAILURE(Mount("tmpfs"));
    const auto path = root_ + "/file";
    int fd = open(path.c_str(), O_CREAT | O_RDWR | O_DIRECT, 0600);
    ASSERT_GE(fd, 0);
    const char bytes[] = "owner";
    ASSERT_EQ(5, write(fd, bytes, 5));
    char actual[5] = {};
    EXPECT_EQ(5, pread(fd, actual, 5, 0));
    EXPECT_EQ(0, memcmp(actual, bytes, 5));
    close(fd);
    EXPECT_EQ(0, unlink(path.c_str()));
}

class DirectFat : public testing::Test {
 protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(disk_.SetUp("fat_stat_blocks.img", "/tmp", "direct_fat"));
        ASSERT_NO_FATAL_FAILURE(disk_.Mount(0, "vfat"));
        path_ = disk_.mount_point() + "/file";
        fd_ = open(path_.c_str(), O_CREAT | O_RDWR | O_DIRECT, 0600);
        ASSERT_GE(fd_, 0);
        ASSERT_EQ(0, posix_memalign(&aligned_, 4096, 8192));
        memset(aligned_, 'd', 8192);
    }
    void TearDown() override {
        if (fd_ >= 0) close(fd_);
        free(aligned_);
    }
    dunitest::LoopExt4 disk_;
    std::string path_;
    int fd_ = -1;
    void* aligned_ = nullptr;
};
TEST_F(DirectFat, ExpansionFallsBackButExistingRangeRequiresAlignment) {
    const char value[] = "abc";
    // FAT cannot directly map this extending write; ordinary buffered policy applies.
    ASSERT_EQ(3, pwrite(fd_, value, 3, 0)) << strerror(errno);
    ASSERT_EQ(8192, pwrite(fd_, aligned_, 8192, 3));
    ASSERT_EQ(0, fsync(fd_));
    errno = 0;
    EXPECT_EQ(-1, pwrite(fd_, static_cast<unsigned char*>(aligned_) + 1, 4096, 0));
    EXPECT_EQ(EINVAL, errno);
    ASSERT_EQ(4096, pwrite(fd_, aligned_, 4096, 0));
    int buffered = open(path_.c_str(), O_RDONLY);
    ASSERT_GE(buffered, 0);
    unsigned char actual[4096];
    ASSERT_EQ(4096, pread(buffered, actual, sizeof(actual), 0));
    EXPECT_EQ(0, memcmp(aligned_, actual, sizeof(actual)));
    close(buffered);
}
TEST_F(DirectFat, DirectReadImmediatelySeesBufferedExpansionFallback) {
    ASSERT_EQ(8192, pwrite(fd_, aligned_, 8192, 0)) << strerror(errno);
    memset(aligned_, 0, 8192);
    EXPECT_EQ(8192, pread(fd_, aligned_, 8192, 0)) << strerror(errno);
    for (size_t i = 0; i < 8192; ++i) ASSERT_EQ('d', static_cast<unsigned char*>(aligned_)[i]) << i;
}
TEST(DirectBlock, LoopRawIoPreservesSectorAdmissionAndTypedReads) {
    dunitest::LoopExt4 disk;
    ASSERT_NO_FATAL_FAILURE(disk.SetUp("ext4_inode_identity.img", "/tmp", "direct_block"));
    int fd = open(disk.loop_path().c_str(), O_RDWR | O_DIRECT);
    ASSERT_GE(fd, 0) << strerror(errno);
    void* data = nullptr;
    ASSERT_EQ(0, posix_memalign(&data, 4096, 4096));
    ASSERT_EQ(4096, pread(fd, data, 4096, 0));
    errno = 0;
    EXPECT_EQ(-1, pwrite(fd, static_cast<unsigned char*>(data) + 1, 512, 0));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(4096, pwrite(fd, data, 4096, 0));
    EXPECT_EQ(0, fsync(fd));
    free(data);
    EXPECT_EQ(0, close(fd));
}

class DirectOverlay : public DirectExt4 {
 protected:
    void SetUp() override {
        DirectExt4::SetUp();
        ASSERT_FALSE(HasFatalFailure());
        lower_ = disk_.mount_point() + "/lower";
        upper_ = disk_.mount_point() + "/upper";
        work_ = disk_.mount_point() + "/work";
        merged_ = disk_.mount_point() + "/merged";
        for (const auto* path : {&lower_, &upper_, &work_, &merged_}) ASSERT_EQ(0, mkdir(path->c_str(), 0755));
        int lower = open((lower_ + "/file").c_str(), O_CREAT | O_RDWR, 0600);
        ASSERT_GE(lower, 0);
        ASSERT_EQ(8192, write(lower, aligned_, 8192));
        close(lower);
        std::string options = "lowerdir=" + lower_ + ",upperdir=" + upper_ + ",workdir=" + work_;
        ASSERT_EQ(0, mount("overlay", merged_.c_str(), "overlay", 0, options.c_str())) << strerror(errno);
        mounted_ = true;
    }
    void TearDown() override {
        if (mounted_) { EXPECT_EQ(0, umount(merged_.c_str())); }
        DirectExt4::TearDown();
    }
    std::string lower_, upper_, work_, merged_;
    bool mounted_ = false;
};
TEST_F(DirectOverlay, CopyUpPreservesOriginalInputAlignmentAndDirectRead) {
    int fd = open((merged_ + "/file").c_str(), O_RDWR | O_DIRECT);
    ASSERT_GE(fd, 0) << strerror(errno);
    errno = 0;
    EXPECT_EQ(-1, pwrite(fd, static_cast<unsigned char*>(aligned_) + 1, 4096, 0));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(4096, pwrite(fd, aligned_, 4096, 0));
    EXPECT_EQ(4096, pread(fd, aligned_, 4096, 0));
    EXPECT_EQ(0, close(fd));
}
TEST_F(DirectOverlay, FlagUpdateAndVectorBoundariesReachBackingOwner) {
    int fd = open((merged_ + "/file").c_str(), O_RDWR);
    ASSERT_GE(fd, 0);
    ASSERT_EQ(0, fcntl(fd, F_SETFL, fcntl(fd, F_GETFL) | O_DIRECT));
    iovec invalid[] = {{aligned_, 1}, {static_cast<unsigned char*>(aligned_) + 1, 4095}};
    errno = 0;
    EXPECT_EQ(-1, writev(fd, invalid, 2));
    EXPECT_EQ(EINVAL, errno);
    iovec valid[] = {{aligned_, 4096}, {static_cast<unsigned char*>(aligned_) + 4096, 4096}};
    EXPECT_EQ(8192, writev(fd, valid, 2));
    EXPECT_EQ(0, close(fd));
}

TEST_F(DirectExt4, SectorOverwritePreservesDirtyNeighbouringBytes) {
    const unsigned char neighbour = 'x';
    ASSERT_EQ(1, pwrite(buffered_, &neighbour, 1, 7));
    ASSERT_EQ(512, pwrite(direct_, aligned_, 512, 512));
    unsigned char actual[4096];
    ASSERT_EQ(4096, pread(buffered_, actual, sizeof(actual), 0));
    for (size_t i = 0; i < sizeof(actual); ++i) {
        const unsigned char expected = i == 7 ? 'x' : (i >= 512 && i < 1024 ? 'n' : 'o');
        ASSERT_EQ(expected, actual[i]) << i;
    }
    ASSERT_EQ(0, fsync(buffered_));
    ASSERT_EQ(4096, pread(direct_, aligned_, 4096, 0));
    EXPECT_EQ(0, memcmp(aligned_, actual, sizeof(actual)));
}
TEST_F(DirectExt4, ConcurrentBufferedNeighbourCannotBeDiscarded) {
    std::atomic<bool> start{false};
    std::atomic<int> errors{0};
    std::thread writer([&] {
        while (!start.load(std::memory_order_acquire)) {}
        const unsigned char value = 'x';
        for (int i = 0; i < 100; ++i) {
            if (pwrite(buffered_, &value, 1, 7) != 1) ++errors;
        }
    });
    start.store(true, std::memory_order_release);
    for (int i = 0; i < 100; ++i) {
        if (pwrite(direct_, aligned_, 512, 512) != 512) ++errors;
    }
    writer.join();
    ASSERT_EQ(0, errors.load());
    ASSERT_EQ(0, fsync(buffered_));
    unsigned char actual[4096];
    ASSERT_EQ(4096, pread(buffered_, actual, sizeof(actual), 0));
    EXPECT_EQ('x', actual[7]);
    EXPECT_EQ('o', actual[1024]);
    for (size_t i = 512; i < 1024; ++i) ASSERT_EQ('n', actual[i]);
}
TEST_F(DirectMemoryFs, OverlayOverTmpfsPreservesMemoryDirectPolicy) {
    ASSERT_NO_FATAL_FAILURE(Mount("tmpfs"));
    const std::string lower = root_ + "/lower", upper = root_ + "/upper";
    const std::string work = root_ + "/work", merged = root_ + "/merged";
    for (const auto* path : {&lower, &upper, &work, &merged}) ASSERT_EQ(0, mkdir(path->c_str(), 0755));
    const std::string options = "lowerdir=" + lower + ",upperdir=" + upper + ",workdir=" + work;
    ASSERT_EQ(0, mount("overlay", merged.c_str(), "overlay", 0, options.c_str()));
    overlay_ = merged;
    int fd = open((merged + "/file").c_str(), O_CREAT | O_RDWR | O_DIRECT, 0600);
    ASSERT_GE(fd, 0) << strerror(errno);
    const char bytes[] = "xmemory";
    EXPECT_EQ(6, pwrite(fd, bytes + 1, 6, 1));
    char actual[7] = {};
    EXPECT_EQ(7, pread(fd, actual, sizeof(actual), 0));
    EXPECT_EQ(0, actual[0]);
    EXPECT_EQ(0, memcmp(bytes + 1, actual + 1, 6));
    EXPECT_EQ(0, close(fd));
}

TEST_F(DirectExt4, EmptyFileDirectWriteAllocatesAndPublishesEof) {
    ASSERT_EQ(0, close(direct_));
    direct_ = open((disk_.mount_point() + "/empty").c_str(), O_CREAT | O_RDWR | O_DIRECT, 0600);
    ASSERT_GE(direct_, 0);
    EXPECT_EQ(8192, pwrite(direct_, aligned_, 8192, 0)) << strerror(errno);
    struct stat info{};
    ASSERT_EQ(0, fstat(direct_, &info));
    EXPECT_EQ(8192, info.st_size);
    memset(aligned_, 0, 8192);
    EXPECT_EQ(8192, pread(direct_, aligned_, 8192, 0));
    for (size_t i = 0; i < 8192; ++i) ASSERT_EQ('n', static_cast<unsigned char*>(aligned_)[i]) << i;
}
TEST_F(DirectExt4, DirectWriteAllocatesSparseHoleAndExtendsEof) {
    ASSERT_EQ(0, ftruncate(buffered_, 1024 * 1024));
    ASSERT_EQ(4096, pwrite(direct_, aligned_, 4096, 512 * 1024)) << strerror(errno);
    unsigned char zero[512];
    ASSERT_EQ(512, pread(buffered_, zero, sizeof(zero), 256 * 1024));
    for (unsigned char byte : zero) ASSERT_EQ(0, byte);
    ASSERT_EQ(4096, pwrite(direct_, aligned_, 4096, 1024 * 1024 + 4096)) << strerror(errno);
    struct stat info{};
    ASSERT_EQ(0, fstat(direct_, &info));
    EXPECT_EQ(1024 * 1024 + 8192, info.st_size);
    memset(aligned_, 0, 4096);
    EXPECT_EQ(4096, pread(buffered_, aligned_, 4096, 512 * 1024));
    for (size_t i = 0; i < 4096; ++i) ASSERT_EQ('n', static_cast<unsigned char*>(aligned_)[i]) << i;
}
TEST_F(DirectExt4, UnrelatedDelayedAppendCannotBlockDirectOverwrite) {
    ASSERT_EQ(8192, pwrite(buffered_, aligned_, 8192, 3 * 65536));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(10);
        _exit(pwrite(direct_, aligned_, 512, 0) == 512 ? 0 : 13);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
    ASSERT_EQ(0, fsync(buffered_));
    unsigned char tail[8192];
    ASSERT_EQ(8192, pread(buffered_, tail, sizeof(tail), 3 * 65536));
    EXPECT_EQ(0, memcmp(aligned_, tail, sizeof(tail)));
}

void CheckStorageLimitIndependence(const std::string& loop_path) {
    int device = open(loop_path.c_str(), O_RDWR | O_DIRECT);
    ASSERT_GE(device, 0) << strerror(errno);
    char ordinary_path[] = "/tmp/direct-limit-control-XXXXXX";
    int ordinary = mkstemp(ordinary_path);
    ASSERT_GE(ordinary, 0);
    ASSERT_EQ(0, unlink(ordinary_path));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(15);
        signal(SIGXFSZ, SIG_IGN);
        void* sector = nullptr;
        if (posix_memalign(&sector, 4096, 4096) != 0) _exit(20);
        memset(sector, 's', 4096);
        rlimit limit{4096, 4096};
        if (setrlimit(RLIMIT_FSIZE, &limit) != 0) _exit(21);
        if (pwrite(device, sector, 4096, 8192) != 4096) _exit(22);
        errno = 0;
        if (pwrite(ordinary, sector, 1, 4096) != -1 || errno != EFBIG) _exit(23);
        _exit(0);
    }
    int status = 0;
    EXPECT_EQ(child, waitpid(child, &status, 0));
    EXPECT_TRUE(WIFEXITED(status)) << status;
    if (WIFEXITED(status)) { EXPECT_EQ(0, WEXITSTATUS(status)); }
    void* sector = nullptr;
    ASSERT_EQ(0, posix_memalign(&sector, 4096, 4096));
    EXPECT_EQ(4096, pread(device, sector, 4096, 8192));
    for (size_t i = 0; i < 4096; ++i) {
        if (static_cast<unsigned char*>(sector)[i] != 's') {
            ADD_FAILURE() << "storage write was not published at byte " << i;
            break;
        }
    }
    free(sector);
    EXPECT_EQ(0, close(device));
    EXPECT_EQ(0, close(ordinary));
}
TEST(DirectBlock, StorageBackingDoesNotInheritSubmitterFileSizeLimit) {
    dunitest::LoopExt4 disk;
    ASSERT_NO_FATAL_FAILURE(disk.SetUp("ext4_inode_identity.img", "/tmp", "direct_storage_limit"));
    ASSERT_NO_FATAL_FAILURE(CheckStorageLimitIndependence(disk.loop_path()));
}
TEST_F(DirectOverlay, LoopBackingPropagatesStorageAdmissionThroughOverlay) {
    dunitest::LoopExt4 disk;
    ASSERT_NO_FATAL_FAILURE(disk.SetUp("ext4_inode_identity.img", merged_.c_str(), "overlay_storage_limit"));
    ASSERT_NO_FATAL_FAILURE(CheckStorageLimitIndependence(disk.loop_path()));
}
TEST_F(DirectMemoryFs, PathOnlyOpenDoesNotAcquireDataDirectPolicy) {
    ASSERT_NO_FATAL_FAILURE(Mount("ramfs"));
    const auto path = root_ + "/file";
    int file = open(path.c_str(), O_CREAT | O_RDWR, 0600);
    ASSERT_GE(file, 0);
    close(file);
    int path_only = open(path.c_str(), O_PATH | O_DIRECT);
    EXPECT_GE(path_only, 0) << strerror(errno);
    if (path_only >= 0) close(path_only);
    EXPECT_EQ(0, unlink(path.c_str()));
}

void CheckPacketWritesDiscardUnreadTail(int reader, int writer) {
    ASSERT_EQ(4, write(writer, "abcd", 4));
    ASSERT_EQ(2, write(writer, "xy", 2));
    char bytes[2] = {};
    ASSERT_EQ(2, read(reader, bytes, sizeof(bytes)));
    EXPECT_EQ(0, memcmp(bytes, "ab", 2));
    ASSERT_EQ(2, read(reader, bytes, sizeof(bytes)));
    EXPECT_EQ(0, memcmp(bytes, "xy", 2));
    errno = 0;
    EXPECT_EQ(-1, read(reader, bytes, sizeof(bytes)));
    EXPECT_EQ(EAGAIN, errno);
}
TEST_F(DirectMemoryFs, NamedFifoDirectOpenRejectsWithoutRetainingReader) {
    ASSERT_NO_FATAL_FAILURE(Mount("tmpfs"));
    const auto path = root_ + "/fifo";
    ASSERT_EQ(0, mkfifo(path.c_str(), 0600));
    errno = 0;
    int fd = open(path.c_str(), O_RDONLY | O_NONBLOCK | O_DIRECT);
    const int error = errno;
    if (fd >= 0) close(fd);
    EXPECT_EQ(-1, fd);
    EXPECT_EQ(EINVAL, error);
    // The failed post-open capability check must release its FIFO opener.
    errno = 0;
    int writer = open(path.c_str(), O_WRONLY | O_NONBLOCK);
    const int writer_error = errno;
    if (writer >= 0) close(writer);
    EXPECT_EQ(-1, writer);
    EXPECT_EQ(ENXIO, writer_error);
    EXPECT_EQ(0, unlink(path.c_str()));
}
TEST_F(DirectMemoryFs, NamedFifoFlagUpdateEnablesPacketWrites) {
    ASSERT_NO_FATAL_FAILURE(Mount("tmpfs"));
    const auto path = root_ + "/fifo";
    ASSERT_EQ(0, mkfifo(path.c_str(), 0600));
    int reader = open(path.c_str(), O_RDONLY | O_NONBLOCK);
    ASSERT_GE(reader, 0);
    int writer = open(path.c_str(), O_WRONLY | O_NONBLOCK);
    ASSERT_GE(writer, 0);
    EXPECT_EQ(0, fcntl(reader, F_SETFL, fcntl(reader, F_GETFL) | O_DIRECT));
    EXPECT_EQ(0, fcntl(writer, F_SETFL, fcntl(writer, F_GETFL) | O_DIRECT));
    CheckPacketWritesDiscardUnreadTail(reader, writer);
    EXPECT_EQ(0, close(writer));
    EXPECT_EQ(0, close(reader));
    EXPECT_EQ(0, unlink(path.c_str()));
}
TEST(DirectPipe, Pipe2DirectRetainsPacketSemantics) {
    int descriptors[2];
    ASSERT_EQ(0, pipe2(descriptors, O_DIRECT | O_NONBLOCK | O_CLOEXEC));
    CheckPacketWritesDiscardUnreadTail(descriptors[0], descriptors[1]);
    EXPECT_EQ(0, close(descriptors[1]));
    EXPECT_EQ(0, close(descriptors[0]));
}
TEST(DirectPipe, ProcReopenOfAnonymousPipeUsesOrdinaryOpenPolicy) {
    int descriptors[2];
    ASSERT_EQ(0, pipe2(descriptors, O_DIRECT | O_NONBLOCK | O_CLOEXEC));
    const auto path = "/proc/self/fd/" + std::to_string(descriptors[0]);
    errno = 0;
    int reopened = open(path.c_str(), O_RDONLY | O_NONBLOCK | O_DIRECT);
    const int error = errno;
    if (reopened >= 0) close(reopened);
    EXPECT_EQ(-1, reopened);
    EXPECT_EQ(EINVAL, error);
    CheckPacketWritesDiscardUnreadTail(descriptors[0], descriptors[1]);
    EXPECT_EQ(0, close(descriptors[1]));
    EXPECT_EQ(0, close(descriptors[0]));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
