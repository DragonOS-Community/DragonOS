#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <signal.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/epoll.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/stat.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <unistd.h>

#include <string>

namespace {
constexpr unsigned kHipri = 0x01;
constexpr unsigned kDsync = 0x02;
constexpr unsigned kSync = 0x04;
constexpr unsigned kNowait = 0x08;
constexpr unsigned kAppend = 0x10;
constexpr unsigned kUnknown = 0x80000000U;

// All supported native 64-bit ABIs have six arguments. The entire offset is
// in pos_l, pos_h is ignored, and only the low 32 bits of rwf_t are flags.
ssize_t Pwritev2(int fd, const iovec* vectors, size_t count, int64_t offset,
                unsigned long flags, unsigned long high = 0) {
    return syscall(SYS_pwritev2, fd, vectors, count, static_cast<uint64_t>(offset), high,
                   flags);
}

class Pwritev2Flags : public testing::Test {
 protected:
    void SetUp() override {
        char name[] = "/tmp/pwritev2-flags-XXXXXX";
        fd_ = mkstemp(name);
        ASSERT_GE(fd_, 0) << strerror(errno);
        path_ = name;
        ASSERT_EQ(6, write(fd_, "abcdef", 6));
        ASSERT_EQ(2, lseek(fd_, 2, SEEK_SET));
        bad_ = mmap(nullptr, 4096, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, bad_);
    }
    void TearDown() override {
        if (bad_ != MAP_FAILED) munmap(bad_, 4096);
        if (fd_ >= 0) close(fd_);
        if (!path_.empty()) unlink(path_.c_str());
    }
    void Error(int error, int fd, const iovec* vectors, size_t count,
               int64_t offset, unsigned long flags) {
        errno = 0;
        EXPECT_EQ(-1, Pwritev2(fd, vectors, count, offset, flags));
        EXPECT_EQ(error, errno) << strerror(errno);
    }
    int fd_ = -1;
    std::string path_;
    void* bad_ = MAP_FAILED;
};

TEST_F(Pwritev2Flags, NegativeOffsetPrecedesDescriptorImportAndFlags) {
    Error(EINVAL, -1, static_cast<iovec*>(bad_), 1, -2, kUnknown);
}

TEST_F(Pwritev2Flags, DescriptorLookupPrecedesImportAndFlags) {
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
        Error(EBADF, -1, static_cast<iovec*>(bad_), 1, offset, kUnknown);
    }
}

TEST_F(Pwritev2Flags, PathOnlyDescriptorPrecedesImportAndFlags) {
    int fd = open(path_.c_str(), O_PATH | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
        Error(EBADF, fd, static_cast<iovec*>(bad_), 1, offset, kUnknown);
    }
    close(fd);
}

TEST_F(Pwritev2Flags, PositionalCapabilityPrecedesImportAndFlags) {
    int pair[2];
    ASSERT_EQ(0, pipe2(pair, O_NONBLOCK | O_CLOEXEC));
    Error(ESPIPE, pair[1], static_cast<iovec*>(bad_), 1, 0, kUnknown);
    Error(ESPIPE, pair[1], nullptr, 0, 0, kUnknown);
    EXPECT_EQ(0, Pwritev2(pair[1], nullptr, 0, -1, kUnknown));
    close(pair[0]);
    close(pair[1]);
}

TEST_F(Pwritev2Flags, ImportPrecedesWriteModeAndFlagValidation) {
    int readonly = open(path_.c_str(), O_RDONLY | O_CLOEXEC);
    ASSERT_GE(readonly, 0);
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
        Error(EFAULT, fd_, static_cast<iovec*>(bad_), 1, offset, kUnknown);
        Error(EINVAL, fd_, nullptr, 1025, offset, kUnknown);
        // Linux 6.6 imports before FMODE_WRITE; Linux 6.8 changed this order.
        Error(EFAULT, readonly, static_cast<iovec*>(bad_), 1, offset, kUnknown);
    }
    close(readonly);
}

TEST_F(Pwritev2Flags, WriteModePrecedesEmptyVectorAndFlags) {
    int readonly = open(path_.c_str(), O_RDONLY | O_CLOEXEC);
    ASSERT_GE(readonly, 0);
    char byte = 'x';
    iovec vector = {&byte, 1};
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
        Error(EBADF, readonly, &vector, 1, offset, kUnknown);
        Error(EBADF, readonly, nullptr, 0, offset, kUnknown);
    }
    close(readonly);
}

