#include <gtest/gtest.h>

#include <cerrno>
#include <cstdint>
#include <cstring>
#include <fcntl.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

namespace {
struct Fd {
    int value;
    explicit Fd(int fd) : value(fd) {}
    ~Fd() { if (value >= 0) close(value); }
    Fd(const Fd&) = delete;
    Fd& operator=(const Fd&) = delete;
};
void Error(ssize_t result, int expected) {
    const int actual = errno;
    EXPECT_EQ(-1, result);
    EXPECT_EQ(expected, actual) << strerror(actual);
}
class ProcWriteAdmission : public testing::TestWithParam<const char*> {
 protected:
    void SetUp() override {
        page_size_ = sysconf(_SC_PAGESIZE);
        ASSERT_GT(page_size_, 0);
        protected_ = mmap(nullptr, page_size_, PROT_NONE,
                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
        ASSERT_NE(MAP_FAILED, protected_) << strerror(errno);
        fd_ = open(GetParam(), O_WRONLY | O_CLOEXEC);
        ASSERT_GE(fd_, 0) << GetParam() << ": " << strerror(errno);
    }
    void TearDown() override {
        if (fd_ >= 0) close(fd_);
        if (protected_ != MAP_FAILED) munmap(protected_, page_size_);
    }
    int fd_ = -1;
    long page_size_ = 0;
    void* protected_ = MAP_FAILED;
};
TEST_P(ProcWriteAdmission, ScalarMissingCallbackRejectsBeforePayloadAccess) {
    const char byte = 'x';
    for (const void* input : {static_cast<const void*>(&byte),
                              static_cast<const void*>(nullptr),
                              static_cast<const void*>(protected_)}) {
        Error(syscall(SYS_write, fd_, input, 1UL), EIO);
    }
    Error(syscall(SYS_write, fd_, nullptr, 0UL), EIO);
    EXPECT_EQ(0, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(ProcWriteAdmission, NumericalRangeChecksStillPrecedeMissingCallback) {
    const uintptr_t outside = ~uintptr_t(0);
    Error(syscall(SYS_write, fd_, outside, 1UL), EFAULT);
    iovec vector{reinterpret_cast<void*>(outside), 1};
    Error(writev(fd_, &vector, 1), EFAULT);
    EXPECT_EQ(0, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(ProcWriteAdmission, NonemptyVectorRejectsBeforePayloadAccess) {
    char byte = 'x';
    for (void* input : {static_cast<void*>(&byte), static_cast<void*>(nullptr), protected_}) {
        iovec vector{input, 1};
        Error(writev(fd_, &vector, 1), EIO);
        Error(syscall(SYS_pwritev2, fd_, &vector, 1UL, -1L, 0UL, 0U), EIO);
    }
    iovec leading_empty[] = {{nullptr, 0}, {protected_, 1}};
    Error(writev(fd_, leading_empty, 2), EIO);
    EXPECT_EQ(0, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(ProcWriteAdmission, EmptyVectorCompletesWithoutCallingWrapper) {
    iovec empty[] = {{nullptr, 0}, {protected_, 0}};
    EXPECT_EQ(0, writev(fd_, nullptr, 0));
    EXPECT_EQ(0, writev(fd_, empty, 1));
    EXPECT_EQ(0, writev(fd_, empty, 2));
    EXPECT_EQ(0, syscall(SYS_pwritev2, fd_, empty, 2UL, -1L, 0UL, 0xffffffffU));
    EXPECT_EQ(0, lseek(fd_, 0, SEEK_CUR));
}
TEST_P(ProcWriteAdmission, VectorMetadataChecksStillPrecedeMissingCallback) {
    Error(syscall(SYS_writev, fd_, protected_, 1UL), EFAULT);
    Error(syscall(SYS_writev, fd_, protected_, 1025UL), EINVAL);
    iovec data{protected_, 1};
    // Linux's legacy write callback accepts HIPRI but no other RWF flags.
    Error(syscall(SYS_pwritev2, fd_, &data, 1UL, -1L, 0UL, 0x80000000U), EOPNOTSUPP);
}
TEST_P(ProcWriteAdmission, PositionalRestrictionPrecedesPayloadAndMetadataImport) {
    Error(syscall(SYS_pwrite64, fd_, protected_, 1UL, 0L), ESPIPE);
    Error(syscall(SYS_pwritev, fd_, protected_, 1UL, 0UL, 0UL), ESPIPE);
    Error(syscall(SYS_pwritev2, fd_, protected_, 1UL, 0L, 0UL, 0U), ESPIPE);
}
TEST_P(ProcWriteAdmission, ReadOnlyOpenStillRejectsWrites) {
    Fd read_only(open(GetParam(), O_RDONLY | O_CLOEXEC));
    ASSERT_GE(read_only.value, 0);
    iovec data{protected_, 1};
    Error(syscall(SYS_write, read_only.value, protected_, 1UL), EBADF);
    Error(writev(read_only.value, &data, 1), EBADF);
    // Keep this owner test independent of Linux's version-specific ordering
    // between a read-only fd and an invalid iovec descriptor. The Linux 6.6
    // import-before-mode contract is covered by write_capability_admission.
}
INSTANTIATE_TEST_SUITE_P(GlobalProc, ProcWriteAdmission,
                        testing::Values("/proc/version", "/proc/meminfo"));

TEST(ProcWriteAdmissionControl, DirectPidSeqFileStillHasNoWriteMethod) {
    Fd fd(open("/proc/self/stat", O_WRONLY | O_CLOEXEC));
    ASSERT_GE(fd.value, 0) << strerror(errno);
    Error(syscall(SYS_write, fd.value, nullptr, 1UL), EINVAL);
    iovec data{nullptr, 1};
    Error(writev(fd.value, &data, 1), EINVAL);
    Error(syscall(SYS_write, fd.value, nullptr, 0UL), EINVAL);
}

TEST(ProcWriteAdmissionControl, ExistingLegacyCallbackKeepsPayloadAndPositionContract) {
    Fd fd(open("/proc/self/oom_score_adj", O_RDWR | O_CLOEXEC));
    ASSERT_GE(fd.value, 0) << strerror(errno);
    char value[32];
    const ssize_t length = read(fd.value, value, sizeof(value));
    ASSERT_GT(length, 0);
    ASSERT_EQ(7, lseek(fd.value, 7, SEEK_SET));
    // Reapply the current value, as proc_write_position does; no policy change.
    EXPECT_EQ(length, write(fd.value, value, length));
    EXPECT_EQ(7, lseek(fd.value, 0, SEEK_CUR));
    Error(syscall(SYS_write, fd.value, nullptr, 1UL), EFAULT);
    EXPECT_EQ(7, lseek(fd.value, 0, SEEK_CUR));
    char current[32];
    const ssize_t readback = pread(fd.value, current, sizeof(current), 0);
    ASSERT_EQ(length, readback);
    EXPECT_EQ(0, memcmp(value, current, length));
}
}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
