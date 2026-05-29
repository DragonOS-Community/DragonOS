#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <string>

#ifndef __NR_sync_file_range
#if defined(__x86_64__)
#define __NR_sync_file_range 277
#elif defined(__riscv) || defined(__loongarch64)
#define __NR_sync_file_range 84
#else
#error "__NR_sync_file_range is not defined for this architecture"
#endif
#endif

#ifndef __NR_syncfs
#if defined(__x86_64__)
#define __NR_syncfs 306
#elif defined(__riscv) || defined(__loongarch64)
#define __NR_syncfs 267
#else
#error "__NR_syncfs is not defined for this architecture"
#endif
#endif

#ifndef SYNC_FILE_RANGE_WAIT_BEFORE
#define SYNC_FILE_RANGE_WAIT_BEFORE 1
#endif

#ifndef SYNC_FILE_RANGE_WRITE
#define SYNC_FILE_RANGE_WRITE 2
#endif

#ifndef SYNC_FILE_RANGE_WAIT_AFTER
#define SYNC_FILE_RANGE_WAIT_AFTER 4
#endif

namespace {

constexpr const char* kErrSeqInjectPath = "/sys/kernel/debug/errseq/inject";
constexpr size_t kPageSize = 4096;

long RawSyncFileRange(int fd, off_t offset, off_t nbytes, unsigned int flags) {
    return syscall(__NR_sync_file_range, fd, offset, nbytes, flags);
}

long RawSyncfs(int fd) {
    return syscall(__NR_syncfs, fd);
}

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/tmp/dunitest_errseq_XXXXXX";
        fd_ = mkstemp(tmpl);
        if (fd_ >= 0) {
            path_ = tmpl;
            const char data[] = "DragonOS errseq writeback reporting\n";
            initialized_ =
                write(fd_, data, sizeof(data) - 1) == static_cast<ssize_t>(sizeof(data) - 1);
        }
    }

    ~TempFile() {
        if (fd_ >= 0) {
            close(fd_);
        }
        if (!path_.empty()) {
            unlink(path_.c_str());
        }
    }

    TempFile(const TempFile&) = delete;
    TempFile& operator=(const TempFile&) = delete;

    bool valid() const {
        return fd_ >= 0 && initialized_;
    }

    int fd() const {
        return fd_;
    }

    const char* path() const {
        return path_.c_str();
    }

  private:
    std::string path_;
    int fd_ = -1;
    bool initialized_ = false;
};

void ExpectErrnoEq(int expected_errno) {
    EXPECT_EQ(expected_errno, errno) << "got errno=" << errno << " (" << strerror(errno) << ")";
}

void ExpectFailsWithErrno(long result, int expected_errno) {
    EXPECT_EQ(-1, result);
    ExpectErrnoEq(expected_errno);
}

void ExpectSucceeds(long result) {
    EXPECT_EQ(0, result) << "errno=" << errno << " (" << strerror(errno) << ")";
}

void InjectError(const char* target, int fd, const char* error) {
    int inject_fd = open(kErrSeqInjectPath, O_WRONLY);
    ASSERT_GE(inject_fd, 0) << "open(" << kErrSeqInjectPath << ") failed: errno=" << errno << " ("
                            << strerror(errno) << ")";

    char command[128];
    int len = snprintf(command, sizeof(command), "%s %d %s\n", target, fd, error);
    ASSERT_GT(len, 0);
    ASSERT_LT(static_cast<size_t>(len), sizeof(command));

    ssize_t written = write(inject_fd, command, static_cast<size_t>(len));
    EXPECT_EQ(len, written) << "write inject command failed: errno=" << errno << " ("
                            << strerror(errno) << ")";
    EXPECT_EQ(0, close(inject_fd));
}

int OpenSecondDescription(const TempFile& file) {
    int fd = open(file.path(), O_RDWR);
    EXPECT_GE(fd, 0) << "open second fd failed: errno=" << errno << " (" << strerror(errno)
                     << ")";
    return fd;
}

}  // namespace