TEST_F(Pwritev2Flags, WriteCapabilityPrecedesEmptyVectorAndFlags) {
    int fd = epoll_create1(EPOLL_CLOEXEC);
    ASSERT_GE(fd, 0);
    char byte = 'x';
    iovec vector = {&byte, 1};
    Error(EFAULT, fd, static_cast<iovec*>(bad_), 1, -1, kUnknown);
    Error(EINVAL, fd, &vector, 1, -1, kUnknown);
    Error(EINVAL, fd, nullptr, 0, -1, kUnknown);
    Error(ESPIPE, fd, static_cast<iovec*>(bad_), 1, 0, kUnknown);
    close(fd);
}

TEST_F(Pwritev2Flags, EmptyVectorsSkipRangeAndFlagValidation) {
    iovec empty = {bad_, 0};
    for (int64_t offset : {int64_t(-1), int64_t(0), INT64_MAX}) {
        EXPECT_EQ(0, Pwritev2(fd_, nullptr, 0, offset, kUnknown | kNowait | kAppend));
        EXPECT_EQ(0, Pwritev2(fd_, &empty, 1, offset, kUnknown | kNowait | kAppend));
        Error(EFAULT, fd_, static_cast<iovec*>(bad_), 1, offset, kUnknown);
    }
    EXPECT_EQ(2, lseek(fd_, 0, SEEK_CUR));
}

TEST_F(Pwritev2Flags, OffsetRangePrecedesFlags) {
    char byte = 'x';
    iovec vector = {&byte, 1};
    Error(EINVAL, fd_, &vector, 1, INT64_MAX, kUnknown);
    EXPECT_EQ(2, lseek(fd_, 0, SEEK_CUR));
}

TEST_F(Pwritev2Flags, UnknownFlagsPrecedePayloadFaultAndPreserveFile) {
    iovec vector = {bad_, 1};
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
        Error(EOPNOTSUPP, fd_, &vector, 1, offset, kUnknown);
        EXPECT_EQ(2, lseek(fd_, 0, SEEK_CUR));
    }
    char bytes[6];
    ASSERT_EQ(6, pread(fd_, bytes, sizeof(bytes), 0));
    EXPECT_EQ("abcdef", std::string(bytes, sizeof(bytes)));
}

TEST_F(Pwritev2Flags, NativeOffsetUsesFullLowWordAndIgnoresHighWord) {
    char byte = 'X';
    iovec vector = {&byte, 1};
    ASSERT_EQ(1, Pwritev2(fd_, &vector, 1, 1, 0, 1));
    char result = 0;
    ASSERT_EQ(1, pread(fd_, &result, 1, 1));
    EXPECT_EQ('X', result);
    const int64_t large = (int64_t(1) << 32) + 1;
    ASSERT_EQ(1, Pwritev2(fd_, &vector, 1, large, 0));
    struct stat metadata{};
    ASSERT_EQ(0, fstat(fd_, &metadata));
    EXPECT_EQ(large + 1, metadata.st_size);
    ASSERT_EQ(1, pread(fd_, &result, 1, large));
    EXPECT_EQ('X', result);
    ASSERT_EQ(0, ftruncate(fd_, 6));
    ASSERT_EQ(1, Pwritev2(fd_, &vector, 1, -1, 0, 0x12345678));
    EXPECT_EQ(3, lseek(fd_, 0, SEEK_CUR));
}

TEST_F(Pwritev2Flags, NativeFlagsUseSixthArgumentAndTruncateToRwfWidth) {
    char byte = 'X';
    iovec vector = {&byte, 1};
    ASSERT_EQ(1, Pwritev2(fd_, &vector, 1, 0, 1UL << 32, kUnknown));
    EXPECT_EQ(2, lseek(fd_, 0, SEEK_CUR));
    Error(EOPNOTSUPP, fd_, &vector, 1, 0, (1UL << 32) | kUnknown);
}

TEST_F(Pwritev2Flags, AppendUsesEofAndOnlyCurrentPositionPathPublishesPosition) {
    char bytes[] = "XY";
    iovec vector = {bytes, 2};
    const int before = fcntl(fd_, F_GETFL);
    ASSERT_GE(before, 0);
    ASSERT_EQ(2, Pwritev2(fd_, &vector, 1, 0, kAppend));
    EXPECT_EQ(2, lseek(fd_, 0, SEEK_CUR));
    ASSERT_EQ(2, Pwritev2(fd_, &vector, 1, -1, kAppend));
    EXPECT_EQ(10, lseek(fd_, 0, SEEK_CUR));
    EXPECT_EQ(before, fcntl(fd_, F_GETFL));
    char result[10];
    ASSERT_EQ(10, pread(fd_, result, sizeof(result), 0));
    EXPECT_EQ("abcdefXYXY", std::string(result, sizeof(result)));
}

