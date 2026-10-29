#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <stdint.h>
#include <signal.h>
#include <sys/resource.h>
#include <sys/wait.h>
#include <stdio.h>
#include <string.h>
#include <sys/mman.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <unistd.h>

#include <string>

namespace {
enum class Operation { Write, Writev, Pwrite, Pwritev, Pwritev2, Pwritev2Current };
ssize_t Write(Operation op, int fd, const std::string& value, off_t offset) {
    iovec vectors[] = {{const_cast<char*>(value.data()), value.size()},
                       {const_cast<char*>(value.data()), value.size()}};
    switch (op) {
        case Operation::Write: return write(fd, value.data(), value.size());
        case Operation::Writev: return writev(fd, vectors, 2);
        case Operation::Pwrite: return pwrite(fd, value.data(), value.size(), offset);
        case Operation::Pwritev: return pwritev(fd, vectors, 2, offset);
        case Operation::Pwritev2:
            return syscall(SYS_pwritev2, fd, vectors, 2, offset, 0UL, 0U);
        case Operation::Pwritev2Current:
            return syscall(SYS_pwritev2, fd, vectors, 2, -1L, 0UL, 0U);
    }
    return -1;
}
class ProcWritePosition : public testing::TestWithParam<Operation> {
 protected:
    void SetUp() override {
        const int fd = open("/proc/self/oom_score_adj", O_RDONLY | O_CLOEXEC);
        ASSERT_GE(fd, 0);
        char value[32];
        ssize_t length = read(fd, value, sizeof(value));
        close(fd);
        ASSERT_GT(length, 0);
        value_.assign(value, length);
    }
    void Check(bool append) {
        int fd = open("/proc/self/oom_score_adj", O_WRONLY | O_CLOEXEC | (append ? O_APPEND : 0));
        ASSERT_GE(fd, 0) << strerror(errno);
        ASSERT_EQ(3, lseek(fd, 3, SEEK_SET));
        const bool scalar = GetParam() == Operation::Write || GetParam() == Operation::Pwrite;
        EXPECT_EQ(static_cast<ssize_t>(value_.size() * (scalar ? 1 : 2)), Write(GetParam(), fd, value_, 7));
        EXPECT_EQ(3, lseek(fd, 0, SEEK_CUR));
        // The callback ignores the supplied position, but the file remains seekable.
        EXPECT_EQ(11, lseek(fd, 11, SEEK_SET));
        close(fd);
        int readback = open("/proc/self/oom_score_adj", O_RDONLY | O_CLOEXEC);
        ASSERT_GE(readback, 0);
        char value[32];
        ssize_t length = read(readback, value, sizeof(value));
        close(readback);
        ASSERT_EQ(static_cast<ssize_t>(value_.size()), length);
        EXPECT_EQ(0, memcmp(value, value_.data(), value_.size()));
    }
    std::string value_;
};
TEST_P(ProcWritePosition, CallbackLeavesTheOpenFilePositionUnchanged) { Check(false); }
TEST_P(ProcWritePosition, AppendDoesNotReplaceTheProcOwnerPositionContract) { Check(true); }
TEST_P(ProcWritePosition, FileSizeLimitDoesNotApplyToTheProcOwner) {
    const pid_t child = fork();
    ASSERT_GE(child, 0);
    if (child == 0) {
        signal(SIGXFSZ, SIG_IGN);
        rlimit limit{0, 0};
        if (setrlimit(RLIMIT_FSIZE, &limit) != 0) _exit(10);
        const int fd = open("/proc/self/oom_score_adj", O_WRONLY | O_CLOEXEC);
        if (fd < 0 || lseek(fd, 3, SEEK_SET) != 3) _exit(11);
        const bool scalar = GetParam() == Operation::Write || GetParam() == Operation::Pwrite;
        const ssize_t expected = value_.size() * (scalar ? 1 : 2);
        errno = 0;
        const ssize_t result = Write(GetParam(), fd, value_, 7);
        const int error = errno;
        const off_t position = lseek(fd, 0, SEEK_CUR);
        close(fd);
        if (result != expected || position != 3) {
            dprintf(STDERR_FILENO, "PROC_RLIMIT operation=%d result=%zd errno=%d position=%lld expected=%zd\n",
                    static_cast<int>(GetParam()), result, error, static_cast<long long>(position), expected);
            _exit(12);
        }
        _exit(0);
    }
    int status = 0;
    pid_t result;
    do { result = waitpid(child, &status, 0); } while (result < 0 && errno == EINTR);
    ASSERT_EQ(child, result);
    ASSERT_TRUE(WIFEXITED(status)) << status;
    EXPECT_EQ(0, WEXITSTATUS(status));
}
INSTANTIATE_TEST_SUITE_P(Operations, ProcWritePosition,
                        testing::Values(Operation::Write, Operation::Writev, Operation::Pwrite,
                                        Operation::Pwritev, Operation::Pwritev2, Operation::Pwritev2Current));

TEST(ProcWritePositionPolicy, NumericSysctlNonzeroAppendUsesOwnerPosition) {
    const char* path = "/proc/sys/kernel/pid_max";
    char original[64] = {}, current[64] = {};
    int reader = open(path, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(reader, 0) << strerror(errno);
    const ssize_t length = read(reader, original, sizeof(original));
    close(reader);
    ASSERT_GT(length, 0);
    for (bool vector : {false, true}) {
        SCOPED_TRACE(vector);
        const int fd = open(path, O_RDWR | O_APPEND | O_CLOEXEC);
        ASSERT_GE(fd, 0) << strerror(errno);
        ASSERT_EQ(13, lseek(fd, 13, SEEK_SET));
        // Nonzero numeric sysctl writes are ignored. An invalid value also
        // prevents a regressed append-to-zero path from changing pid_max.
        iovec vectors[] = {{const_cast<char*>("x"), 1}, {const_cast<char*>("x"), 1}};
        EXPECT_EQ(vector ? 2 : 1, vector ? writev(fd, vectors, 2) : write(fd, "x", 1));
        EXPECT_EQ(vector ? 15 : 14, lseek(fd, 0, SEEK_CUR));
        const ssize_t readback = pread(fd, current, sizeof(current), 0);
        EXPECT_EQ(length, readback);
        EXPECT_EQ(0, memcmp(original, current, length));
        close(fd);
    }
}

TEST(ProcWritePositionPolicy, SetgroupsAppendDoesNotBypassNonzeroPosition) {
    const char* path = "/proc/self/setgroups";
    char original[32] = {}, current[32] = {};
    int reader = open(path, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(reader, 0) << strerror(errno);
    const ssize_t length = read(reader, original, sizeof(original));
    close(reader);
    ASSERT_GT(length, 0);
    ASSERT_LT(length, static_cast<ssize_t>(sizeof(original)));
    const std::string value(original, length);
    ASSERT_TRUE(value == "allow\n" || value == "deny\n");
    const int fd = open(path, O_WRONLY | O_APPEND | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    ASSERT_EQ(7, lseek(fd, 7, SEEK_SET));
    // Use the existing value: even a broken append-to-zero implementation
    // cannot disable a namespace's setgroups permission during the baseline.
    errno = 0;
    EXPECT_EQ(-1, write(fd, value.data(), value.size()));
    EXPECT_EQ(EINVAL, errno);
    EXPECT_EQ(7, lseek(fd, 0, SEEK_CUR));
    close(fd);
    reader = open(path, O_RDONLY | O_CLOEXEC);
    ASSERT_GE(reader, 0);
    const ssize_t readback = read(reader, current, sizeof(current));
    close(reader);
    EXPECT_EQ(length, readback);
    EXPECT_EQ(0, memcmp(original, current, length));
}

class UtsNoopWrite : public testing::TestWithParam<int> {};
TEST_P(UtsNoopWrite, BeyondTheStringKeepsValueAndPosition) {
    const char* path = GetParam() < 2 ? "/proc/sys/kernel/hostname" : "/proc/sys/kernel/domainname";
    const bool vector = GetParam() % 2 != 0;
    const int fd = open(path, O_RDWR | O_CLOEXEC);
    ASSERT_GE(fd, 0) << path << ": " << strerror(errno);
    char original[256] = {}, current[256] = {};
    const ssize_t length = pread(fd, original, sizeof(original), 0);
    ASSERT_GT(length, 0);
    ASSERT_LT(length, static_cast<ssize_t>(sizeof(original)));
    const off_t offset = length + 17;
    ASSERT_EQ(offset, lseek(fd, offset, SEEK_SET));
    // Both Linux's strict string handler and the DragonOS owner discard a
    // write beyond the current string. Do not write at zero or change UTS state.
    iovec vectors[] = {{const_cast<char*>("x"), 1}, {const_cast<char*>("y"), 1}};
    const ssize_t result = vector ? writev(fd, vectors, 2) : write(fd, "x", 1);
    EXPECT_EQ(vector ? 2 : 1, result) << strerror(errno);
    EXPECT_EQ(offset, lseek(fd, 0, SEEK_CUR));
    const ssize_t readback = pread(fd, current, sizeof(current), 0);
    EXPECT_EQ(length, readback);
    EXPECT_EQ(0, memcmp(original, current, length));
    close(fd);
}
INSTANTIATE_TEST_SUITE_P(OwnersAndOperations, UtsNoopWrite, testing::Values(0, 1, 2, 3));

TEST(ProcWritePositionMemory, AppendStillUsesTheRequestedMemoryAddress) {
    // /proc/pid/mem's legacy write owns its address cursor; O_APPEND must not
    // redirect this to the proc inode's synthetic zero-size EOF.
    const int fd = open("/proc/self/mem", O_RDWR | O_APPEND | O_CLOEXEC);
    ASSERT_GE(fd, 0) << strerror(errno);
    char output[8] = {};
    const off_t address = reinterpret_cast<uintptr_t>(output);
    ASSERT_EQ(address, lseek(fd, address, SEEK_SET));
    EXPECT_EQ(2, write(fd, "ab", 2));
    EXPECT_EQ(address + 2, lseek(fd, 0, SEEK_CUR));
    EXPECT_EQ(0, memcmp(output, "ab", 2));
    iovec vectors[] = {{const_cast<char*>("CD"), 2}, {const_cast<char*>("ef"), 2}};
    EXPECT_EQ(4, writev(fd, vectors, 2));
    EXPECT_EQ(address + 6, lseek(fd, 0, SEEK_CUR));
    EXPECT_EQ(0, memcmp(output, "abCDef", 6));
    EXPECT_EQ(2, pwrite(fd, "GH", 2, address + 6));
    EXPECT_EQ(address + 6, lseek(fd, 0, SEEK_CUR));
    EXPECT_EQ(0, memcmp(output, "abCDefGH", 8));
    close(fd);
}
TEST(ProcWritePositionMemory, PositionalVectorsWithAppendKeepTheRequestedMemoryAddress) {
    const int fd = open("/proc/self/mem", O_RDWR | O_APPEND | O_CLOEXEC);
    ASSERT_GE(fd, 0);
    char output[4] = {};
    const off_t address = reinterpret_cast<uintptr_t>(output);
    ASSERT_EQ(3, lseek(fd, 3, SEEK_SET));
    iovec vectors[] = {{const_cast<char*>("ab"), 2}, {const_cast<char*>("CD"), 2}};
    EXPECT_EQ(4, pwritev(fd, vectors, 2, address));
    EXPECT_EQ(0, memcmp(output, "abCD", 4));
    EXPECT_EQ(3, lseek(fd, 0, SEEK_CUR));
    memset(output, 0, sizeof(output));
    EXPECT_EQ(4, syscall(SYS_pwritev2, fd, vectors, 2, address, 0UL, 0U));
    EXPECT_EQ(0, memcmp(output, "abCD", 4));
    EXPECT_EQ(3, lseek(fd, 0, SEEK_CUR));
    close(fd);
}
}  // namespace
int main(int argc, char** argv) { testing::InitGoogleTest(&argc, argv); return RUN_ALL_TESTS(); }
