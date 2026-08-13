#include <errno.h>
#include <fcntl.h>
#include <gtest/gtest.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>

#ifndef SEEK_DATA
#define SEEK_DATA 3
#endif

#ifndef SEEK_HOLE
#define SEEK_HOLE 4
#endif

namespace {

class TempFile {
  public:
    TempFile() {
        char path[] = "/tmp/dragonos-lseek-XXXXXX";
        fd_ = mkstemp(path);
        if (fd_ >= 0) {
            unlink(path);
        }
    }

    ~TempFile() {
        if (fd_ >= 0) {
            close(fd_);
        }
    }

    TempFile(const TempFile &) = delete;
    TempFile &operator=(const TempFile &) = delete;

    int fd() const { return fd_; }
    bool valid() const { return fd_ >= 0; }

  private:
    int fd_ = -1;
};

void ExpectSeekFailureWithoutOffsetChange(int fd, off_t offset, int whence,
                                          int expected_errno, off_t original_offset) {
    errno = 0;
    EXPECT_EQ(-1, lseek(fd, offset, whence));
    EXPECT_EQ(expected_errno, errno);
    EXPECT_EQ(original_offset, lseek(fd, 0, SEEK_CUR));
}

TEST(LseekSeekDataHole, DenseRegularFileUsesGenericLinuxFallback) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    constexpr char kContents[] = "DEADBEEF";
    ASSERT_EQ(static_cast<ssize_t>(sizeof(kContents) - 1),
              write(file.fd(), kContents, sizeof(kContents) - 1));

    int duplicate = dup(file.fd());
    ASSERT_GE(duplicate, 0) << "dup failed: " << strerror(errno);

    EXPECT_EQ(0, lseek(file.fd(), 0, SEEK_DATA));
    EXPECT_EQ(0, lseek(duplicate, 0, SEEK_CUR));

    EXPECT_EQ(4, lseek(file.fd(), 4, SEEK_DATA));
    EXPECT_EQ(4, lseek(duplicate, 0, SEEK_CUR));

    EXPECT_EQ(8, lseek(file.fd(), 4, SEEK_HOLE));
    EXPECT_EQ(8, lseek(duplicate, 0, SEEK_CUR));

    ASSERT_EQ(2, lseek(file.fd(), 2, SEEK_SET));
    ExpectSeekFailureWithoutOffsetChange(file.fd(), -1, SEEK_DATA, ENXIO, 2);
    ExpectSeekFailureWithoutOffsetChange(file.fd(), -1, SEEK_HOLE, ENXIO, 2);
    ExpectSeekFailureWithoutOffsetChange(file.fd(), 8, SEEK_DATA, ENXIO, 2);
    ExpectSeekFailureWithoutOffsetChange(file.fd(), 8, SEEK_HOLE, ENXIO, 2);
    ExpectSeekFailureWithoutOffsetChange(file.fd(), 9, SEEK_DATA, ENXIO, 2);
    ExpectSeekFailureWithoutOffsetChange(file.fd(), 9, SEEK_HOLE, ENXIO, 2);

    EXPECT_EQ(0, close(duplicate));
}

TEST(LseekSeekDataHole, EmptyFileHasNeitherDataNorHoleAtEof) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    ExpectSeekFailureWithoutOffsetChange(file.fd(), 0, SEEK_DATA, ENXIO, 0);
    ExpectSeekFailureWithoutOffsetChange(file.fd(), 0, SEEK_HOLE, ENXIO, 0);
}

TEST(LseekSeekDataHole, PreservesLinuxErrorPrecedence) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    constexpr int kInvalidWhence = 99;
    errno = 0;
    EXPECT_EQ(-1, lseek(file.fd(), 0, kInvalidWhence));
    EXPECT_EQ(EINVAL, errno);

    int invalid_fd = dup(file.fd());
    ASSERT_GE(invalid_fd, 0) << "dup failed: " << strerror(errno);
    ASSERT_EQ(0, close(invalid_fd));

    errno = 0;
    EXPECT_EQ(-1, lseek(invalid_fd, 0, kInvalidWhence));
    EXPECT_EQ(EBADF, errno);
}

TEST(LseekSeekDataHole, DirectoryDoesNotUseRegularFileFallback) {
    int fd = open("/tmp", O_RDONLY | O_DIRECTORY);
    ASSERT_GE(fd, 0) << "open /tmp failed: " << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, lseek(fd, 0, SEEK_DATA));
    EXPECT_EQ(EINVAL, errno);

    errno = 0;
    EXPECT_EQ(-1, lseek(fd, 0, SEEK_HOLE));
    EXPECT_EQ(EINVAL, errno);

    EXPECT_EQ(0, close(fd));
}

} // namespace

int main(int argc, char **argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
