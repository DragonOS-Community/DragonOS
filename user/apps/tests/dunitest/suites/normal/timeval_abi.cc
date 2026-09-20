#include <gtest/gtest.h>

#include <cerrno>
#include <climits>
#include <cstdlib>
#include <cstring>
#include <sys/select.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/time.h>
#include <time.h>
#include <unistd.h>

namespace {

static_assert(sizeof(timeval::tv_usec) == sizeof(long));
static_assert(sizeof(long) == 8);
constexpr long kHighMicroseconds = 1L << 32;

class FdGuard {
  public:
    explicit FdGuard(int fd) : fd_(fd) {}
    FdGuard(const FdGuard&) = delete;
    FdGuard& operator=(const FdGuard&) = delete;
    ~FdGuard() { if (fd_ >= 0) close(fd_); }
  private:
    int fd_;
};

void ExpectNormalized(const timeval& value) {
    EXPECT_GE(value.tv_usec, 0);
    EXPECT_LT(value.tv_usec, 1000000);
}

void ExpectZero(const itimerval& value) {
    EXPECT_EQ(value.it_interval.tv_sec, 0);
    EXPECT_EQ(value.it_interval.tv_usec, 0);
    EXPECT_EQ(value.it_value.tv_sec, 0);
    EXPECT_EQ(value.it_value.tv_usec, 0);
}

int Compare(const timeval& lhs, const timeval& rhs) {
    if (lhs.tv_sec != rhs.tv_sec) return lhs.tv_sec < rhs.tv_sec ? -1 : 1;
    if (lhs.tv_usec != rhs.tv_usec) return lhs.tv_usec < rhs.tv_usec ? -1 : 1;
    return 0;
}

TEST(TimevalAbi, GettimeofdayWritesCompleteNativeFields) {
    for (int i = 0; i < 64; ++i) {
        timespec before {}, after {};
        timeval value;
        memset(&value, 0xa5, sizeof(value));
        ASSERT_EQ(clock_gettime(CLOCK_REALTIME, &before), 0);
        ASSERT_EQ(syscall(SYS_gettimeofday, &value, nullptr), 0);
        ASSERT_EQ(clock_gettime(CLOCK_REALTIME, &after), 0);
        ExpectNormalized(value);
        EXPECT_GE(Compare(value, {before.tv_sec, before.tv_nsec / 1000}), 0);
        EXPECT_LE(Compare(value, {after.tv_sec, after.tv_nsec / 1000}), 0);
    }
}

TEST(TimevalAbi, GetitimerWritesCompleteNativeFields) {
    for (int which : {ITIMER_REAL, ITIMER_VIRTUAL, ITIMER_PROF}) {
        SCOPED_TRACE(which);
        itimerval zero {}, value;
        ASSERT_EQ(syscall(SYS_setitimer, which, &zero, nullptr), 0);
        memset(&value, 0xa5, sizeof(value));
        ASSERT_EQ(syscall(SYS_getitimer, which, &value), 0);
        ExpectZero(value);
    }
}

TEST(TimevalAbi, SetitimerWritesCompleteOldValue) {
    for (int which : {ITIMER_REAL, ITIMER_VIRTUAL, ITIMER_PROF}) {
        SCOPED_TRACE(which);
        itimerval zero {}, old_value;
        ASSERT_EQ(syscall(SYS_setitimer, which, &zero, nullptr), 0);
        memset(&old_value, 0xa5, sizeof(old_value));
        ASSERT_EQ(syscall(SYS_setitimer, which, &zero, &old_value), 0);
        ExpectZero(old_value);
    }
}

TEST(TimevalAbi, SetitimerRejectsHighBitsBeforeWritingOldValue) {
    for (int which : {ITIMER_REAL, ITIMER_VIRTUAL, ITIMER_PROF}) {
        itimerval zero {};
        ASSERT_EQ(syscall(SYS_setitimer, which, &zero, nullptr), 0);
        for (bool interval : {false, true}) {
            for (long invalid : {kHighMicroseconds, -kHighMicroseconds}) {
                SCOPED_TRACE(testing::Message() << which << "/" << interval << "/" << invalid);
                itimerval input {}, old_value, untouched;
                (interval ? input.it_interval : input.it_value).tv_usec = invalid;
                memset(&old_value, 0xa5, sizeof(old_value));
                memcpy(&untouched, &old_value, sizeof(old_value));
                errno = 0;
                EXPECT_EQ(syscall(SYS_setitimer, which, &input, &old_value), -1);
                EXPECT_EQ(errno, EINVAL);
                EXPECT_EQ(memcmp(&old_value, &untouched, sizeof(old_value)), 0);
                itimerval current {};
                ASSERT_EQ(syscall(SYS_getitimer, which, &current), 0);
                ExpectZero(current);
            }
        }
    }
}

#ifdef SYS_select
void ExpectReadySelect(timeval timeout, long minimum_remaining_seconds) {
    int fds[2];
    ASSERT_EQ(pipe(fds), 0);
    FdGuard reader(fds[0]), writer(fds[1]);
    ASSERT_LT(fds[0], FD_SETSIZE);
    ASSERT_EQ(write(fds[1], "x", 1), 1);
    fd_set readfds;
    FD_ZERO(&readfds);
    FD_SET(fds[0], &readfds);
    ASSERT_EQ(syscall(SYS_select, fds[0] + 1, &readfds, nullptr, nullptr, &timeout), 1)
        << "errno=" << errno;
    EXPECT_TRUE(FD_ISSET(fds[0], &readfds));
    ExpectNormalized(timeout);
    EXPECT_GT(timeout.tv_sec, minimum_remaining_seconds);
}

TEST(TimevalAbi, SelectPreservesPositiveMicrosecondHighBits) {
    ExpectReadySelect({0, kHighMicroseconds}, 4000);
}

TEST(TimevalAbi, SelectRejectsNegativeMicrosecondHighBits) {
    // A ready pipe prevents a broken implementation from sleeping indefinitely.
    int fds[2];
    ASSERT_EQ(pipe(fds), 0);
    FdGuard reader(fds[0]), writer(fds[1]);
    ASSERT_LT(fds[0], FD_SETSIZE);
    ASSERT_EQ(write(fds[1], "x", 1), 1);
    fd_set readfds;
    FD_ZERO(&readfds);
    FD_SET(fds[0], &readfds);
    timeval timeout {0, -kHighMicroseconds};
    errno = 0;
    EXPECT_EQ(syscall(SYS_select, fds[0] + 1, &readfds, nullptr, nullptr, &timeout), -1);
    EXPECT_EQ(errno, EINVAL);
}

TEST(TimevalAbi, SelectHandlesLargePositiveTimeouts) {
    ExpectReadySelect({0, LONG_MAX}, 1000000000L);
    ExpectReadySelect({LONG_MAX, 0}, 1000000000L);
}
#endif

#ifdef SYS_utimes
TEST(TimevalAbi, UtimesValidatesCompleteMicrosecondFields) {
    char path[] = "/tmp/timeval_abi.XXXXXX";
    int fd = mkstemp(path);
    ASSERT_GE(fd, 0);
    FdGuard file(fd);
    // Keep cleanup local even when a fatal assertion returns from the test.
    struct PathGuard {
        const char* path;
        ~PathGuard() { unlink(path); }
    } cleanup {path};
    timeval valid[2] {{1700000000, 123456}, {1700000001, 654321}};
    ASSERT_EQ(syscall(SYS_utimes, path, valid), 0);
    struct stat before {};
    ASSERT_EQ(fstat(fd, &before), 0);
    EXPECT_EQ(before.st_atim.tv_sec, valid[0].tv_sec);
    EXPECT_EQ(before.st_atim.tv_nsec, valid[0].tv_usec * 1000);
    EXPECT_EQ(before.st_mtim.tv_sec, valid[1].tv_sec);
    EXPECT_EQ(before.st_mtim.tv_nsec, valid[1].tv_usec * 1000);
    for (int field : {0, 1}) {
        for (long invalid : {kHighMicroseconds, -kHighMicroseconds}) {
            SCOPED_TRACE(testing::Message() << field << "/" << invalid);
            timeval input[2] {valid[0], valid[1]};
            input[field].tv_usec = invalid;
            errno = 0;
            EXPECT_EQ(syscall(SYS_utimes, path, input), -1);
            EXPECT_EQ(errno, EINVAL);
            struct stat after {};
            ASSERT_EQ(fstat(fd, &after), 0);
            EXPECT_EQ(after.st_atim.tv_sec, before.st_atim.tv_sec);
            EXPECT_EQ(after.st_atim.tv_nsec, before.st_atim.tv_nsec);
            EXPECT_EQ(after.st_mtim.tv_sec, before.st_mtim.tv_sec);
            EXPECT_EQ(after.st_mtim.tv_nsec, before.st_mtim.tv_nsec);
            EXPECT_EQ(after.st_ctim.tv_sec, before.st_ctim.tv_sec);
            EXPECT_EQ(after.st_ctim.tv_nsec, before.st_ctim.tv_nsec);
        }
    }
}
#endif

} // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
