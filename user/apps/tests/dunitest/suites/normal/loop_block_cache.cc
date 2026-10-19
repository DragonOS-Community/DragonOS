#include <gtest/gtest.h>
#include "loop_ext4_test_support.h"

#include <array>
#include <cerrno>
#include <cstring>
#include <fcntl.h>
#include <linux/fs.h>
#include <linux/loop.h>
#include <string>
#include <sys/ioctl.h>
#include <sys/mman.h>
#include <signal.h>
#include <time.h>
#include <sys/wait.h>
#include <unistd.h>
#include <vector>

namespace {
struct CacheFd {
    int value = -1;
    ~CacheFd() { if (value >= 0) close(value); }
};
struct CacheLoop {
    CacheFd fd;
    bool bound = false;
    ~CacheLoop() { if (bound) ioctl(fd.value, LOOP_CLR_FD, 0); }
};

uint64_t CacheNow() {
    timespec value {};
    if (clock_gettime(CLOCK_MONOTONIC, &value)) return 0;
    return uint64_t(value.tv_sec) * 1000000000ULL + value.tv_nsec;
}

bool WaitCacheChild(pid_t child, int* status) {
    const uint64_t started = CacheNow();
    const uint64_t deadline = started + 15ULL * 1000000000ULL;
    while (started) {
        const uint64_t now = CacheNow();
        if (!now || now >= deadline) break;
        pid_t result = waitpid(child, status, WNOHANG);
        if (result == child) return true;
        if (result < 0 && errno != EINTR) return false;
        usleep(1000);
    }
    kill(child, SIGKILL);
    // A broken kernel must be reported by the deadline, not hidden behind an
    // unbounded userspace waitpid. The outer dunitest timeout owns a guest
    // whose stuck kernel I/O cannot deliver even SIGKILL.
    const uint64_t cleanup = CacheNow() + 2ULL * 1000000000ULL;
    while (true) {
        const uint64_t now = CacheNow();
        if (!now || now >= cleanup) break;
        if (waitpid(child, status, WNOHANG) == child) return false;
        usleep(1000);
    }
    return false;
}

class LoopBlockCache : public testing::Test {
protected:
    void SetUp() override {
        const char* directory = getenv("DUNITEST_LOOP_BACKING_DIR");
        SetUpIn(directory ? directory : "/tmp");
    }
    void SetUpIn(const char* directory) {
        page_ = sysconf(_SC_PAGESIZE);
        std::string pattern = std::string(directory) + "/loop_cache_XXXXXX";
        std::vector<char> name(pattern.begin(), pattern.end());
        name.push_back(0);
        backing_ = mkstemp(name.data());
        ASSERT_GE(backing_, 0) << strerror(errno);
        path_ = name.data();
        std::vector<char> initial(4 * page_, 'a');
        ASSERT_EQ(static_cast<ssize_t>(initial.size()),
                  pwrite(backing_, initial.data(), initial.size(), 0));
        ASSERT_EQ(0, fsync(backing_));
        int control = open("/dev/loop-control", O_RDWR | O_CLOEXEC);
        ASSERT_GE(control, 0) << strerror(errno);
        int minor = ioctl(control, LOOP_CTL_GET_FREE, 0);
        int error = errno;
        close(control);
        ASSERT_GE(minor, 0) << strerror(error);
        loop_path_ = "/dev/loop" + std::to_string(minor);
        loop_ = open(loop_path_.c_str(), O_RDWR | O_CLOEXEC);
        ASSERT_GE(loop_, 0) << strerror(errno);
        ASSERT_EQ(0, ioctl(loop_, LOOP_SET_FD, backing_)) << strerror(errno);
        bound_ = true;
    }
    void TearDown() override {
        if (bound_) {
            EXPECT_EQ(0, ioctl(loop_, LOOP_CLR_FD, 0)) << strerror(errno);
        }
        if (loop_ >= 0) close(loop_);
        if (backing_ >= 0) close(backing_);
        if (!path_.empty()) unlink(path_.c_str());
    }
    void ExpectByte(int fd, off_t offset, char expected) {
        char value = 0;
        ASSERT_EQ(1, pread(fd, &value, 1, offset)) << strerror(errno);
        EXPECT_EQ(expected, value);
    }
    void BackingByte(off_t offset, char value) {
        ASSERT_EQ(1, pwrite(backing_, &value, 1, offset));
        ASSERT_EQ(0, fsync(backing_));
    }
    void RequireDirectBacking() {
        int flags = fcntl(backing_, F_GETFL);
        ASSERT_GE(flags, 0);
        ASSERT_EQ(0, fcntl(backing_, F_SETFL, flags | O_DIRECT));
        void* probe = nullptr;
        ASSERT_EQ(0, posix_memalign(&probe, page_, page_));
        errno = 0;
        ssize_t result = pread(backing_, probe, page_, 513);
        int error = errno;
        free(probe);
        ASSERT_EQ(-1, result);
        ASSERT_EQ(EINVAL, error) << "test requires a direct-capable backing owner";
    }
    void SetOffset(size_t offset) {
        loop_info64 status {};
        ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
        status.lo_offset = offset;
        ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status)) << strerror(errno);
    }
    void BindAnother(int backing, CacheLoop* another) {
        CacheFd control {open("/dev/loop-control", O_RDWR | O_CLOEXEC)};
        ASSERT_GE(control.value, 0);
        int minor = ioctl(control.value, LOOP_CTL_GET_FREE, 0);
        ASSERT_GE(minor, 0);
        std::string path = "/dev/loop" + std::to_string(minor);
        another->fd.value = open(path.c_str(), O_RDWR | O_CLOEXEC);
        ASSERT_GE(another->fd.value, 0);
        ASSERT_EQ(0, ioctl(another->fd.value, LOOP_SET_FD, backing));
        another->bound = true;
    }
    void ClearAndWait() {
        for (unsigned i = 0; i < 2000; ++i) {
            int result = ioctl(loop_, LOOP_CLR_FD, 0);
            ASSERT_TRUE(result == 0 || errno == ENXIO) << strerror(errno);
            loop_info64 status {};
            errno = 0;
            result = ioctl(loop_, LOOP_GET_STATUS64, &status);
            if (result == -1 && errno == ENXIO) { bound_ = false; return; }
            ASSERT_EQ(0, result) << strerror(errno);
            ASSERT_NE(0U, status.lo_flags & LO_FLAGS_AUTOCLEAR);
            usleep(1000);
        }
        FAIL() << "loop clear did not detach within deadline";
    }
    size_t page_ = 0;
    int backing_ = -1, loop_ = -1;
    std::string path_, loop_path_;
    bool bound_ = false;
};

