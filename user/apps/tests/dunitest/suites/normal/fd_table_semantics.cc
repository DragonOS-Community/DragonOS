#include <gtest/gtest.h>

#include <cerrno>
#include <climits>
#include <cstdio>
#include <cstdlib>
#include <cstring>
#include <fcntl.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <unistd.h>

namespace {

constexpr char kNrOpenPath[] = "/proc/sys/fs/nr_open";

long read_nr_open() {
    int fd = open(kNrOpenPath, O_RDONLY);
    if (fd < 0) {
        return -1;
    }
    char buf[64] = {};
    const ssize_t n = read(fd, buf, sizeof(buf) - 1);
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    if (n <= 0) {
        return -1;
    }
    char* end = nullptr;
    const long value = strtol(buf, &end, 10);
    return end == buf ? -1 : value;
}

int write_nr_open(long value) {
    int fd = open(kNrOpenPath, O_WRONLY);
    if (fd < 0) {
        return -1;
    }
    char buf[64] = {};
    const int len = snprintf(buf, sizeof(buf), "%ld\n", value);
    const ssize_t n = write(fd, buf, static_cast<size_t>(len));
    const int saved_errno = errno;
    close(fd);
    errno = saved_errno;
    return n == len ? 0 : -1;
}

TEST(FdTableSemantics, NrOpenRejectsOutOfRangeValues) {
    const long original = read_nr_open();
    ASSERT_GT(original, 64) << strerror(errno);

    errno = 0;
    EXPECT_EQ(-1, write_nr_open(63));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(original, read_nr_open());

    errno = 0;
    EXPECT_EQ(-1, write_nr_open(INT_MAX));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(original, read_nr_open());
}

TEST(FdTableSemantics, SparseGrowthHonorsNrOpenAndExistingHighFdSurvivesSoftDrop) {
    const long original_nr_open = read_nr_open();
    ASSERT_GE(original_nr_open, 4096) << strerror(errno);

    const pid_t child = fork();
    ASSERT_GE(child, 0) << strerror(errno);
    if (child == 0) {
        int source = open("/dev/null", O_RDONLY);
        if (source < 0) {
            _exit(10);
        }

        struct rlimit limit = {};
        if (getrlimit(RLIMIT_NOFILE, &limit) != 0 || limit.rlim_max < 961) {
            _exit(11);
        }
        limit.rlim_cur = limit.rlim_max;
        if (setrlimit(RLIMIT_NOFILE, &limit) != 0) {
            _exit(12);
        }
        if (write_nr_open(960) != 0) {
            _exit(13);
        }

        if (dup2(source, 959) != 959) {
            write_nr_open(original_nr_open);
            _exit(14);
        }
        errno = 0;
        if (dup2(source, 960) != -1 || errno != EBADF) {
            write_nr_open(original_nr_open);
            _exit(15);
        }

        limit.rlim_cur = 64;
        limit.rlim_max = 960;
        if (setrlimit(RLIMIT_NOFILE, &limit) != 0) {
            write_nr_open(original_nr_open);
            _exit(16);
        }
        if (dup2(959, 959) != 959) {
            write_nr_open(original_nr_open);
            _exit(17);
        }
        errno = 0;
        if (dup3(959, 959, 0) != -1 || errno != EINVAL) {
            write_nr_open(original_nr_open);
            _exit(18);
        }

        // Lowering the global ceiling does not invalidate an already-open high
        // descriptor, but Linux dup_fd cannot allocate a clone layout above
        // the current nr_open and therefore rejects fork with EMFILE.
        if (write_nr_open(64) != 0) {
            write_nr_open(original_nr_open);
            _exit(19);
        }
        if (fcntl(959, F_GETFD) < 0) {
            write_nr_open(original_nr_open);
            _exit(20);
        }
        errno = 0;
        const pid_t grandchild = fork();
        if (grandchild != -1 || errno != EMFILE) {
            if (grandchild == 0) {
                _exit(21);
            }
            if (grandchild > 0) {
                int unexpected_status = 0;
                waitpid(grandchild, &unexpected_status, 0);
            }
            write_nr_open(original_nr_open);
            _exit(22);
        }

        close(959);
        close(source);
        if (write_nr_open(original_nr_open) != 0) {
            _exit(23);
        }
        _exit(0);
    }

    int status = 0;
    ASSERT_EQ(child, waitpid(child, &status, 0)) << strerror(errno);
    ASSERT_TRUE(WIFEXITED(status));
    EXPECT_EQ(0, WEXITSTATUS(status));
    EXPECT_EQ(original_nr_open, read_nr_open());
}

TEST(FdTableSemantics, FcntlDupFdStartsAtRequestedMinimum) {
    int source = open("/dev/null", O_RDONLY);
    ASSERT_GE(source, 0) << strerror(errno);

    struct rlimit limit = {};
    ASSERT_EQ(0, getrlimit(RLIMIT_NOFILE, &limit)) << strerror(errno);
    if (limit.rlim_cur <= 513) {
        close(source);
        GTEST_SKIP() << "RLIMIT_NOFILE is too small for the sparse-minimum check";
    }

    const int duplicate = fcntl(source, F_DUPFD_CLOEXEC, 513);
    ASSERT_GE(duplicate, 513) << strerror(errno);
    EXPECT_NE(0, fcntl(duplicate, F_GETFD) & FD_CLOEXEC);
    close(duplicate);
    close(source);
}

}  // namespace

int main(int argc, char** argv) {
    ::testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