TEST(ErrSeqWritebackReporting, FsyncReportsMappingErrorPerOpenDescription) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    int second = OpenSecondDescription(file);
    ASSERT_GE(second, 0);

    InjectError("mapping", file.fd(), "EIO");

    errno = 0;
    ExpectFailsWithErrno(fsync(file.fd()), EIO);
    errno = 0;
    ExpectSucceeds(fsync(file.fd()));

    errno = 0;
    ExpectFailsWithErrno(fsync(second), EIO);

    int late = OpenSecondDescription(file);
    ASSERT_GE(late, 0);
    errno = 0;
    ExpectSucceeds(fsync(late));

    errno = 0;
    ExpectFailsWithErrno(RawSyncfs(file.fd()), EIO);

    close(late);
    close(second);
}

TEST(ErrSeqWritebackReporting, FdatasyncReportsMappingErrorOnce) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    InjectError("mapping", file.fd(), "ENOSPC");

    errno = 0;
    ExpectFailsWithErrno(fdatasync(file.fd()), ENOSPC);
    errno = 0;
    ExpectSucceeds(fdatasync(file.fd()));

    errno = 0;
    ExpectFailsWithErrno(RawSyncfs(file.fd()), ENOSPC);
}

TEST(ErrSeqWritebackReporting, SyncFileRangeWaitConsumesMappingError) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    InjectError("mapping", file.fd(), "EIO");

    errno = 0;
    ExpectSucceeds(RawSyncFileRange(file.fd(), 0, 0, SYNC_FILE_RANGE_WRITE));

    errno = 0;
    ExpectFailsWithErrno(RawSyncFileRange(file.fd(), 0, 0, SYNC_FILE_RANGE_WAIT_AFTER), EIO);

    errno = 0;
    ExpectSucceeds(RawSyncFileRange(file.fd(), 0, 0, SYNC_FILE_RANGE_WAIT_BEFORE));

    errno = 0;
    ExpectFailsWithErrno(RawSyncfs(file.fd()), EIO);
}

TEST(ErrSeqWritebackReporting, SyncfsReportsSuperblockErrorPerOpenDescription) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    int second = OpenSecondDescription(file);
    ASSERT_GE(second, 0);

    InjectError("superblock", file.fd(), "ENOSPC");

    errno = 0;
    ExpectFailsWithErrno(RawSyncfs(file.fd()), ENOSPC);
    errno = 0;
    ExpectSucceeds(RawSyncfs(file.fd()));

    errno = 0;
    ExpectFailsWithErrno(RawSyncfs(second), ENOSPC);

    int late = OpenSecondDescription(file);
    ASSERT_GE(late, 0);
    errno = 0;
    ExpectSucceeds(RawSyncfs(late));

    close(late);
    close(second);
}

TEST(ErrSeqWritebackReporting, MsyncSharedMappingReportsMappingErrorOnce) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    ASSERT_EQ(0, ftruncate(file.fd(), static_cast<off_t>(kPageSize)));

    void* addr = mmap(nullptr, kPageSize, PROT_READ | PROT_WRITE, MAP_SHARED, file.fd(), 0);
    ASSERT_NE(MAP_FAILED, addr) << "mmap failed: errno=" << errno << " (" << strerror(errno)
                                << ")";

    static_cast<char*>(addr)[0] = 'x';
    InjectError("mapping", file.fd(), "EIO");

    errno = 0;
    ExpectFailsWithErrno(msync(addr, kPageSize, MS_SYNC), EIO);

    errno = 0;
    ExpectSucceeds(msync(addr, kPageSize, MS_SYNC));

    errno = 0;
    ExpectFailsWithErrno(RawSyncfs(file.fd()), EIO);

    EXPECT_EQ(0, munmap(addr, kPageSize));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