class LoopBlockDirectCache : public LoopBlockCache {
protected:
    void SetUp() override {
        ASSERT_NO_FATAL_FAILURE(fs_.SetUp("ext4_inode_identity.img", "/tmp", "loop_block_cache"));
        ASSERT_NO_FATAL_FAILURE(fs_.Mount());
        mounted_ = true;
        ASSERT_NO_FATAL_FAILURE(SetUpIn(fs_.mount_point().c_str()));
    }
    void TearDown() override {
        LoopBlockCache::TearDown();
        if (mounted_) {
            ASSERT_NO_FATAL_FAILURE(fs_.Detach());
            ASSERT_NO_FATAL_FAILURE(fs_.FinishDetached());
        }
    }
    dunitest::LoopExt4 fs_;
    bool mounted_ = false;
};

TEST_F(LoopBlockCache, WarmRawPageSurvivesExternalBackingWriteUntilInvalidated) {
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 17, 'a'));
    ASSERT_NO_FATAL_FAILURE(BackingByte(17, 'b'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, 17, 'b'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 17, 'a'));
    ASSERT_EQ(0, ioctl(loop_, BLKFLSBUF, 0)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 17, 'b'));
}

TEST_F(LoopBlockCache, DirectWriteInvalidatesWarmRawPage) {
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 17, 'a'));
    int direct = open(loop_path_.c_str(), O_RDWR | O_DIRECT | O_CLOEXEC);
    ASSERT_GE(direct, 0) << strerror(errno);
    void* bytes = nullptr;
    ASSERT_EQ(0, posix_memalign(&bytes, page_, page_));
    memset(bytes, 'd', page_);
    ssize_t count = pwrite(direct, bytes, page_, 0);
    int error = errno;
    free(bytes);
    close(direct);
    ASSERT_EQ(static_cast<ssize_t>(page_), count) << strerror(error);
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 17, 'd'));
}

