#include <gtest/gtest.h>

#include <errno.h>
#include <fcntl.h>
#include <poll.h>
#include <sched.h>
#include <signal.h>
#include <stdio.h>
#include <stdint.h>
#include <stdlib.h>
#include <string.h>
#include <sys/eventfd.h>
#include <sys/mman.h>
#include <sys/socket.h>
#include <sys/syscall.h>
#include <sys/uio.h>
#include <sys/wait.h>
#include <time.h>
#include <unistd.h>

namespace {

class Descriptor {
 public:
    explicit Descriptor(int fd) : fd_(fd) {}
    ~Descriptor() { if (fd_ >= 0) close(fd_); }
    Descriptor(const Descriptor&) = delete;
    Descriptor& operator=(const Descriptor&) = delete;
    int get() const { return fd_; }
 private:
    int fd_;
};

enum class Operation { Writev, Pwritev2, Pwritev2Hipri };
ssize_t Write(Operation operation, int fd, const iovec* vectors, int count) {
    if (operation == Operation::Writev) return writev(fd, vectors, count);
    return syscall(SYS_pwritev2, fd, vectors, count, -1L, 0UL,
                   operation == Operation::Pwritev2Hipri ? 1U : 0U);
}

class Child {
 public:
    explicit Child(pid_t pid) : pid_(pid) {}
    ~Child() {
        if (pid_ > 0) {
            kill(pid_, SIGKILL);
            int status;
            while (waitpid(pid_, &status, 0) < 0 && errno == EINTR) {}
        }
    }
    pid_t Wait(int* status) {
        timespec start{};
        if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) return -1;
        for (;;) {
            const pid_t result = waitpid(pid_, status, WNOHANG);
            if (result == pid_) {
                pid_ = -1;
                return result;
            }
            if (result < 0 && errno != EINTR) {
                if (errno == ECHILD) pid_ = -1;
                return -1;
            }
            timespec now{};
            if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return -1;
            const int64_t elapsed = (now.tv_sec - start.tv_sec) * 1000000000LL
                                  + now.tv_nsec - start.tv_nsec;
            if (elapsed >= 3000000000LL) {
                errno = ETIMEDOUT;
                return -1;
            }
            sched_yield();
        }
    }
 private:
    pid_t pid_;
};

bool WaitForSleepingChild(pid_t child) {
    timespec start{};
    if (clock_gettime(CLOCK_MONOTONIC, &start) != 0) return false;
    char path[64];
    snprintf(path, sizeof(path), "/proc/%d/stat", child);
    for (;;) {
        Descriptor stat(open(path, O_RDONLY | O_CLOEXEC));
        if (stat.get() < 0) return false;
        char data[512];
        const ssize_t count = read(stat.get(), data, sizeof(data) - 1);
        if (count <= 0) return false;
        data[count] = 0;
        const char* name_end = strrchr(data, ')');
        if (name_end && name_end[1] == ' ' && name_end[2] == 'S') return true;
        if (name_end && name_end[1] == ' ' && name_end[2] == 'Z') return false;
        timespec now{};
        if (clock_gettime(CLOCK_MONOTONIC, &now) != 0) return false;
        const int64_t elapsed = (now.tv_sec - start.tv_sec) * 1000000000LL
                              + now.tv_nsec - start.tv_nsec;
        if (elapsed >= 2000000000LL) return false;
        sched_yield();
    }
}

