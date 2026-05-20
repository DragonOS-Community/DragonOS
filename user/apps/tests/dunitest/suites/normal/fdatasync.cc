#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdlib.h>
#include <string.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <string>

#ifndef __NR_fdatasync
#if defined(__x86_64__)
#define __NR_fdatasync 75
#elif defined(__riscv) || defined(__loongarch64)
#define __NR_fdatasync 83
#else
#error "__NR_fdatasync is not defined for this architecture"
#endif
#endif

namespace {

long RawFdatasync(int fd) {
    return syscall(__NR_fdatasync, fd);
}

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/tmp/dunitest_fdatasync_XXXXXX";
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

    int close_fd() {
        const int fd = fd_;
        fd_ = -1;
        return close(fd);
    }

    bool write_test_data() const {
        constexpr char kData[] = "DragonOS fdatasync dunitest data\n";
        return write(fd_, kData, sizeof(kData) - 1) == static_cast<ssize_t>(sizeof(kData) - 1);
    }

  private:
    std::string path_;
    int fd_ = -1;
};

void ExpectFdatasyncErrno(int fd, int expected_errno) {
    errno = 0;
    EXPECT_EQ(-1, RawFdatasync(fd));
    EXPECT_EQ(expected_errno, errno) << "got errno=" << errno << " (" << strerror(errno) << ")";
}

}  // namespace

TEST(Fdatasync, TempFileSucceedsAndPreservesOffset) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    ASSERT_TRUE(file.write_test_data()) << "write failed: " << strerror(errno);
    ASSERT_EQ(3, lseek(file.fd(), 3, SEEK_SET));

    errno = 0;
    EXPECT_EQ(0, RawFdatasync(file.fd()));
    EXPECT_EQ(0, errno);
    EXPECT_EQ(3, lseek(file.fd(), 0, SEEK_CUR));
}

TEST(Fdatasync, TempDirSucceeds) {
    int dir_fd = open("/tmp", O_RDONLY | O_DIRECTORY);
    ASSERT_GE(dir_fd, 0) << "open directory failed: " << strerror(errno);

    errno = 0;
    EXPECT_EQ(0, RawFdatasync(dir_fd));
    EXPECT_EQ(0, errno);

    close(dir_fd);
}

TEST(Fdatasync, InvalidFdReturnsEbadf) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    const int fd = file.fd();
    ASSERT_EQ(0, file.close_fd());

    ExpectFdatasyncErrno(fd, EBADF);
}

TEST(Fdatasync, PipeReturnsEinval) {
    int pipefd[2] = {-1, -1};
    ASSERT_EQ(0, pipe(pipefd)) << "pipe failed: " << strerror(errno);

    ExpectFdatasyncErrno(pipefd[0], EINVAL);
    ExpectFdatasyncErrno(pipefd[1], EINVAL);

    close(pipefd[0]);
    close(pipefd[1]);
}

TEST(Fdatasync, SocketPairReturnsEinval) {
    int fds[2] = {-1, -1};
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_STREAM, 0, fds))
            << "socketpair failed: " << strerror(errno);

    ExpectFdatasyncErrno(fds[0], EINVAL);
    ExpectFdatasyncErrno(fds[1], EINVAL);

    close(fds[0]);
    close(fds[1]);
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