TEST_F(LoopBlockCache, FsyncPublishesBufferedBytesAndRetainsBackingCursor) {
    ASSERT_EQ(137, lseek(backing_, 137, SEEK_SET));
    std::vector<char> bytes(page_, 'w');
    ASSERT_EQ(static_cast<ssize_t>(bytes.size()), pwrite(loop_, bytes.data(), bytes.size(), 0));
    ASSERT_EQ(0, fsync(loop_)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, 17, 'w'));
    EXPECT_EQ(137, lseek(backing_, 0, SEEK_CUR));
}

TEST_F(LoopBlockCache, MappingChangeFlushesOldDirtyPageAndDiscardsOldCleanPage) {
    ASSERT_NO_FATAL_FAILURE(BackingByte(page_, 'b'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 0, 'a'));
    std::vector<char> bytes(page_, 'w');
    ASSERT_EQ(static_cast<ssize_t>(page_), pwrite(loop_, bytes.data(), bytes.size(), 0));
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
    status.lo_offset = page_;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status)) << strerror(errno);
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, 0, 'w'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 0, 'b'));
}

TEST_F(LoopBlockCache, WillNeedPublicationCannotSurviveOffsetChange) {
    ASSERT_NO_FATAL_FAILURE(BackingByte(page_, 'b'));
    ASSERT_EQ(0, posix_fadvise(loop_, 0, page_, POSIX_FADV_WILLNEED));
    ASSERT_NO_FATAL_FAILURE(SetOffset(page_));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 0, 'b'));
}

TEST_F(LoopBlockCache, TaggedAsyncWriteIsDrainedBeforeOffsetChange) {
    ASSERT_NO_FATAL_FAILURE(BackingByte(page_, 'b'));
    std::vector<char> bytes(page_, 't');
    ASSERT_EQ(static_cast<ssize_t>(page_), pwrite(loop_, bytes.data(), bytes.size(), 0));
    ASSERT_EQ(0, sync_file_range(loop_, 0, page_, SYNC_FILE_RANGE_WRITE));
    ASSERT_NO_FATAL_FAILURE(SetOffset(page_));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, 0, 't'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, page_, 'b'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 0, 'b'));
    ASSERT_EQ(0, sync_file_range(loop_, 0, page_, SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WAIT_AFTER));
}

TEST_F(LoopBlockCache, TaggedAsyncWriteDoesNotReachReboundBacking) {
    std::vector<char> bytes(page_, 't');
    ASSERT_EQ(static_cast<ssize_t>(page_), pwrite(loop_, bytes.data(), bytes.size(), 0));
    ASSERT_EQ(0, sync_file_range(loop_, 0, page_, SYNC_FILE_RANGE_WRITE));
    ASSERT_NO_FATAL_FAILURE(ClearAndWait());
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, 0, 't'));
    std::string name = path_ + ".next";
    CacheFd next {open(name.c_str(), O_CREAT | O_EXCL | O_RDWR | O_CLOEXEC, 0600)};
    ASSERT_GE(next.value, 0);
    ASSERT_EQ(0, unlink(name.c_str()));
    std::vector<char> initial(4 * page_, 'n');
    ASSERT_EQ(static_cast<ssize_t>(initial.size()), pwrite(next.value, initial.data(), initial.size(), 0));
    ASSERT_EQ(0, fsync(next.value));
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_FD, next.value));
    bound_ = true;
    ASSERT_EQ(0, sync_file_range(loop_, 0, page_, SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WAIT_AFTER));
    ASSERT_EQ(0, fsync(loop_));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 0, 'n'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(next.value, 0, 'n'));
}