void CheckBlockedWriter(Operation operation, bool scalar, void* faulty_output = nullptr) {
    Descriptor fd(eventfd(0, EFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    uint64_t initial = UINT64_MAX - (scalar ? 1 : 2);
    ASSERT_EQ(8, write(fd.get(), &initial, sizeof(initial)));
    int ready[2];
    ASSERT_EQ(0, pipe2(ready, O_CLOEXEC));
    const pid_t child = fork();
    if (child < 0) {
        close(ready[0]);
        close(ready[1]);
        FAIL() << strerror(errno);
        return;
    }
    if (child == 0) {
        close(ready[0]);
        alarm(10);
        const char marker = 'r';
        if (write(ready[1], &marker, 1) != 1) _exit(10);
        close(ready[1]);
        uint64_t value = 1;
        iovec vectors[] = {{&value, 8}, {&value, 8}};
        const ssize_t result = scalar ? write(fd.get(), &value, sizeof(value))
                                      : Write(operation, fd.get(), vectors, 2);
        _exit(result == (scalar ? 8 : 16) ? 0 : 11);
    }
    close(ready[1]);
    Descriptor ready_input(ready[0]);
    Child cleanup(child);
    pollfd readable{ready_input.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&readable, 1, 2000));
    char marker = 0;
    ASSERT_EQ(1, read(ready_input.get(), &marker, 1));
    ASSERT_EQ('r', marker);
    // Read the state only after the ready handshake, so a child sleeping on
    // unrelated setup cannot be mistaken for the blocked eventfd owner.
    ASSERT_TRUE(WaitForSleepingChild(child));
    uint64_t drained = 0;
    pollfd counter_readable{fd.get(), POLLIN, 0};
    ASSERT_EQ(1, poll(&counter_readable, 1, 2000));
    if (faulty_output) {
        errno = 0;
        EXPECT_EQ(-1, read(fd.get(), faulty_output, 8));
        EXPECT_EQ(EFAULT, errno);
    } else {
        ASSERT_EQ(8, read(fd.get(), &drained, sizeof(drained)));
        EXPECT_EQ(UINT64_MAX - 1, drained);
    }
    // The parent deadline bounds the missing-wakeup regression; cleanup
    // kills and reaps the child on timeout or any earlier assertion failure.
    int status = 0;
    ASSERT_EQ(child, cleanup.Wait(&status));
    ASSERT_TRUE(WIFEXITED(status)) << "writer did not wake after counter drain: " << status;
    ASSERT_EQ(0, WEXITSTATUS(status));
    ASSERT_EQ(1, poll(&counter_readable, 1, 2000));
    ASSERT_EQ(8, read(fd.get(), &drained, sizeof(drained)));
    EXPECT_EQ(1U, drained);
}

class LegacyWritev : public testing::TestWithParam<Operation> {
 protected:
    void SetUp() override {
        page_ = sysconf(_SC_PAGESIZE);
        ASSERT_GT(page_, 0);
        mapping_ = static_cast<char*>(mmap(nullptr, 2 * page_, PROT_READ | PROT_WRITE,
                                          MAP_PRIVATE | MAP_ANONYMOUS, -1, 0));
        ASSERT_NE(MAP_FAILED, mapping_);
        ASSERT_EQ(0, mprotect(mapping_ + page_, page_, PROT_NONE));
    }
    void TearDown() override {
        if (mapping_ != MAP_FAILED) munmap(mapping_, 2 * page_);
    }
    ssize_t Call(int fd, const iovec* vectors, int count) {
        return Write(GetParam(), fd, vectors, count);
    }
    void Error(int expected, int fd, const iovec* vectors, int count) {
        errno = 0;
        EXPECT_EQ(-1, Call(fd, vectors, count));
        EXPECT_EQ(expected, errno) << strerror(errno);
    }
    void Counter(int fd, uint64_t expected) {
        uint64_t value = 0;
        ASSERT_EQ(8, read(fd, &value, sizeof(value))) << strerror(errno);
        EXPECT_EQ(expected, value);
        Empty(fd);
    }
    void Empty(int fd) {
        uint64_t value = 0;
        errno = 0;
        EXPECT_EQ(-1, read(fd, &value, sizeof(value)));
        EXPECT_EQ(EAGAIN, errno);
    }
    char* bad() const { return mapping_ + page_; }
    long page_ = 0;
    char* mapping_ = static_cast<char*>(MAP_FAILED);
};

TEST_P(LegacyWritev, EventfdCommitsEachCompleteIovec) {
    Descriptor fd(eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    uint64_t values[] = {2, 3};
    iovec vectors[] = {{&values[0], 8}, {&values[1], 8}};
    EXPECT_EQ(16, Call(fd.get(), vectors, 2));
    Counter(fd.get(), 5);
}

TEST_P(LegacyWritev, EventfdDoesNotJoinShortIovecsIntoARecord) {
    Descriptor fd(eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    uint64_t value = 2;
    iovec vectors[] = {{&value, 4}, {reinterpret_cast<char*>(&value) + 4, 4}};
    Error(EINVAL, fd.get(), vectors, 2);
    Empty(fd.get());
}

TEST_P(LegacyWritev, EventfdShortWriteStopsBeforeTheNextIovec) {
    Descriptor fd(eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    uint64_t values[] = {2, 3};
    iovec vectors[] = {{values, sizeof(values)}, {bad(), 8}};
    EXPECT_EQ(8, Call(fd.get(), vectors, 2));
    Counter(fd.get(), 2);
}

TEST_P(LegacyWritev, EventfdLaterErrorsReturnOnlyTheCommittedPrefix) {
    for (int scenario = 0; scenario < 4; ++scenario) {
        SCOPED_TRACE(scenario);
        Descriptor fd(eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC));
        ASSERT_GE(fd.get(), 0);
        uint64_t first = 2, second = UINT64_MAX;
        if (scenario == 2) {
            uint64_t initial = UINT64_MAX - 2;
            ASSERT_EQ(8, write(fd.get(), &initial, 8));
            first = second = 1;
        }
        iovec vectors[] = {{&first, 8}, {&second, 8}};
        if (scenario == 1) vectors[1].iov_len = 4;
        if (scenario == 3) vectors[1].iov_base = bad();
        EXPECT_EQ(8, Call(fd.get(), vectors, 2));
        Counter(fd.get(), scenario == 2 ? UINT64_MAX - 1 : 2);
    }
}

TEST_P(LegacyWritev, EventfdChecksRecordSizeBeforeReadingPayload) {
    Descriptor fd(eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    iovec vector = {bad(), 4};
    Error(EINVAL, fd.get(), &vector, 1);
    Empty(fd.get());
}

TEST_P(LegacyWritev, EventfdFirstRecordFaultDoesNotCommit) {
    Descriptor fd(eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    iovec vector = {bad(), 8};
    Error(EFAULT, fd.get(), &vector, 1);
    Empty(fd.get());
    uint32_t low = 2;
    memcpy(mapping_ + page_ - sizeof(low), &low, sizeof(low));
    vector = {mapping_ + page_ - sizeof(low), 8};
    Error(EFAULT, fd.get(), &vector, 1);
    Empty(fd.get());
}

TEST_P(LegacyWritev, EventfdCopiesOnlyItsEightByteRecord) {
    Descriptor fd(eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    uint64_t value = 2;
    memcpy(mapping_, &value, 8);
    iovec vectors[] = {{mapping_, static_cast<size_t>(page_) + 8}, {bad(), 8}};
    EXPECT_EQ(8, Call(fd.get(), vectors, 2));
    Counter(fd.get(), 2);
}

TEST_P(LegacyWritev, EventfdPreservesLeadingAndSkipsInteriorZeroIovecs) {
    Descriptor fd(eventfd(0, EFD_NONBLOCK | EFD_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    uint64_t values[] = {2, 3};
    iovec leading[] = {{bad(), 0}, {&values[0], 8}};
    Error(EINVAL, fd.get(), leading, 2);
    Empty(fd.get());
    iovec interior[] = {{&values[0], 8}, {bad(), 0}, {&values[1], 8}, {bad(), 0}};
    EXPECT_EQ(16, Call(fd.get(), interior, 4));
    Counter(fd.get(), 5);
    iovec empty[] = {{bad(), 0}, {bad(), 0}};
    EXPECT_EQ(0, Call(fd.get(), empty, 2));
    Empty(fd.get());
}

TEST_P(LegacyWritev, FullReturnsItsOwnerErrorWithoutCopyingPayload) {
    Descriptor fd(open("/dev/full", O_WRONLY | O_CLOEXEC));
    ASSERT_GE(fd.get(), 0);
    iovec vector = {bad(), 1};
    Error(ENOSPC, fd.get(), &vector, 1);
    iovec leading[] = {{bad(), 0}, {bad(), 1}};
    Error(ENOSPC, fd.get(), leading, 2);
    iovec empty[] = {{bad(), 0}, {bad(), 0}};
    EXPECT_EQ(0, Call(fd.get(), empty, 2));
}

TEST_P(LegacyWritev, IteratorFileStillTreatsVectorsAsOneLogicalInput) {
    char path[] = "/tmp/legacy-writev-control-XXXXXX";
    Descriptor fd(mkstemp(path));
    ASSERT_GE(fd.get(), 0);
    ASSERT_EQ(0, unlink(path));
    char first[] = "ab", second[] = "CD";
    iovec vectors[] = {{bad(), 0}, {first, 2}, {bad(), 0}, {second, 2}};
    EXPECT_EQ(4, Call(fd.get(), vectors, 4));
    EXPECT_EQ(4, lseek(fd.get(), 0, SEEK_CUR));
    char result[4];
    ASSERT_EQ(4, pread(fd.get(), result, 4, 0));
    EXPECT_EQ(0, memcmp(result, "abCD", 4));
}

TEST_P(LegacyWritev, IteratorDatagramPreservesOneRecordAndZeroIovecs) {
    int pair[2];
    ASSERT_EQ(0, socketpair(AF_UNIX, SOCK_DGRAM | SOCK_NONBLOCK | SOCK_CLOEXEC, 0, pair));
    Descriptor output(pair[0]), input(pair[1]);
    char first[] = "ab", second[] = "CD";
    iovec vectors[] = {{bad(), 0}, {first, 2}, {bad(), 0}, {second, 2}};
    EXPECT_EQ(4, Call(output.get(), vectors, 4));
    char result[8];
    ASSERT_EQ(4, read(input.get(), result, sizeof(result)));
    EXPECT_EQ(0, memcmp(result, "abCD", 4));
    // sendmsg uses the shared IoVecs gather path rather than File's primary
    // source. Retaining legacy zero metadata must preserve this caller too.
    msghdr message{};
    message.msg_iov = vectors;
    message.msg_iovlen = 4;
    EXPECT_EQ(4, sendmsg(output.get(), &message, 0));
    ASSERT_EQ(4, read(input.get(), result, sizeof(result)));
    EXPECT_EQ(0, memcmp(result, "abCD", 4));
    errno = 0;
    EXPECT_EQ(-1, read(input.get(), result, sizeof(result)));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(LegacyWritev, IteratorPipeDoesNotCommitFirstIovecBeforeLaterCopyFault) {
    int pair[2];
    ASSERT_EQ(0, pipe2(pair, O_NONBLOCK | O_CLOEXEC));
    Descriptor input(pair[0]), output(pair[1]);
    char first[] = "ab";
    iovec vectors[] = {{first, 2}, {bad(), 2}};
    Error(EFAULT, output.get(), vectors, 2);
    char result[8];
    errno = 0;
    EXPECT_EQ(-1, read(input.get(), result, sizeof(result)));
    EXPECT_EQ(EAGAIN, errno);
}

TEST_P(LegacyWritev, CounterDrainWakesTheBlockedSecondLegacyInvocation) {
    ASSERT_NO_FATAL_FAILURE(CheckBlockedWriter(GetParam(), false));
    ASSERT_NO_FATAL_FAILURE(CheckBlockedWriter(GetParam(), false, bad()));
}

TEST(LegacyWritevWakeup, CounterDrainWakesABlockedScalarWriter) {
    CheckBlockedWriter(Operation::Writev, true);
}

INSTANTIATE_TEST_SUITE_P(Operations, LegacyWritev,
                        testing::Values(Operation::Writev, Operation::Pwritev2,
                                        Operation::Pwritev2Hipri));

TEST(LegacyWritevPosition, ProcMemWritevCommitsTheAccumulatedPosition) {
    Descriptor fd(open("/proc/self/mem", O_RDWR | O_CLOEXEC));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    char destination[8] = {};
    char first[] = "ab", second[] = "CD";
    const off_t offset = reinterpret_cast<uintptr_t>(destination);
    ASSERT_EQ(offset, lseek(fd.get(), offset, SEEK_SET));
    iovec vectors[] = {{first, 2}, {second, 2}};
    EXPECT_EQ(4, writev(fd.get(), vectors, 2));
    EXPECT_EQ(0, memcmp(destination, "abCD", 4));
    EXPECT_EQ(offset + 4, lseek(fd.get(), 0, SEEK_CUR));
}

TEST(LegacyWritevPosition, ProcMemPositionalVectorsKeepTheOpenFilePosition) {
    Descriptor fd(open("/proc/self/mem", O_RDWR | O_CLOEXEC));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    char destination[8] = {};
    char first[] = "ab", second[] = "CD";
    const off_t offset = reinterpret_cast<uintptr_t>(destination);
    ASSERT_EQ(offset + 6, lseek(fd.get(), offset + 6, SEEK_SET));
    iovec vectors[] = {{first, 2}, {second, 2}};
    for (int operation = 0; operation < 3; ++operation) {
        SCOPED_TRACE(operation);
        memset(destination, 0, sizeof(destination));
        const ssize_t result = operation == 0 ? pwritev(fd.get(), vectors, 2, offset)
            : syscall(SYS_pwritev2, fd.get(), vectors, 2, offset, 0UL,
                      operation == 2 ? 1U : 0U);
        EXPECT_EQ(4, result) << strerror(errno);
        EXPECT_EQ(0, memcmp(destination, "abCD", 4));
        EXPECT_EQ(offset + 6, lseek(fd.get(), 0, SEEK_CUR));
    }
}

TEST(LegacyWritevPosition, ProcMemLaterCopyFaultKeepsTheSuccessfulPrefixAndPosition) {
    Descriptor fd(open("/proc/self/mem", O_RDWR | O_CLOEXEC));
    ASSERT_GE(fd.get(), 0) << strerror(errno);
    const long page = sysconf(_SC_PAGESIZE);
    ASSERT_GT(page, 0);
    void* bad = mmap(nullptr, page, PROT_NONE, MAP_PRIVATE | MAP_ANONYMOUS, -1, 0);
    ASSERT_NE(MAP_FAILED, bad);
    char destination[8] = {};
    char first[] = "ab";
    const off_t offset = reinterpret_cast<uintptr_t>(destination);
    iovec vectors[] = {{first, 2}, {bad, 2}};
    // A later owner fault must not erase earlier per-iovec progress, for
    // either a shared position or an explicit positional operation.
    for (int operation = 0; operation < 4; ++operation) {
        SCOPED_TRACE(operation);
        memset(destination, 0, sizeof(destination));
        ASSERT_EQ(offset, lseek(fd.get(), offset, SEEK_SET));
        const ssize_t result = operation == 0 ? writev(fd.get(), vectors, 2)
            : (operation == 1 ? pwritev(fd.get(), vectors, 2, offset)
               : syscall(SYS_pwritev2, fd.get(), vectors, 2, offset, 0UL,
                         operation == 3 ? 1U : 0U));
        EXPECT_EQ(2, result) << strerror(errno);
        EXPECT_EQ(0, memcmp(destination, "ab", 2));
        EXPECT_EQ(0, destination[2]);
        EXPECT_EQ(offset + (operation == 0 ? 2 : 0), lseek(fd.get(), 0, SEEK_CUR));
    }
    EXPECT_EQ(0, munmap(bad, page));
}

}  // namespace

int main(int argc, char** argv) {
    testing::InitGoogleTest(&argc, argv);
    return RUN_ALL_TESTS();
}
