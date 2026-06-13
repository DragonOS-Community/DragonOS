#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/resource.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <unistd.h>

#include <string>

#ifndef __NR_fallocate
#if defined(__x86_64__)
#define __NR_fallocate 285
#elif defined(__riscv) || defined(__loongarch64)
#define __NR_fallocate 47
#else
#error "__NR_fallocate is not defined for this architecture"
#endif
#endif

#ifndef FALLOC_FL_KEEP_SIZE
#define FALLOC_FL_KEEP_SIZE 0x01
#endif

namespace {

long RawFallocate(int fd, int mode, off_t offset, off_t len) {
    return syscall(__NR_fallocate, fd, mode, offset, len);
}

class TempFile {
  public:
    TempFile() {
        char tmpl[] = "/tmp/dunitest_fallocate_XXXXXX";
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

    void unlink_path() {
        if (!path_.empty()) {
            ASSERT_EQ(0, unlink(path_.c_str())) << "unlink failed: errno=" << errno << " ("
                                                << strerror(errno) << ")";
            path_.clear();
        }
    }

    int reopen_readonly() const {
        return open(path_.c_str(), O_RDONLY);
    }

  private:
    std::string path_;
    int fd_ = -1;
};

off_t FileSize(int fd) {
    struct stat st {};
    if (fstat(fd, &st) != 0) {
        return -1;
    }
    return st.st_size;
}

class ScopedSignalIgnore {
  public:
    explicit ScopedSignalIgnore(int signal) : signal_(signal) {
        old_handler_ = ::signal(signal_, SIG_IGN);
    }

    ~ScopedSignalIgnore() {
        if (old_handler_ != SIG_ERR) {
            ::signal(signal_, old_handler_);
        }
    }

  private:
    int signal_;
    sighandler_t old_handler_;
};

class ScopedRlimit {
  public:
    ScopedRlimit(int resource, rlim_t soft_limit) : resource_(resource) {
        valid_ = getrlimit(resource_, &old_) == 0;
        if (!valid_) {
            return;
        }

        struct rlimit next = old_;
        next.rlim_cur = soft_limit;
        valid_ = setrlimit(resource_, &next) == 0;
    }

    ~ScopedRlimit() {
        if (valid_) {
            setrlimit(resource_, &old_);
        }
    }

    bool valid() const {
        return valid_;
    }

  private:
    int resource_;
    struct rlimit old_ {};
    bool valid_ = false;
};

}  // namespace

TEST(FallocateSemantics, DeletedOpenFileMode0ExtendsSize) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);
    file.unlink_path();

    errno = 0;
    ASSERT_EQ(0, RawFallocate(file.fd(), 0, 0, 123)) << "fallocate failed: errno=" << errno << " ("
                                                     << strerror(errno) << ")";
    EXPECT_EQ(123, FileSize(file.fd()));
}

TEST(FallocateSemantics, Mode0DoesNotShrink) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    ASSERT_EQ(0, RawFallocate(file.fd(), 0, 0, 4096)) << "initial fallocate failed: errno=" << errno
                                                      << " (" << strerror(errno) << ")";
    ASSERT_EQ(4096, FileSize(file.fd()));

    errno = 0;
    EXPECT_EQ(0, RawFallocate(file.fd(), 0, 0, 128)) << "smaller fallocate failed: errno=" << errno
                                                     << " (" << strerror(errno) << ")";
    EXPECT_EQ(4096, FileSize(file.fd()));
}

TEST(FallocateSemantics, UnsupportedKeepSizeRemainsUnsupported) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, RawFallocate(file.fd(), FALLOC_FL_KEEP_SIZE, 0, 4096));
    EXPECT_EQ(EOPNOTSUPP, errno) << "unexpected errno=" << errno << " (" << strerror(errno)
                                 << ")";
}

TEST(FallocateSemantics, ReadonlyFdReturnsEbadf) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    int readonly_fd = file.reopen_readonly();
    ASSERT_GE(readonly_fd, 0) << "reopen readonly failed: " << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, RawFallocate(readonly_fd, 0, 0, 123));
    EXPECT_EQ(EBADF, errno) << "unexpected errno=" << errno << " (" << strerror(errno) << ")";

    close(readonly_fd);
}

TEST(FallocateSemantics, RlimitFsizeBlocksGrowth) {
    TempFile file;
    ASSERT_TRUE(file.valid()) << "mkstemp failed: " << strerror(errno);

    ScopedSignalIgnore ignore_sigxfsz(SIGXFSZ);
    ScopedRlimit limit(RLIMIT_FSIZE, 64);
    ASSERT_TRUE(limit.valid()) << "setrlimit failed: " << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, RawFallocate(file.fd(), 0, 0, 128));
    EXPECT_EQ(EFBIG, errno) << "unexpected errno=" << errno << " (" << strerror(errno) << ")";
    EXPECT_EQ(0, FileSize(file.fd()));
}

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