TEST_F(LoopBlockCache, LastCloseFlushesDirtyRawData) {
    std::vector<char> bytes(page_, 'c');
    ASSERT_EQ(static_cast<ssize_t>(page_), pwrite(loop_, bytes.data(), bytes.size(), 0));
    ASSERT_EQ(0, close(loop_));
    loop_ = -1;
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, 17, 'c'));
    loop_ = open(loop_path_.c_str(), O_RDWR | O_CLOEXEC);
    ASSERT_GE(loop_, 0);
}

TEST_F(LoopBlockCache, WriteFaultReturnsCommittedPagePrefixAndAdvancesCursor) {
    void* bytes = mmap(nullptr, 2 * page_, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, bytes);
    memset(bytes, 'p', page_);
    ASSERT_EQ(0, mprotect(static_cast<char*>(bytes) + page_, page_, PROT_NONE));
    ssize_t count = write(loop_, bytes, 2 * page_);
    int error = errno;
    EXPECT_EQ(static_cast<ssize_t>(page_), count) << strerror(error);
    EXPECT_EQ(static_cast<off_t>(page_), lseek(loop_, 0, SEEK_CUR));
    EXPECT_EQ(0, munmap(bytes, 2 * page_));
    ASSERT_EQ(0, fsync(loop_));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, 17, 'p'));
    ASSERT_NO_FATAL_FAILURE(ExpectByte(backing_, page_, 'a'));
}

TEST_F(LoopBlockCache, ReadFaultReturnsCommittedPagePrefixAndAdvancesCursor) {
    void* bytes = mmap(nullptr, 2 * page_, PROT_READ | PROT_WRITE,
                       MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, bytes);
    ASSERT_EQ(0, mprotect(static_cast<char*>(bytes) + page_, page_, PROT_NONE));
    ssize_t count = read(loop_, bytes, 2 * page_);
    int error = errno;
    EXPECT_EQ(static_cast<ssize_t>(page_), count) << strerror(error);
    EXPECT_EQ(static_cast<off_t>(page_), lseek(loop_, 0, SEEK_CUR));
    if (count == static_cast<ssize_t>(page_)) {
        EXPECT_EQ(std::string(page_, 'a'), std::string(static_cast<char*>(bytes), page_));
    }
    EXPECT_EQ(0, munmap(bytes, 2 * page_));
}

TEST_F(LoopBlockDirectCache, FullPageBufferedWriteDefersMisalignedBackingErrorToFsync) {
    ASSERT_NO_FATAL_FAILURE(RequireDirectBacking());
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_DIRECT_IO, 0));
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
    status.lo_offset = 513;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status)) << strerror(errno);
    std::vector<char> bytes(page_, 'e');
    ASSERT_EQ(static_cast<ssize_t>(page_), pwrite(loop_, bytes.data(), bytes.size(), 0))
        << strerror(errno);
    // Linux BLKFLSBUF ignores sync_blockdev's terminal writeback errno.
    EXPECT_EQ(0, ioctl(loop_, BLKFLSBUF, 0));
    errno = 0;
    EXPECT_EQ(-1, fsync(loop_));
    EXPECT_EQ(EIO, errno);
    // The completed failing writeback is reported once to this OFD; an
    // unchanged clean mapping does not resubmit the same failing bytes.
    EXPECT_EQ(0, fsync(loop_));
}

TEST_F(LoopBlockDirectCache, ColdPartialPageWritePrereadsAndReportsMisalignedBackingError) {
    ASSERT_NO_FATAL_FAILURE(RequireDirectBacking());
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
    status.lo_offset = 513;
    status.lo_sizelimit = 8192;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status));
    std::array<char, 512> bytes;
    bytes.fill('s');
    errno = 0;
    ssize_t written = pwrite(loop_, bytes.data(), bytes.size(), 0);
    int error = errno;
    RecordProperty("write_result", written);
    RecordProperty("write_errno", error);
    EXPECT_EQ(-1, written);
    EXPECT_EQ(EIO, error);
    EXPECT_EQ(0, fsync(loop_));
}