TEST_F(Pwritev2Flags, ExistingAppendFlagSurvivesPerOperationFlags) {
    int fd = open(path_.c_str(), O_WRONLY | O_APPEND | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    const int before = fcntl(fd, F_GETFL);
    char byte = 'X';
    iovec vector = {&byte, 1};
    ASSERT_EQ(1, Pwritev2(fd, &vector, 1, 0, kHipri));
    EXPECT_EQ(0, lseek(fd, 0, SEEK_CUR));
    EXPECT_EQ(before, fcntl(fd, F_GETFL));
    char result = 0;
    ASSERT_EQ(1, pread(fd_, &result, 1, 6));
    EXPECT_EQ('X', result);
    close(fd);
}

TEST_F(Pwritev2Flags, LegacyEventfdAcceptsOnlyHipriForNonemptyWrites) {
    int fd = eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC);
    ASSERT_GE(fd, 0);
    uint64_t value = 1;
    iovec vector = {&value, sizeof(value)};
    ASSERT_EQ(8, Pwritev2(fd, &vector, 1, -1, kHipri));
    for (unsigned flags : {kDsync, kSync, kAppend, kNowait, kUnknown}) {
        Error(EOPNOTSUPP, fd, &vector, 1, -1, flags);
        EXPECT_EQ(0, Pwritev2(fd, nullptr, 0, -1, flags));
    }
    uint64_t result = 0;
    ASSERT_EQ(8, read(fd, &result, sizeof(result)));
    EXPECT_EQ(1U, result);
    close(fd);
}

TEST_F(Pwritev2Flags, LegacyFullRejectsHintsBeforeBackendError) {
    int fd = open("/dev/full", O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    char byte = 'x';
    iovec vector = {&byte, 1};
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
        for (unsigned flags : {kDsync, kSync, kAppend, kNowait, kUnknown}) {
            Error(EOPNOTSUPP, fd, &vector, 1, offset, flags);
            EXPECT_EQ(0, Pwritev2(fd, nullptr, 0, offset, flags));
        }
        Error(ENOSPC, fd, &vector, 1, offset, kHipri);
    }
    close(fd);
}

TEST_F(Pwritev2Flags, NullIteratorAcceptsSyncAppendAndHipri) {
    int fd = open("/dev/null", O_WRONLY | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    char byte = 'x';
    iovec vector = {&byte, 1};
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
        for (unsigned flags : {kHipri, kDsync, kSync, kAppend,
                               kHipri | kDsync | kSync | kAppend}) {
            EXPECT_EQ(1, Pwritev2(fd, &vector, 1, offset, flags));
        }
    }
    close(fd);
}

TEST_F(Pwritev2Flags, ProcLegacyAndSysctlIteratorUseIndependentWriteCapabilities) {
    int legacy = open("/proc/self/oom_score_adj", O_RDWR | O_CLOEXEC);
    ASSERT_GE(legacy, 0) << strerror(errno);
    int iterator = open("/proc/sys/kernel/pid_max", O_RDWR | O_CLOEXEC);
    if (iterator < 0) {
        close(legacy);
        FAIL() << "isolated root guest requires writable sysctl: " << strerror(errno);
        return;
    }
    iovec vector = {bad_, 1};
    for (int64_t offset : {int64_t(-1), int64_t(0)}) {
        // Neither call changes a setting: the legacy path rejects the hint
        // before fetching payload; the iterator reaches its user-copy fault.
        Error(EOPNOTSUPP, legacy, &vector, 1, offset, kDsync);
        Error(EFAULT, iterator, &vector, 1, offset, kDsync);
    }
    close(iterator);
    close(legacy);
}

TEST_F(Pwritev2Flags, NowaitOnFullPipeCannotBlockOrChangeSharedFlags) {
    int pair[2];
    ASSERT_EQ(0, pipe2(pair, O_NONBLOCK | O_CLOEXEC));
    char bytes[4096] = {};
    while (write(pair[1], bytes, sizeof(bytes)) > 0) {}
    ASSERT_EQ(EAGAIN, errno);
    const int flags = fcntl(pair[1], F_GETFL) & ~O_NONBLOCK;
    ASSERT_EQ(0, fcntl(pair[1], F_SETFL, flags));
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        alarm(2);
        iovec vector = {bytes, 1};
        errno = 0;
        const ssize_t result = Pwritev2(pair[1], &vector, 1, -1, kNowait);
        const int error = errno;
        const int after = fcntl(pair[1], F_GETFL);
        _exit(result == -1 && (error == EAGAIN || error == EOPNOTSUPP) &&
                      after >= 0 && !(after & O_NONBLOCK) ? 0 : 1);
    }
    int status = 0;
    pid_t result;
    do { result = waitpid(child, &status, 0); } while (result < 0 && errno == EINTR);
    EXPECT_EQ(flags, fcntl(pair[1], F_GETFL));
    close(pair[0]);
    close(pair[1]);
    ASSERT_EQ(child, result);
    ASSERT_TRUE(WIFEXITED(status)) << "NOWAIT blocked until alarm, status=" << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
