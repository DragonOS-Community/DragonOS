#ifndef _GNU_SOURCE
#define _GNU_SOURCE
#endif

#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdio.h>
#include <string.h>
#include <sys/syscall.h>
#include <sys/types.h>
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

constexpr char kTestData[] = "DragonOS sync_file_range dunitest data\n";

long RawSyncFileRange(int fd, off_t offset, off_t nbytes, unsigned int flags) {
    return syscall(__NR_sync_file_range, fd, offset, nbytes, flags);
}

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/tmp/dunitest_sync_file_range_XXXXXX";
        fd_ = mkstemp(tmpl);
        if (fd_ >= 0) {
            path_ = tmpl;
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
        return fd_ >= 0;
    }

    int fd() const {
        return fd_;
    }

    const char* path() const {
        return path_.c_str();
    }

    bool write_test_data() const {
        return write(fd_, kTestData, sizeof(kTestData) - 1) == static_cast<ssize_t>(sizeof(kTestData) - 1);
    }

  private:
    std::string path_;
    int fd_ = -1;
};

void ExpectSyncFileRangeErrno(int fd, off_t offset, off_t nbytes, unsigned int flags,
                              int expected_errno) {
    errno = 0;
    EXPECT_EQ(-1, RawSyncFileRange(fd, offset, nbytes, flags));
    EXPECT_EQ(expected_errno, errno) << "got errno=" << errno << " (" << strerror(errno) << ")";
}

}  // namespace

TEST(SyncFileRange, RegularFileAcceptedFlagsAndPreservesOffset) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    ASSERT_TRUE(file.write_test_data()) << "write failed: " << strerror(errno);

    const unsigned int flags[] = {
        0,
        SYNC_FILE_RANGE_WRITE,
        SYNC_FILE_RANGE_WAIT_BEFORE,
        SYNC_FILE_RANGE_WAIT_AFTER,
        SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE,
        SYNC_FILE_RANGE_WAIT_BEFORE | SYNC_FILE_RANGE_WRITE | SYNC_FILE_RANGE_WAIT_AFTER,
    };

    for (unsigned int flag : flags) {
        errno = 0;
        EXPECT_EQ(0, RawSyncFileRange(file.fd(), 0, 0, flag)) << "flags=" << flag;
        EXPECT_EQ(0, errno);
    }

    ASSERT_EQ(1, lseek(file.fd(), 1, SEEK_SET));

    errno = 0;
    EXPECT_EQ(0, RawSyncFileRange(file.fd(), 1, 8, SYNC_FILE_RANGE_WRITE));
    EXPECT_EQ(0, errno);
    EXPECT_EQ(1, lseek(file.fd(), 0, SEEK_CUR));
}

TEST(SyncFileRange, ReadonlyRegularFileCanStartWriteback) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    ASSERT_TRUE(file.write_test_data()) << "write failed: " << strerror(errno);

    int ro_fd = open(file.path(), O_RDONLY);
    ASSERT_GE(ro_fd, 0) << "open readonly failed: " << strerror(errno);

    errno = 0;
    EXPECT_EQ(0, RawSyncFileRange(ro_fd, 0, 0, SYNC_FILE_RANGE_WRITE));
    EXPECT_EQ(0, errno);

    close(ro_fd);
}

TEST(SyncFileRange, DirectoryFdIsAccepted) {
    int dir_fd = open("/tmp", O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dir_fd, 0) << "open directory failed: " << strerror(errno);

    errno = 0;
    EXPECT_EQ(0, RawSyncFileRange(dir_fd, 0, 0, SYNC_FILE_RANGE_WRITE));
    EXPECT_EQ(0, errno);

    close(dir_fd);
}

TEST(SyncFileRange, LinuxCompatibleErrnos) {
    ExpectSyncFileRangeErrno(-1, 0, 0, SYNC_FILE_RANGE_WRITE, EBADF);
    ExpectSyncFileRangeErrno(-1, -1, 1, SYNC_FILE_RANGE_WRITE, EBADF);
    ExpectSyncFileRangeErrno(-1, 0, 0, 0x8, EBADF);

    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    ExpectSyncFileRangeErrno(file.fd(), 0, 0, 0x8, EINVAL);
    ExpectSyncFileRangeErrno(file.fd(), -1, 1, SYNC_FILE_RANGE_WRITE, EINVAL);
    ExpectSyncFileRangeErrno(file.fd(), 0, -1, SYNC_FILE_RANGE_WRITE, EINVAL);
    ExpectSyncFileRangeErrno(file.fd(), static_cast<off_t>(1) << 62, static_cast<off_t>(1) << 62,
                             SYNC_FILE_RANGE_WRITE, EINVAL);

#ifdef O_PATH
    int path_fd = open(file.path(), O_PATH);
    ASSERT_GE(path_fd, 0) << "open O_PATH failed: " << strerror(errno);
    ExpectSyncFileRangeErrno(path_fd, 0, 0, SYNC_FILE_RANGE_WRITE, EBADF);
    ExpectSyncFileRangeErrno(path_fd, -1, 1, SYNC_FILE_RANGE_WRITE, EBADF);
    ExpectSyncFileRangeErrno(path_fd, 0, 0, 0x8, EBADF);
    close(path_fd);
#endif

    int pipefd[2];
    ASSERT_EQ(0, pipe(pipefd)) << "pipe failed: " << strerror(errno);
    ExpectSyncFileRangeErrno(pipefd[0], -1, 1, SYNC_FILE_RANGE_WRITE, EINVAL);
    ExpectSyncFileRangeErrno(pipefd[0], 0, 0, 0x8, EINVAL);
    ExpectSyncFileRangeErrno(pipefd[0], 0, 0, SYNC_FILE_RANGE_WRITE, ESPIPE);
    close(pipefd[0]);
    close(pipefd[1]);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