TEST_F(LoopBlockDirectCache, WritebackErrorRetiresDirtyGenerationButRetainsCachedBytes) {
    ASSERT_NO_FATAL_FAILURE(RequireDirectBacking());
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
    status.lo_offset = 513;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status));
    std::vector<char> bytes(page_, 'e');
    ASSERT_EQ(static_cast<ssize_t>(page_), pwrite(loop_, bytes.data(), bytes.size(), 0));
    errno = 0;
    ASSERT_EQ(-1, fsync(loop_));
    ASSERT_EQ(EIO, errno);
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 17, 'e'));
    EXPECT_EQ(0, fsync(loop_));
}

TEST_F(LoopBlockDirectCache, DirectReadReportsEarlierDirtyWritebackError) {
    ASSERT_NO_FATAL_FAILURE(RequireDirectBacking());
    ASSERT_NO_FATAL_FAILURE(SetOffset(513));
    std::vector<char> dirty(page_, 'e');
    ASSERT_EQ(static_cast<ssize_t>(page_), pwrite(loop_, dirty.data(), dirty.size(), 0));
    CacheFd direct {open(loop_path_.c_str(), O_RDONLY | O_DIRECT | O_CLOEXEC)};
    ASSERT_GE(direct.value, 0);
    void* bytes = nullptr;
    ASSERT_EQ(0, posix_memalign(&bytes, page_, page_));
    errno = 0;
    ssize_t result = pread(direct.value, bytes, page_, 0);
    int error = errno;
    free(bytes);
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EIO, error);
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 17, 'e'));
}

TEST_F(LoopBlockDirectCache, DirectWriteReportsEarlierDirtyWritebackError) {
    ASSERT_NO_FATAL_FAILURE(RequireDirectBacking());
    ASSERT_NO_FATAL_FAILURE(SetOffset(513));
    std::vector<char> dirty(page_, 'e');
    ASSERT_EQ(static_cast<ssize_t>(page_), pwrite(loop_, dirty.data(), dirty.size(), 0));
    CacheFd direct {open(loop_path_.c_str(), O_RDWR | O_DIRECT | O_CLOEXEC)};
    ASSERT_GE(direct.value, 0);
    void* bytes = nullptr;
    ASSERT_EQ(0, posix_memalign(&bytes, page_, page_));
    memset(bytes, 'd', page_);
    errno = 0;
    ssize_t result = pwrite(direct.value, bytes, page_, 0);
    int error = errno;
    free(bytes);
    EXPECT_EQ(-1, result);
    EXPECT_EQ(EIO, error);
    ASSERT_NO_FATAL_FAILURE(ExpectByte(loop_, 17, 'e'));
}

TEST_F(LoopBlockDirectCache, NestedSyncBackingAndAsyncInnerWritebackComplete) {
    CacheFd sync_inner {open(loop_path_.c_str(), O_RDWR | O_SYNC | O_CLOEXEC)};
    ASSERT_GE(sync_inner.value, 0);
    CacheLoop outer;
    ASSERT_NO_FATAL_FAILURE(BindAnother(sync_inner.value, &outer));
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(12);
        std::vector<char> lower(page_), upper(page_);
        for (unsigned round = 0; round < 64; ++round) {
            std::fill(lower.begin(), lower.end(), 'A' + round % 26);
            std::fill(upper.begin(), upper.end(), 'a' + round % 26);
            if (pwrite(outer.fd.value, upper.data(), page_, 0) != static_cast<ssize_t>(page_)) _exit(1);
            if (pwrite(loop_, lower.data(), page_, page_) != static_cast<ssize_t>(page_)) _exit(2);
            if (sync_file_range(outer.fd.value, 0, page_, SYNC_FILE_RANGE_WRITE)) _exit(3);
            if (sync_file_range(loop_, page_, page_, SYNC_FILE_RANGE_WRITE)) _exit(4);
        }
        if (fsync(outer.fd.value) || fsync(loop_)) _exit(5);
        char first = 0, second = 0;
        if (pread(backing_, &first, 1, 0) != 1 || pread(backing_, &second, 1, page_) != 1) _exit(6);
        _exit(first == 'a' + 63 % 26 && second == 'A' + 63 % 26 ? 0 : 7);
    }
    int status = 0;
    ASSERT_TRUE(WaitCacheChild(child, &status)) << "nested raw writeback exceeded its deadline";
    ASSERT_TRUE(WIFEXITED(status)) << "child status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}

TEST_F(LoopBlockDirectCache, FullCapacitySubpageWriteRetainsOriginalCacheBlockSize) {
    ASSERT_NO_FATAL_FAILURE(RequireDirectBacking());
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
    status.lo_offset = 513;
    status.lo_sizelimit = 512;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status));
    std::array<char, 512> bytes;
    bytes.fill('t');
    errno = 0;
    ssize_t written = pwrite(loop_, bytes.data(), bytes.size(), 0);
    int error = errno;
    RecordProperty("write_result", written);
    RecordProperty("write_errno", error);
    EXPECT_EQ(-1, written);
    EXPECT_EQ(EIO, error);
    EXPECT_EQ(0, fsync(loop_));
}

TEST_F(LoopBlockDirectCache, ColdBufferedReadReturnsPrefixAfterDirectBackingTruncate) {
    ASSERT_NO_FATAL_FAILURE(RequireDirectBacking());
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_DIRECT_IO, 1));
    ASSERT_EQ(0, ftruncate(backing_, page_));
    std::vector<char> bytes(2 * page_, '!');
    errno = 0;
    ssize_t count = read(loop_, bytes.data(), bytes.size());
    int error = errno;
    RecordProperty("read_result", count);
    RecordProperty("read_errno", error);
    EXPECT_EQ(static_cast<ssize_t>(page_), count) << strerror(error);
    EXPECT_EQ(static_cast<off_t>(page_), lseek(loop_, 0, SEEK_CUR));
    if (count > 0) {
        EXPECT_EQ(std::string(page_, 'a'), std::string(bytes.data(), page_));
    }
}

TEST_F(LoopBlockDirectCache, ReopenedSubpageCapacityRetainsCacheBlockSize) {
    ASSERT_NO_FATAL_FAILURE(RequireDirectBacking());
    loop_info64 status {};
    ASSERT_EQ(0, ioctl(loop_, LOOP_GET_STATUS64, &status));
    status.lo_offset = 513;
    status.lo_sizelimit = 512;
    ASSERT_EQ(0, ioctl(loop_, LOOP_SET_STATUS64, &status));
    ASSERT_EQ(0, close(loop_));
    loop_ = open(loop_path_.c_str(), O_RDWR | O_CLOEXEC);
    ASSERT_GE(loop_, 0);
    size_t block_size = 0;
    ASSERT_EQ(0, ioctl(loop_, BLKBSZGET, &block_size));
    RecordProperty("cache_block_size", block_size);
    EXPECT_EQ(page_, block_size);
    std::array<char, 512> bytes;
    bytes.fill('t');
    errno = 0;
    ssize_t written = pwrite(loop_, bytes.data(), bytes.size(), 0);
    int error = errno;
    RecordProperty("write_result", written);
    RecordProperty("write_errno", error);
    EXPECT_EQ(-1, written);
    EXPECT_EQ(EIO, error);
    EXPECT_EQ(0, fsync(loop_));
}

TEST_F(LoopBlockCache, FlushBufferRequiresAdministrativeCapability) {
    pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        if (setgid(65534) != 0 || setuid(65534) != 0) _exit(2);
        errno = 0;
        int result = ioctl(loop_, BLKFLSBUF, 0);
        _exit(result == -1 && errno == EACCES ? 0 : 1);
    }
    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0));
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
